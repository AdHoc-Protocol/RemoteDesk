// The clipboard on Windows: its text has `\r\n` lines here, `\n` for the rest of the host.
//
// One thread owns a message-only window for the life of the program: Windows tells it of every change of the clipboard, and it is
// the owner of what the page sets - so its own changes are told apart from the user's.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassW, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLIPBOARDUPDATE, WNDCLASSW,
};

const CF_UNICODETEXT: u32 = 13;
/// The window is asked to set `PENDING` on the clipboard.
const WM_SET: u32 = WM_APP + 1;

/// The window of the clipboard thread, once it is made.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The text of the page, UTF-16 with `\r\n`, waiting for the thread of the window to set it.
static PENDING: Mutex<Option<Vec<u16>>> = Mutex::new(None);
/// Who is told of a change of the clipboard.
static CHANGED: OnceLock<fn(String)> = OnceLock::new();

/// Tells `changed` of every change of the clipboard that is not of `set`: its text, with `\n` lines.
pub fn watch(changed: fn(String)) {
    let _ = CHANGED.set(changed);
    std::thread::Builder::new().name("clipboard".into()).spawn(run).expect("the thread of the clipboard");
}

/// Sets `text` ( `\n` lines ) on the clipboard of the machine, by its window: true - handed to it.
pub fn set(text: &str) -> bool {
    let mut wide = Vec::with_capacity(text.len() + text.len() / 16 + 1);
    let mut cr = false;
    for u in text.encode_utf16() {
        if u == '\n' as u16 && !cr {
            wide.push('\r' as u16);
        }
        cr = u == '\r' as u16;
        wide.push(u);
    }
    wide.push(0);
    *PENDING.lock().unwrap() = Some(wide);
    let window = WINDOW.load(Ordering::Acquire);
    if window != 0 {
        unsafe { let _ = PostMessageW(Some(HWND(window as _)), WM_SET, WPARAM(0), LPARAM(0)); }
    }
    true
}

fn run() {
    unsafe {
        let class = WNDCLASSW { lpfnWndProc: Some(window_proc), lpszClassName: w!("RemoteDeskClipboard"), ..Default::default() };
        RegisterClassW(&class);
        let window = match CreateWindowExW(WINDOW_EX_STYLE(0), w!("RemoteDeskClipboard"), w!(""), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None) {
            Ok(w) => w,
            Err(e) => return eprintln!("clipboard: no window: {e}"),
        };
        if let Err(e) = AddClipboardFormatListener(window) {
            return eprintln!("clipboard: not told of changes: {e}");
        }
        WINDOW.store(window.0 as isize, Ordering::Release);
        // a text set before the window was there waits for it: the line a just started program puts
        if PENDING.lock().unwrap().is_some() {
            let _ = PostMessageW(Some(window), WM_SET, WPARAM(0), LPARAM(0));
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_CLIPBOARDUPDATE => {
            changed(window);
            LRESULT(0)
        }
        WM_SET => {
            if let Some(text) = PENDING.lock().unwrap().take() {
                put(window, &text);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// The clipboard of the machine changed: its text is told - unless `set` set it.
fn changed(window: HWND) {
    if unsafe { GetClipboardOwner() }.is_ok_and(|owner| owner == window) {
        return;
    }
    let Some(text) = get(window) else { return };
    if let Some(changed) = CHANGED.get() {
        changed(text.replace("\r\n", "\n"));
    }
}

fn get(window: HWND) -> Option<String> {
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT).ok()?;
        open(window)?;
        let text = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let memory = HGLOBAL(handle.0);
            let at = GlobalLock(memory) as *const u16;
            if at.is_null() {
                return None;
            }
            let units = std::slice::from_raw_parts(at, GlobalSize(memory) / 2);
            let text = String::from_utf16_lossy(&units[..units.iter().position(|&u| u == 0).unwrap_or(units.len())]);
            let _ = GlobalUnlock(memory);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

/// `text` - UTF-16 ending in 0.
fn put(window: HWND, text: &[u16]) {
    unsafe {
        let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2) else { return };
        let at = GlobalLock(memory) as *mut u16;
        if at.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), at, text.len());
        let _ = GlobalUnlock(memory);
        if open(window).is_none() {
            let _ = GlobalFree(Some(memory));
            return eprintln!("clipboard: another program holds the clipboard");
        }
        // the clipboard owns the memory once it is set
        if EmptyClipboard().is_err() || SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))).is_err() {
            let _ = GlobalFree(Some(memory));
            eprintln!("clipboard: the text of the page is not set");
        }
        let _ = CloseClipboard();
    }
}

/// Another program may hold the clipboard for a moment: a few tries.
fn open(window: HWND) -> Option<()> {
    for _ in 0..10 {
        if unsafe { OpenClipboard(Some(window)) }.is_ok() {
            return Some(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    None
}
