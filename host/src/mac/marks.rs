// The marks of the viewers on macOS: a borderless window of each stroke, clear but for its image, above the windows of every space
// ( the level of the screen saver ), that the mouse goes through. AppKit wants its windows on the main thread: the windows live in a
// table there, by number; the layer of the thread of the marks holds the number. To fade, only the opacity of the window changes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use objc2::rc::Retained;
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSColor, NSImage, NSImageView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask};
use objc2_core_graphics::{CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow, CGBitmapContextGetData, CGColorSpace, CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::video::Display;

/// How large the marks are on a display of `size`: one of 1080 lines is 1.
pub fn scale(size: (u32, u32)) -> f32 { (size.0.min(size.1) as f32 / 1080.0).max(1.0) }

/// The order of the bytes of an image: B, G, R, A - premultiplied, first in a little-endian word.
pub const RGBA: bool = false;
/// kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little.
const BGRA: u32 = 2 | (2 << 12);
/// kCGScreenSaverWindowLevel: above the windows of the apps, the menu bar and the Dock.
const LEVEL: isize = 1000;

thread_local! {
    /// The windows of the layers, on the main thread.
    static WINDOWS: RefCell<HashMap<usize, Retained<NSWindow>>> = RefCell::new(HashMap::new());
}

static NEXT: AtomicUsize = AtomicUsize::new(1);

/// The window of one stroke: its number in `WINDOWS`.
pub struct Layer {
    id: usize,
}

impl Layer {
    pub fn new() -> Result<Self, String> {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let made = super::on_main(move || {
            let Some(mtm) = MainThreadMarker::new() else { return false };
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
            let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false) };
            unsafe { window.setReleasedWhenClosed(false) };
            window.setOpaque(false);
            window.setBackgroundColor(Some(&NSColor::clearColor()));
            window.setIgnoresMouseEvents(true);
            window.setHasShadow(false);
            window.setLevel(LEVEL);
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::Stationary | NSWindowCollectionBehavior::FullScreenAuxiliary | NSWindowCollectionBehavior::IgnoresCycle,
            );
            WINDOWS.with(|w| w.borrow_mut().insert(id, window));
            true
        });
        if made == Some(true) { Ok(Self { id }) } else { Err(String::from("AppKit made no window over the screen")) }
    }

    /// Shows `pixels`, `width` × `height`, at `x`, `y` of `display` ( pixels ), of opacity `alpha`.
    pub fn show(&mut self, display: &Display, x: i32, y: i32, width: u32, height: u32, pixels: &[u8], alpha: f32) {
        let id = self.id;
        let data = pixels.to_vec();
        let bounds = display.bounds;
        let scale = if bounds.size.width > 0.0 { display.width as f64 / bounds.size.width } else { 1.0 };
        let _ = super::on_main(move || {
            let Some(mtm) = MainThreadMarker::new() else { return };
            // the memory of the context is its own: the image may share it after the context is let go, never ours
            let space = CGColorSpace::new_device_rgb();
            let Some(context) = (unsafe { CGBitmapContextCreate(std::ptr::null_mut(), width as usize, height as usize, 8, width as usize * 4, space.as_deref(), BGRA) }) else { return };
            let into = CGBitmapContextGetData(Some(&context)) as *mut u8;
            if into.is_null() || CGBitmapContextGetBytesPerRow(Some(&context)) != width as usize * 4 {
                return;
            }
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), into, data.len().min((width * height * 4) as usize)) };
            let Some(image) = CGBitmapContextCreateImage(Some(&context)) else { return };
            // points, and the coordinates of Cocoa: from the bottom of the main display up
            let size = NSSize::new(width as f64 / scale, height as f64 / scale);
            let left = bounds.origin.x + x as f64 / scale;
            let top = bounds.origin.y + y as f64 / scale;
            let main = CGDisplayBounds(CGMainDisplayID());
            let frame = NSRect::new(NSPoint::new(left, main.size.height - top - size.height), size);
            let picture = NSImage::initWithCGImage_size(NSImage::alloc(), &image, size);
            let view = NSImageView::imageViewWithImage(&picture, mtm);
            WINDOWS.with(|w| {
                if let Some(window) = w.borrow().get(&id) {
                    window.setContentView(Some(&view));
                    window.setFrame_display(frame, true);
                    window.setAlphaValue(alpha as f64);
                    window.orderFrontRegardless();
                }
            });
        });
    }

    /// The opacity of what it shows.
    pub fn fade(&mut self, alpha: f32) {
        let id = self.id;
        let _ = super::on_main(move || {
            WINDOWS.with(|w| {
                if let Some(window) = w.borrow().get(&id) {
                    window.setAlphaValue(alpha as f64);
                }
            })
        });
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        let id = self.id;
        let _ = super::on_main(move || {
            if let Some(window) = WINDOWS.with(|w| w.borrow_mut().remove(&id)) {
                window.close();
            }
        });
    }
}

/// AppKit serves the windows on the main thread itself.
pub fn pump() {}
