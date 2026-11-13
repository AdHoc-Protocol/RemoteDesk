// The clipboard on Linux and macOS, by arboard ( X11, Wayland, NSPasteboard ): looked at twice a second - the systems tell no
// change to a program that has no window; macOS counts the changes, the text is read after one only - and set when the page
// copied. A picture is looked for when the clipboard holds no text: on X11 and Wayland every other turn, a picture is read whole.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use arboard::Clipboard;

/// The clipboard: one for the life of the program - on X11 it serves what was set while it lives.
static BOARD: Mutex<Option<Clipboard>> = Mutex::new(None);
/// The text the clipboard has, as the host knows it: a change to another is the user's.
static LAST: Mutex<Option<String>> = Mutex::new(None);
/// The picture the clipboard has, as the host knows it: a hash of its pixels.
static LAST_PICTURE: Mutex<u64> = Mutex::new(0);
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
            let mut turn = 0u32; // the turns with no text: a picture is looked for on every other
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
                let Some(mut text) = with(|b| b.get_text().ok()) else {
                    turn += 1;
                    if cfg!(target_os = "macos") || turn % 2 == 0 {
                        picture();
                    }
                    continue;
                };
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

/// A picture on the clipboard, new: PNG, to the viewer.
fn picture() {
    let Some(image) = with(|b| b.get_image().ok()) else { return };
    let mut h = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&image.bytes[..], &mut h);
    let h = std::hash::Hasher::finish(&h);
    if std::mem::replace(&mut *LAST_PICTURE.lock().unwrap(), h) == h {
        return;
    }
    if let Some(png) = crate::picture::png_of(&image.bytes, image.width as u32, image.height as u32) {
        crate::clipboard::image_changed(png);
    }
}

/// Sets a picture, PNG, on the clipboard of the machine: false - there is no clipboard, or it is no PNG.
pub fn set_image(png: &[u8]) -> bool {
    let Some((rgba, w, h)) = crate::picture::rgba_of(png) else { return false };
    let mut hash = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&rgba[..], &mut hash);
    *LAST_PICTURE.lock().unwrap() = std::hash::Hasher::finish(&hash);
    *LAST.lock().unwrap() = None;
    let image = arboard::ImageData { width: w as usize, height: h as usize, bytes: std::borrow::Cow::Owned(rgba) };
    with(|b| b.set_image(image).ok()).is_some()
}

/// Sets `text` ( `\n` lines ) on the clipboard of the machine: false - there is no clipboard to set.
pub fn set(text: &str) -> bool {
    *LAST.lock().unwrap() = Some(text.to_string());
    with(|b| b.set_text(text.to_string()).ok()).is_some()
}
