// The clipboard on Windows: its text has `\r\n` lines here, `\n` for the rest of the host. A picture is set both as "PNG" - the format
// the browsers, Office and the messengers take, with its transparency - and as CF_DIB, the one every program takes; one copied is read
// from "PNG" when a program put it, else from CF_DIB.
//
// One thread owns a message-only window for the life of the program: Windows tells it of every change of the clipboard, and it is
// the owner of what the page sets - so its own changes are told apart from the user's.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassW, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLIPBOARDUPDATE, WNDCLASSW,
};

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
/// The window is asked to set `PENDING` on the clipboard; `PICTURE`.
const WM_SET: u32 = WM_APP + 1;
const WM_SET_PICTURE: u32 = WM_APP + 2;

/// The window of the clipboard thread, once it is made.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The text of the page, UTF-16 with `\r\n`, waiting for the thread of the window to set it.
static PENDING: Mutex<Option<Vec<u16>>> = Mutex::new(None);
/// The picture of the page, PNG, waiting for the thread of the window to set it.
static PICTURE: Mutex<Option<Vec<u8>>> = Mutex::new(None);
/// Who is told of a change of the clipboard.
static CHANGED: OnceLock<fn(String)> = OnceLock::new();

/// Tells `changed` of every change of the clipboard that is not of `set`: its text, with `\n` lines.
pub fn watch(changed: fn(String)) {
    let _ = CHANGED.set(changed);
    std::thread::Builder::new().name("clipboard".into()).spawn(run).expect("the thread of the clipboard");
}

/// Sets a picture, PNG, on the clipboard of the machine, by its window: true - handed to it.
pub fn set_image(png: &[u8]) -> bool {
    *PICTURE.lock().unwrap() = Some(png.to_vec());
    let window = WINDOW.load(Ordering::Acquire);
    if window != 0 {
        unsafe { let _ = PostMessageW(Some(HWND(window as _)), WM_SET_PICTURE, WPARAM(0), LPARAM(0)); }
    }
    true
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
        WM_SET_PICTURE => {
            if let Some(png) = PICTURE.lock().unwrap().take() {
                put_picture(window, &png);
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
    if let Some(text) = get(window) {
        if let Some(changed) = CHANGED.get() {
            changed(text.replace("\r\n", "\n"));
        }
        return;
    }
    if let Some(png) = picture(window) {
        crate::clipboard::image_changed(png);
    }
}

/// The format "PNG" the programs register: the bytes of a PNG file.
fn png_format() -> u32 { unsafe { RegisterClipboardFormatW(w!("PNG")) } }

/// The bytes of a format of the clipboard, opened already.
unsafe fn bytes_of(format: u32) -> Option<Vec<u8>> {
    let handle = GetClipboardData(format).ok()?;
    let memory = HGLOBAL(handle.0);
    let at = GlobalLock(memory) as *const u8;
    if at.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(at, GlobalSize(memory)).to_vec();
    let _ = GlobalUnlock(memory);
    Some(bytes)
}

/// The picture on the clipboard, PNG: of "PNG" as a program put it, else of CF_DIB.
fn picture(window: HWND) -> Option<Vec<u8>> {
    unsafe {
        let png = png_format();
        let has_png = IsClipboardFormatAvailable(png).is_ok();
        if !has_png && IsClipboardFormatAvailable(CF_DIB).is_err() {
            return None;
        }
        open(window)?;
        let got = if has_png { bytes_of(png) } else { bytes_of(CF_DIB).and_then(|dib| png_of_dib(&dib)) };
        let _ = CloseClipboard();
        got
    }
}

/// A DIB of 24 or 32 bits a pixel as PNG.
fn png_of_dib(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |i: usize| dib.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let header = u32_at(0)? as usize;
    let width = u32_at(4)? as i32;
    let height = u32_at(8)? as i32;
    let bits = u16::from_le_bytes([*dib.get(14)?, *dib.get(15)?]) as usize;
    let compression = u32_at(16)?;
    if width <= 0 || height == 0 || !(bits == 24 || bits == 32) || !(compression == 0 || compression == 3) {
        return None;
    }
    // BI_BITFIELDS in a BITMAPINFOHEADER: its three masks come after it
    let start = header + if compression == 3 && header == 40 { 12 } else { 0 };
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let stride = (w * bits / 8 + 3) & !3;
    let pixels = dib.get(start..start + stride * h)?;
    let mut rgba = Vec::with_capacity(w * h * 4);
    let mut alpha = false;
    for y in 0..h {
        let row = if height > 0 { h - 1 - y } else { y }; // positive: from the bottom up
        for x in 0..w {
            let p = &pixels[row * stride + x * bits / 8..];
            let a = if bits == 32 { p[3] } else { 255 };
            alpha |= a != 0;
            rgba.extend_from_slice(&[p[2], p[1], p[0], a]);
        }
    }
    if !alpha {
        // 32 bits with no alpha at all: the program left it 0, the picture is opaque
        rgba.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    crate::picture::png_of(&rgba, w as u32, h as u32)
}

/// `png` on the clipboard as "PNG" and as CF_DIB.
fn put_picture(window: HWND, png: &[u8]) {
    let Some((rgba, w, h)) = crate::picture::rgba_of(png) else { return eprintln!("clipboard: the picture of the page is no PNG") };
    // CF_DIB: BITMAPINFOHEADER, then the rows from the bottom up, B G R A
    let mut dib = Vec::with_capacity(40 + rgba.len());
    for v in [40u32, w, h, 1 | 32 << 16, 0, (w * h * 4), 2835, 2835, 0, 0] {
        dib.extend_from_slice(&v.to_le_bytes());
    }
    for row in (0..h as usize).rev() {
        for p in rgba[row * w as usize * 4..(row + 1) * w as usize * 4].chunks_exact(4) {
            dib.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    unsafe {
        if open(window).is_none() {
            return eprintln!("clipboard: another program holds the clipboard");
        }
        if EmptyClipboard().is_ok() {
            for (format, bytes) in [(png_format(), png), (CF_DIB, &dib[..])] {
                let Some(memory) = global(bytes) else { continue };
                // the clipboard owns the memory once it is set
                if SetClipboardData(format, Some(HANDLE(memory.0))).is_err() {
                    let _ = GlobalFree(Some(memory));
                }
            }
        }
        let _ = CloseClipboard();
    }
}

/// `bytes` in memory the clipboard can own.
unsafe fn global(bytes: &[u8]) -> Option<HGLOBAL> {
    let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let at = GlobalLock(memory) as *mut u8;
    if at.is_null() {
        let _ = GlobalFree(Some(memory));
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
    let _ = GlobalUnlock(memory);
    Some(memory)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A DIB of 32 bits, from the bottom up, with no alpha: read from the top down, opaque.
    #[test]
    fn a_dib_is_read_as_png() {
        let mut dib = Vec::new();
        for v in [40u32, 2, 2, 1 | 32 << 16, 0, 16, 0, 0, 0, 0] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        // the bottom row: blue, green; the top row: red, white - B G R X
        dib.extend_from_slice(&[255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 255, 255, 255, 0]);
        let png = png_of_dib(&dib).unwrap();
        let (rgba, w, h) = crate::picture::rgba_of(&png).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(rgba, [255, 0, 0, 255, 255, 255, 255, 255, 0, 0, 255, 255, 0, 255, 0, 255]);
    }
}
