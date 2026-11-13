// The clipboard on Linux and macOS, by arboard ( X11, Wayland, NSPasteboard ): looked at twice a second - the systems tell no
// change to a program that has no window; macOS counts the changes, the text is read after one only - and set when the page
// copied.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use arboard::Clipboard;

/// The clipboard: one for the life of the program - on X11 it serves what was set while it lives.
static BOARD: Mutex<Option<Clipboard>> = Mutex::new(None);
/// The text the clipboard has, as the host knows it: a change to another is the user's.
static LAST: Mutex<Option<String>> = Mutex::new(None);
static CHANGED: OnceLock<fn(String)> = OnceLock::new();

fn with<T>(f: impl FnOnce(&mut Clipboard) -> Option<T>) -> Option<T> {
    let mut board = BOARD.lock().unwrap();
    if board.is_none() {
        *board = Clipboard::new().map_err(|e| eprintln!("clipboard: {e}")).ok();
    }
    f(board.as_mut()?)
}

/// Tells `changed` of every change of the clipboard that is not of `set`: its text, with `\n` lines.
pub fn watch(changed: fn(String)) {
    let _ = CHANGED.set(changed);
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(|| {
            // macOS counts the changes of the pasteboard: the text is read after one only
            #[cfg(target_os = "macos")]
            let (board, mut seen) = (objc2::rc::autoreleasepool(|_| objc2_app_kit::NSPasteboard::generalPasteboard()), -1);
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if crate::session::controller().is_none() {
                    continue;
                }
                #[cfg(target_os = "macos")]
                {
                    let count = board.changeCount();
                    if count == seen {
                        continue;
                    }
                    seen = count;
                }
                let Some(mut text) = with(|b| b.get_text().ok()) else { continue };
                if text.contains('\r') {
                    text = text.replace("\r\n", "\n");
                }
                let mut last = LAST.lock().unwrap();
                if last.as_deref() == Some(text.as_str()) {
                    continue;
                }
                *last = Some(text.clone());
                drop(last);
                if let Some(changed) = CHANGED.get() {
                    changed(text);
                }
            }
        })
        .expect("the thread of the clipboard");
}

/// Sets `text` ( `\n` lines ) on the clipboard of the machine: false - there is no clipboard to set.
pub fn set(text: &str) -> bool {
    *LAST.lock().unwrap() = Some(text.to_string());
    with(|b| b.set_text(text.to_string()).ok()).is_some()
}
