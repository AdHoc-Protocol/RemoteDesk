// The marks of the viewers on Android: the app shows the image of each stroke in its window over the other apps ( the permission
// "Display over other apps" ), a window the touches go through; to fade, only the opacity of the image changes ( Host.mark* ).

use super::video::Display;

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
