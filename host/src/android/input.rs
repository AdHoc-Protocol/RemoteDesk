// The input on Android: Android lets no app inject input into the others; it lets the service of accessibility of the app
// ( ../android: ControlService ) make gestures of touch, press Back, Home and Recents, and type into the field in focus. The
// touches and the mouse of the page become strokes of fingers, followed as they move; the wheel a short swipe; the keys of
// editing go to the field, a shortcut of Ctrl to its actions ( copy, cut, paste, select all ); the characters typed come as text
// ( the page sends them so to a phone: `Welcome.phone` ).
//
// Until its user turns the service on, the phone only shows its screen: the viewer is told once, the app shows the way.

use std::sync::Mutex;

use super::video::Display;
use crate::gen::__::*;
use crate::keymap::Key;

/// The finger of the mouse; the fingers of the page are 1 + their id.
const MOUSE: i32 = 0;
/// `Host.touch` phases.
const DOWN: i32 = 0;
const MOVE: i32 = 1;
const UP: i32 = 2;
const CANCEL: i32 = 3;
/// `Host.action`: AccessibilityService.GLOBAL_ACTION_BACK, HOME, RECENTS.
const BACK: i32 = 1;
const HOME: i32 = 2;
const RECENTS: i32 = 3;
/// KeyEvent.META_CTRL_ON | META_CTRL_LEFT_ON, META_SHIFT_ON | META_SHIFT_LEFT_ON.
const META_CTRL: i32 = 0x1000 | 0x2000;
const META_SHIFT: i32 = 0x1 | 0x40;

struct State {
    /// Where the mouse is, pixels of the screen; its left button is down.
    at:    (f32, f32),
    down:  bool,
    ctrl:  bool,
    shift: bool,
}

static STATE: Mutex<State> = Mutex::new(State { at: (0.0, 0.0), down: false, ctrl: false, shift: false });

/// The point of the screen of a fraction of it.
fn pixels(at: FractionalCoordinate) -> (f32, f32) {
    let (w, h) = super::video::size();
    (at.x() as f32 * (w.max(1) - 1) as f32 / 65535.0, at.y() as f32 * (h.max(1) - 1) as f32 / 65535.0)
}

/// The service of accessibility did not take it: the viewer is told once.
fn tried(done: bool) {
    static TOLD: std::sync::Once = std::sync::Once::new();
    if !done {
        TOLD.call_once(|| say!("✋", Warn, "The phone takes no input until its user lets RemoteDesk control it: the app shows the way"));
    }
}

/// The service of accessibility is on: the phone takes the input ( `Host.control` ).
static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn taken() -> bool { TAKEN.load(std::sync::atomic::Ordering::Relaxed) }

/// The user of the phone turned the control on or off: the viewers are told.
pub fn control(on: bool) {
    if TAKEN.swap(on, std::sync::atomic::Ordering::Relaxed) != on {
        crate::session::input_changed();
    }
}

pub fn set_displays(_displays: &[Display]) {}

/// A phone has neither Caps Lock nor Num Lock.
pub fn locks() -> (Option<bool>, Option<bool>) { (None, None) }

pub fn key(k: &Key, down: bool) {
    let mut s = STATE.lock().unwrap();
    match k.usb {
        0x0700e0 | 0x0700e4 => return s.ctrl = down,
        0x0700e1 | 0x0700e5 => return s.shift = down,
        _ => {}
    }
    if !down {
        return;
    }
    let meta = if s.ctrl { META_CTRL } else { 0 } | if s.shift { META_SHIFT } else { 0 };
    drop(s);
    let done = match k.usb {
        0x0c0224 | 0x070029 => super::action(BACK), // AC Back, Escape
        0x0c0223 => super::action(HOME),
        0x0c01a2 => super::action(RECENTS),
        usb => match keycode(usb) {
            Some(code) => super::press(code, meta),
            None => return,
        },
    };
    tried(done);
}

/// The KeyEvent of Android of a USB HID usage: the keys of editing, and letters and digits for the shortcuts.
fn keycode(usb: u32) -> Option<i32> {
    Some(match usb {
        0x070004..=0x07001d => 29 + (usb - 0x070004) as i32, // KEYCODE_A … Z
        0x07001e..=0x070026 => 8 + (usb - 0x07001e) as i32,  // KEYCODE_1 … 9
        0x070027 => 7,                                       // KEYCODE_0
        0x070028 | 0x070058 => 66,                           // ENTER
        0x07002a => 67,                                      // DEL: Backspace
        0x07002b => 61,                                      // TAB
        0x07002c => 62,                                      // SPACE
        0x07004a => 122,                                     // MOVE_HOME
        0x07004b => 92,                                      // PAGE_UP
        0x07004c => 112,                                     // FORWARD_DEL
        0x07004d => 123,                                     // MOVE_END
        0x07004e => 93,                                      // PAGE_DOWN
        0x07004f => 22,                                      // DPAD_RIGHT
        0x070050 => 21,                                      // DPAD_LEFT
        0x070051 => 20,                                      // DPAD_DOWN
        0x070052 => 19,                                      // DPAD_UP
        _ => return None,
    })
}

pub fn text(text: &str) { tried(super::type_text(text)); }

/// The mouse moves: a finger, while its button is down - a phone has no pointer that hovers.
pub fn move_to(at: FractionalCoordinate) {
    let p = pixels(at);
    let mut s = STATE.lock().unwrap();
    s.at = p;
    if s.down {
        drop(s);
        super::touch(MOUSE, MOVE, p.0, p.1);
    }
}

/// The left button is a finger; the right one Back, the middle one Home.
pub fn button_at(at: FractionalCoordinate, b: Viewer_MouseButton_Button, down: bool) {
    let p = pixels(at);
    STATE.lock().unwrap().at = p;
    button(b, down);
}

pub fn button(b: Viewer_MouseButton_Button, down: bool) {
    let mut s = STATE.lock().unwrap();
    match b {
        Viewer_MouseButton_Button::LEFT => {
            if s.down == down {
                return;
            }
            s.down = down;
            let at = s.at;
            drop(s);
            tried(super::touch(MOUSE, if down { DOWN } else { UP }, at.0, at.1));
        }
        Viewer_MouseButton_Button::RIGHT | Viewer_MouseButton_Button::BACK if down => {
            drop(s);
            tried(super::action(BACK));
        }
        Viewer_MouseButton_Button::MIDDLE if down => {
            drop(s);
            tried(super::action(HOME));
        }
        _ => {}
    }
}

/// A locked pointer moves nothing on a phone.
pub fn delta(_dx: i32, _dy: i32) {}

/// The wheel, notches: a short swipe where the mouse is - up, the content goes down, as a finger moves it.
pub fn wheel(x: f32, y: f32) {
    let at = STATE.lock().unwrap().at;
    tried(super::scroll(at.0, at.1, x, y));
}

pub fn touch(event: &Viewer_TouchEvent) {
    let phase = match event.kind() {
        Viewer_TouchEvent_Kind::START => DOWN,
        Viewer_TouchEvent_Kind::MOVE => MOVE,
        Viewer_TouchEvent_Kind::END => UP,
        _ => CANCEL,
    };
    for p in event.points.iter().flatten() {
        let (x, y) = pixels(p.at);
        tried(super::touch(1 + p.id() as i32, phase, x, y));
    }
}

/// The fingers down are lifted: the session ended, or the control passed.
pub fn touch_reset() {
    let mut s = STATE.lock().unwrap();
    s.down = false;
    s.ctrl = false;
    s.shift = false;
    drop(s);
    super::touch(-1, CANCEL, 0.0, 0.0);
}
