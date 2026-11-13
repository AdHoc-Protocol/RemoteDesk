// The input of the page, injected on the machine: keys by their place on the keyboard ( the layout is the machine's ), text, the
// mouse, touch. How it is injected is the system's: `os::input`.
//
// Only the input of the viewer that controls the machine is taken. What is pressed is remembered: when the control passes, or the viewer
// that controls leaves, it is released - a lost connection must not leave Ctrl held down.

use std::sync::Mutex;

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};
use crate::keymap;
use crate::os::input as os;
use crate::session::controls;

const CAPS_LOCK: u32 = 0x070039;
const NUM_LOCK: u32 = 0x070053;

/// What the page holds down now: keys by USB HID usage, mouse buttons.
static HELD: Mutex<(Vec<u32>, Vec<Viewer_MouseButton_Button>)> = Mutex::new((Vec::new(), Vec::new()));

pub fn register() {
    actor0::working::on_receive_d::Viewer_KeyEvent.add(on_key);
    actor0::working::on_receive_d::Viewer_TextEvent.add(on_text);
    actor0::working::on_receive_d::Viewer_MouseMove.add(on_move);
    actor0::working::on_receive_d::Viewer_MouseButton.add(on_button);
    actor0::working::on_receive_d::Viewer_MouseDelta.add(on_delta);
    actor0::working::on_receive_d::Viewer_MouseWheel.add(on_wheel);
    actor0::working::on_receive_d::Viewer_TouchEvent.add(on_touch);
}

/// The control passed, or the viewer that controls left: what it held down goes up.
pub fn release_all() {
    let (keys, buttons) = std::mem::take(&mut *HELD.lock().unwrap());
    for usb in keys {
        if let Some(k) = keymap::key(usb) {
            os::key(k, false);
        }
    }
    for b in buttons {
        os::button(b, false);
    }
    os::touch_reset();
}

fn on_key(event: Viewer_KeyEvent, conn: &Connection, _: &Actor0) {
    if !controls(conn) {
        return;
    }
    let usb = event.usb_keycode();
    let Some(k) = keymap::key(usb) else { return };
    if event.pressed() && (event.caps_lock_state_hasValue() || event.num_lock_state_hasValue()) {
        // the lock states the page knows for sure, before the key: a key of a lock itself sets its own
        let (caps, num) = os::locks();
        let toggle = |lock: u32| {
            if let Some(l) = keymap::key(lock) {
                os::key(l, true);
                os::key(l, false);
            }
        };
        if event.caps_lock_state_hasValue() && usb != CAPS_LOCK && caps.is_some_and(|c| c != event.caps_lock_state()) {
            toggle(CAPS_LOCK);
        }
        if event.num_lock_state_hasValue() && usb != NUM_LOCK && num.is_some_and(|n| n != event.num_lock_state()) {
            toggle(NUM_LOCK);
        }
    }
    os::key(k, event.pressed());
    let mut held = HELD.lock().unwrap();
    held.0.retain(|&u| u != usb);
    if event.pressed() {
        held.0.push(usb);
    }
}

fn on_text(event: &mut Viewer_TextEvent, conn: &Connection, _: &Actor0) {
    if !controls(conn) {
        return;
    }
    if let Some(text) = &event.text {
        os::text(text);
    }
}

fn on_move(event: Viewer_MouseMove, conn: &Connection, _: &Actor0) {
    if !controls(conn) {
        return;
    }
    os::move_to(event.at());
}

fn on_button(event: Viewer_MouseButton, conn: &Connection, _: &Actor0) {
    if !controls(conn) {
        return;
    }
    let b = event.button();
    os::button_at(event.at(), b, event.down());
    let mut held = HELD.lock().unwrap();
    held.1.retain(|&x| x != b);
    if event.down() {
        held.1.push(b);
    }
}

fn on_delta(event: Viewer_MouseDelta, conn: &Connection, _: &Actor0) {
    if !controls(conn) {
        return;
    }
    os::delta(event.dx() as i32, event.dy() as i32);
}

/// The wheel in 120ths of a notch: positive - away from the user ( up, right ); less than 120 - a fine wheel.
fn on_wheel(event: Viewer_MouseWheel, conn: &Connection, _: &Actor0) {
    if !controls(conn) {
        return;
    }
    os::wheel(event.x() as f32 / 120.0, event.y() as f32 / 120.0);
}

fn on_touch(event: &mut Viewer_TouchEvent, conn: &Connection, _: &Actor0) {
    if !controls(conn) {
        return;
    }
    os::touch(event);
}
