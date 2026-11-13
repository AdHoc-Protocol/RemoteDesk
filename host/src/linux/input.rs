// The input on Linux. X11: XTEST - keys by their keycodes ( evdev + 8 ), text by a spare keycode given the keysym of each
// character for a moment, the mouse in the coordinates of the screen. Wayland: the portal RemoteDesktop - keys by evdev codes,
// text by keysyms, the pointer in the coordinates of a stream, touch.

use std::sync::Mutex;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, Keycode, Keysym};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::wrapper::ConnectionExt as _;

use super::video::Display;
use super::{portal, wayland, x11};
use crate::gen::__::*;
use crate::keymap::Key;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION: u8 = 6;

/// The displays as the capture saw them last.
static DISPLAYS: Mutex<Vec<Display>> = Mutex::new(Vec::new());
/// The ticks of the wheel not yet a whole step: ( x, y ).
static WHEEL: Mutex<(f32, f32)> = Mutex::new((0.0, 0.0));
/// Touch emulated by the mouse on X11: the point that holds the button.
static TOUCH: Mutex<Option<u8>> = Mutex::new(None);

pub fn set_displays(displays: &[Display]) { *DISPLAYS.lock().unwrap() = displays.to_vec(); }

/// The display of `at` and the point on it: ( the node of its stream, x, y in pixels of the display, the display's x and y ).
fn place(at: FractionalCoordinate) -> Option<(u32, f64, f64, i32, i32)> {
    let displays = DISPLAYS.lock().unwrap();
    let d = displays.iter().find(|d| d.screen == at.screen())?;
    let fx = at.x() as f64 / 65535.0 * (d.width.max(1) - 1) as f64;
    let fy = at.y() as f64 / 65535.0 * (d.height.max(1) - 1) as f64;
    Some((d.node, fx, fy, d.x, d.y))
}

fn fake(x: &x11::X, kind: u8, detail: u8, root_x: i16, root_y: i16) {
    if x.xtest {
        let _ = x.conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, x.root, root_x, root_y, 0);
    }
}

fn flush(x: &x11::X) { let _ = x.conn.flush(); }

/// Caps Lock and Num Lock as the X server holds them; Wayland does not tell.
pub fn locks() -> (Option<bool>, Option<bool>) {
    let Some(x) = x11::x().filter(|_| !wayland()) else { return (None, None) };
    let Some(p) = x.conn.query_pointer(x.root).ok().and_then(|c| c.reply().ok()) else { return (None, None) };
    let mask = u16::from(p.mask);
    (Some(mask & 0x02 != 0), Some(mask & 0x10 != 0)) // Lock, Mod2 - Num Lock as a rule
}

pub fn key(k: &Key, down: bool) {
    if k.evdev == 0 {
        return;
    }
    if wayland() {
        if let Some(p) = portal::opened() {
            portal::key(&p, k.evdev as i32, down);
        }
    } else if let Some(x) = x11::x() {
        if k.evdev + 8 <= 255 {
            fake(x, if down { KEY_PRESS } else { KEY_RELEASE }, (k.evdev + 8) as u8, 0, 0);
            flush(x);
        }
    }
}

/// Text that has no key: each character by its keysym.
pub fn text(text: &str) {
    if wayland() {
        let Some(p) = portal::opened() else { return };
        for c in text.chars() {
            let sym = xkeysym::Keysym::from_char(c).raw() as i32;
            portal::keysym(&p, sym, true);
            portal::keysym(&p, sym, false);
        }
        return;
    }
    let Some(x) = x11::x() else { return };
    // a keycode with no keysym, given the keysym of the character for its press
    let setup = x.conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let Some(map) = x.conn.get_keyboard_mapping(min, max - min + 1).ok().and_then(|c| c.reply().ok()) else { return };
    let per = map.keysyms_per_keycode as usize;
    let Some(spare) = (0..=(max - min) as usize).rev().find(|&i| map.keysyms[i * per..][..per].iter().all(|&s| s == 0)) else { return };
    let spare = min + spare as Keycode;
    for c in text.chars() {
        let sym: Keysym = xkeysym::Keysym::from_char(c).raw();
        let _ = x.conn.change_keyboard_mapping(1, spare, 1, &[sym]);
        let _ = x.conn.sync();
        fake(x, KEY_PRESS, spare, 0, 0);
        fake(x, KEY_RELEASE, spare, 0, 0);
        let _ = x.conn.sync();
    }
    let _ = x.conn.change_keyboard_mapping(1, spare, 1, &[0]);
    flush(x);
}

pub fn move_to(at: FractionalCoordinate) {
    let Some((node, px, py, dx, dy)) = place(at) else { return };
    if wayland() {
        if let Some(p) = portal::opened() {
            portal::pointer_to(&p, node, px, py);
        }
    } else if let Some(x) = x11::x() {
        fake(x, MOTION, 0, (dx + px.round() as i32) as i16, (dy + py.round() as i32) as i16);
        flush(x);
    }
}

/// A button at a point: the pointer goes there first.
pub fn button_at(at: FractionalCoordinate, b: Viewer_MouseButton_Button, down: bool) {
    move_to(at);
    button(b, down);
}

pub fn button(b: Viewer_MouseButton_Button, down: bool) {
    // X11 buttons and the codes of evdev: left, middle, right, back, forward
    let (x_button, evdev) = match b {
        Viewer_MouseButton_Button::LEFT => (1u8, 0x110),
        Viewer_MouseButton_Button::MIDDLE => (2, 0x112),
        Viewer_MouseButton_Button::RIGHT => (3, 0x111),
        Viewer_MouseButton_Button::BACK => (8, 0x113),
        Viewer_MouseButton_Button::FORWARD => (9, 0x114),
        _ => return,
    };
    if wayland() {
        if let Some(p) = portal::opened() {
            portal::button(&p, evdev, down);
        }
    } else if let Some(x) = x11::x() {
        fake(x, if down { BUTTON_PRESS } else { BUTTON_RELEASE }, x_button, 0, 0);
        flush(x);
    }
}

/// The pointer moved by this much: the pointer is locked in the page.
pub fn delta(dx: i32, dy: i32) {
    if wayland() {
        if let Some(p) = portal::opened() {
            portal::pointer_by(&p, dx as f64, dy as f64);
        }
    } else if let Some(x) = x11::x() {
        fake(x, MOTION, 1, dx as i16, dy as i16); // detail 1: relative
        flush(x);
    }
}

/// Ticks of the wheel, positive - away from the user ( up, right ): whole steps go out, the rest waits for more.
pub fn wheel(x_ticks: f32, y_ticks: f32) {
    let (sx, sy) = {
        let mut w = WHEEL.lock().unwrap();
        w.0 += x_ticks;
        w.1 += y_ticks;
        let (sx, sy) = (w.0.trunc() as i32, w.1.trunc() as i32);
        w.0 -= sx as f32;
        w.1 -= sy as f32;
        (sx, sy)
    };
    if sx == 0 && sy == 0 {
        return;
    }
    if wayland() {
        let Some(p) = portal::opened() else { return };
        // libinput counts down and right as positive
        if sy != 0 {
            portal::wheel(&p, true, -sy);
        }
        if sx != 0 {
            portal::wheel(&p, false, sx);
        }
    } else if let Some(x) = x11::x() {
        // buttons 4 and 5 - up and down, 6 and 7 - left and right
        for (steps, positive, negative) in [(sy, 4u8, 5u8), (sx, 7, 6)] {
            let b = if steps > 0 { positive } else { negative };
            for _ in 0..steps.unsigned_abs() {
                fake(x, BUTTON_PRESS, b, 0, 0);
                fake(x, BUTTON_RELEASE, b, 0, 0);
            }
        }
        flush(x);
    }
}

pub fn touch(event: &Viewer_TouchEvent) {
    let kind = event.kind();
    for p in event.points.iter().flatten() {
        if wayland() {
            let Some(portal) = portal::opened() else { return };
            let Some((node, px, py, _, _)) = place(p.at) else { continue };
            let slot = p.id() as u32;
            match kind {
                Viewer_TouchEvent_Kind::START => portal::touch_down(&portal, node, slot, px, py),
                Viewer_TouchEvent_Kind::MOVE => portal::touch_motion(&portal, node, slot, px, py),
                _ => portal::touch_up(&portal, slot),
            }
            continue;
        }
        // X11 has no touch of XTEST: the first point is the mouse
        let mut held = TOUCH.lock().unwrap();
        match kind {
            Viewer_TouchEvent_Kind::START if held.is_none() => {
                *held = Some(p.id());
                move_to(p.at);
                button(Viewer_MouseButton_Button::LEFT, true);
            }
            Viewer_TouchEvent_Kind::MOVE if *held == Some(p.id()) => move_to(p.at),
            Viewer_TouchEvent_Kind::END | Viewer_TouchEvent_Kind::CANCEL if *held == Some(p.id()) => {
                *held = None;
                move_to(p.at);
                button(Viewer_MouseButton_Button::LEFT, false);
            }
            _ => {}
        }
    }
}

/// The touches that are down are forgotten: the session ended.
pub fn touch_reset() {
    if TOUCH.lock().unwrap().take().is_some() {
        button(Viewer_MouseButton_Button::LEFT, false);
    }
}
