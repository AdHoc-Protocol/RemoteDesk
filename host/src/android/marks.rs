// The marks of the viewers on Android: the app shows the image of each stroke in its window over the other apps ( the permission
// "Display over other apps" ), a window the touches go through; to fade, only the opacity of the image changes ( Host.mark* ).

use std::sync::atomic::{AtomicU32, Ordering};

use super::video::Display;

/// The density of the screen, dots an inch, as the app told it.
pub static DPI: AtomicU32 = AtomicU32::new(0);

/// How large the marks are: by the density of the screen, as Android sizes what it draws ( a dp is a pixel of density 160 ) - on a
/// phone of 480 dots an inch three times the pixels; by the size, when the density is not told.
pub fn scale(size: (u32, u32)) -> f32 {
    match DPI.load(Ordering::Relaxed) {
        0 => (size.0.min(size.1) as f32 / 1080.0).max(1.0),
        dpi => dpi as f32 / 160.0, // a pixel of density 160: what Android calls a dp
    }
}

/// The order of the bytes of an image: R, G, B, A - premultiplied, as a Bitmap of Android holds it.
pub const RGBA: bool = true;

/// The image of one stroke in the window of the app: its number there.
pub struct Layer {
    id: i32,
}

impl Layer {
    pub fn new() -> Result<Self, String> {
        match super::mark_new() {
            Some(id) if id >= 0 => Ok(Self { id }),
            _ => Err(String::from("allow RemoteDesk to display over other apps: the app shows the way")),
        }
    }

    /// Shows `pixels`, `width` × `height`, at `x`, `y` of the screen, of opacity `alpha`.
    pub fn show(&mut self, _display: &Display, x: i32, y: i32, width: u32, height: u32, pixels: &[u8], alpha: f32) {
        super::mark_show(self.id, x, y, width, height, pixels, alpha);
    }

    pub fn fade(&mut self, alpha: f32) { super::mark_fade(self.id, alpha); }
}

impl Drop for Layer {
    fn drop(&mut self) { super::mark_gone(self.id); }
}

/// The app shows them on its main thread.
pub fn pump() {}
