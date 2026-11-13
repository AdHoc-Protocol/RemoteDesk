// The input on Android: Android lets no app inject input into the others. What it lets is the service of accessibility of the app -
// gestures of touch, the buttons Back, Home, Recents, text into the field in focus - and that comes in the next step. Until then
// the viewer watches: its input is let go here, and its user is told once.
//
// TODO(port): the service of accessibility of ../android - `touch`, `button_at`, `move_to` as gestures; `text` and `key` into the
// field in focus ( AccessibilityService.InputMethod, Android 13 ); Back, Home, Recents as its global actions.

use std::sync::Once;

use super::video::Display;
use crate::gen::__::*;
use crate::keymap::Key;

static TOLD: Once = Once::new();

/// Says once that the phone does not take the input yet.
fn not_yet() {
    TOLD.call_once(|| say!("✋", Warn, "This phone does not take the input of the viewer yet: it only shows its screen"));
}

pub fn set_displays(_displays: &[Display]) {}

/// Caps Lock and Num Lock: a phone has neither.
pub fn locks() -> (Option<bool>, Option<bool>) { (None, None) }

pub fn key(_k: &Key, _down: bool) { not_yet(); }

pub fn text(_text: &str) { not_yet(); }

pub fn move_to(_at: FractionalCoordinate) {}

pub fn button_at(_at: FractionalCoordinate, _b: Viewer_MouseButton_Button, down: bool) {
    if down {
        not_yet();
    }
}

pub fn button(_b: Viewer_MouseButton_Button, _down: bool) {}

pub fn delta(_dx: i32, _dy: i32) {}

pub fn wheel(_x: f32, _y: f32) { not_yet(); }

pub fn touch(_event: &Viewer_TouchEvent) { not_yet(); }

pub fn touch_reset() {}
