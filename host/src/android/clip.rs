// The clipboard on Android. Text and pictures from the viewer are set on the clipboard of the phone ( `set`, `set_image` ): its user
// pastes them where they like, or the viewer does, with Ctrl+V.
// Android lets an app read the clipboard only while it is in front, so nothing watches it: text goes to the computer when the user of
// the phone sends it - selected in any app and sent to RemoteDesk, shared to it, or the clipboard sent by the button of its
// notification ( `sent` ).

use std::sync::OnceLock;

static CHANGED: OnceLock<fn(String)> = OnceLock::new();

/// Keeps `changed`: it gets the text the user of the phone sends.
pub fn watch(changed: fn(String)) { let _ = CHANGED.set(changed); }

/// Text the user of the phone sent: to the viewer that controls; told when there is none.
pub fn sent(text: String) {
    if text.is_empty() {
        return;
    }
    if crate::session::controller().is_none() {
        return say!("📋", Warn, "No viewer is connected to take the text");
    }
    if let Some(changed) = CHANGED.get() {
        changed(text.replace("\r\n", "\n"));
        say!("📋", Good, "The text went to the computer: paste it there");
    }
}

/// A picture the user of the phone sent: PNG, to the viewer that controls.
pub fn sent_image(png: Vec<u8>) {
    if png.is_empty() {
        return;
    }
    if crate::session::controller().is_none() {
        return say!("📋", Warn, "No viewer is connected to take the picture");
    }
    crate::clipboard::image_changed(png);
    say!("🖼️", Good, "The picture went to the computer: paste it there");
}

/// Sets a picture, PNG, on the clipboard of the phone.
pub fn set_image(png: &[u8]) -> bool { super::set_clip_image(png) }

/// Sets `text` on the clipboard of the phone.
pub fn set(text: &str) -> bool { super::set_clip(text) }
