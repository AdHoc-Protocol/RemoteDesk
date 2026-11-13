// The input on macOS: CGEvent posted to the HID level - keys by their virtual key codes ( the layout is the machine's ), text as
// Unicode, the mouse in the global coordinates of the displays ( points ). macOS has no synthetic touch: the first point of a touch
// is the mouse. The modifiers held are put on every event: macOS does not take them from the key events before.

use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFData, CFRetained, CGPoint};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit};

use super::video::Display;
use crate::gen::__::*;
use crate::keymap::Key;

/// The displays as the capture saw them last.
static DISPLAYS: Mutex<Vec<Display>> = Mutex::new(Vec::new());

struct State {
    /// The modifiers the page holds.
    flags:   u64,
    /// The buttons held: left, right, other.
    buttons: [bool; 3],
    /// Where the pointer is, in points.
    at:      CGPoint,
    /// The last press: when, where, which button, how many in a row - a double click is two presses close in time and place.
    click:   (Option<Instant>, CGPoint, u32, i64),
    wheel:   (f32, f32),
    touch:   Option<u8>,
}

static STATE: Mutex<State> = Mutex::new(State { flags: 0, buttons: [false; 3], at: CGPoint { x: 0.0, y: 0.0 }, click: (None, CGPoint { x: 0.0, y: 0.0 }, 0, 0), wheel: (0.0, 0.0), touch: None });

pub fn set_displays(displays: &[Display]) { *DISPLAYS.lock().unwrap() = displays.to_vec(); }

fn post(event: Option<CFRetained<CGEvent>>, flags: u64) {
    if let Some(e) = event {
        CGEvent::set_flags(Some(&e), CGEventFlags(flags));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
    }
}

/// The flag of a modifier key, by its virtual key code.
fn modifier(code: u16) -> u64 {
    match code {
        56 | 60 => CGEventFlags::MaskShift.0,
        59 | 62 => CGEventFlags::MaskControl.0,
        58 | 61 => CGEventFlags::MaskAlternate.0,
        55 | 54 => CGEventFlags::MaskCommand.0,
        _ => 0,
    }
}

/// Caps Lock as the machine holds it; the Mac has no Num Lock.
pub fn locks() -> (Option<bool>, Option<bool>) {
    let flags = CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState);
    (Some(flags.0 & CGEventFlags::MaskAlphaShift.0 != 0), None)
}

pub fn key(k: &Key, down: bool) {
    if k.mac == 0xffff {
        return;
    }
    let flags = {
        let mut s = STATE.lock().unwrap();
        let m = modifier(k.mac);
        if down {
            s.flags |= m;
        } else {
            s.flags &= !m;
        }
        s.flags
    };
    post(CGEvent::new_keyboard_event(None, k.mac, down), flags);
}

/// Text, as Unicode: what a soft keyboard or an input method typed.
pub fn text(text: &str) {
    let units: Vec<u16> = text.encode_utf16().collect();
    // a keyboard event carries 20 units at most
    for piece in units.chunks(20) {
        for down in [true, false] {
            if let Some(e) = CGEvent::new_keyboard_event(None, 0, down) {
                unsafe { CGEvent::keyboard_set_unicode_string(Some(&e), piece.len() as _, piece.as_ptr()) };
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
            }
        }
    }
}

/// A point of a display in the global coordinates of macOS, points.
fn point(at: FractionalCoordinate) -> Option<CGPoint> {
    let displays = DISPLAYS.lock().unwrap();
    let d = displays.iter().find(|d| d.screen == at.screen())?;
    let b = d.bounds;
    Some(CGPoint { x: b.origin.x + at.x() as f64 / 65535.0 * (b.size.width - 1.0).max(0.0), y: b.origin.y + at.y() as f64 / 65535.0 * (b.size.height - 1.0).max(0.0) })
}

/// The kind of a move: dragged while a button is held.
fn moved(s: &State) -> (CGEventType, CGMouseButton) {
    if s.buttons[0] {
        (CGEventType::LeftMouseDragged, CGMouseButton::Left)
    } else if s.buttons[1] {
        (CGEventType::RightMouseDragged, CGMouseButton::Right)
    } else if s.buttons[2] {
        (CGEventType::OtherMouseDragged, CGMouseButton::Center)
    } else {
        (CGEventType::MouseMoved, CGMouseButton::Left)
    }
}

pub fn move_to(at: FractionalCoordinate) {
    let Some(p) = point(at) else { return };
    let mut s = STATE.lock().unwrap();
    s.at = p;
    let (kind, button) = moved(&s);
    post(CGEvent::new_mouse_event(None, kind, p, button), s.flags);
}

/// A button at a point: the pointer goes there first.
pub fn button_at(at: FractionalCoordinate, b: Viewer_MouseButton_Button, down: bool) {
    move_to(at);
    button(b, down);
}

pub fn button(b: Viewer_MouseButton_Button, down: bool) {
    // the slot of the button held, the button of CoreGraphics, its number for the other buttons
    let (slot, button, number) = match b {
        Viewer_MouseButton_Button::LEFT => (0, CGMouseButton::Left, 0),
        Viewer_MouseButton_Button::RIGHT => (1, CGMouseButton::Right, 1),
        Viewer_MouseButton_Button::MIDDLE => (2, CGMouseButton::Center, 2),
        Viewer_MouseButton_Button::BACK => (2, CGMouseButton::Center, 3),
        Viewer_MouseButton_Button::FORWARD => (2, CGMouseButton::Center, 4),
        _ => return,
    };
    let kind = match (slot, down) {
        (0, true) => CGEventType::LeftMouseDown,
        (0, false) => CGEventType::LeftMouseUp,
        (1, true) => CGEventType::RightMouseDown,
        (1, false) => CGEventType::RightMouseUp,
        (_, true) => CGEventType::OtherMouseDown,
        (_, false) => CGEventType::OtherMouseUp,
    };
    let mut s = STATE.lock().unwrap();
    s.buttons[slot] = down;
    if down {
        let (last, at, which, count) = s.click;
        let near = (at.x - s.at.x).abs() < 4.0 && (at.y - s.at.y).abs() < 4.0;
        let soon = last.is_some_and(|t| t.elapsed() < Duration::from_millis(500));
        let count = if near && soon && which == number { count + 1 } else { 1 };
        s.click = (Some(Instant::now()), s.at, number, count);
    }
    let Some(e) = CGEvent::new_mouse_event(None, kind, s.at, button) else { return };
    CGEvent::set_integer_value_field(Some(&e), CGEventField::MouseEventClickState, s.click.3);
    CGEvent::set_integer_value_field(Some(&e), CGEventField::MouseEventButtonNumber, number as i64);
    post(Some(e), s.flags);
}

/// The pointer moved by this much: the pointer is locked in the page.
pub fn delta(dx: i32, dy: i32) {
    let mut s = STATE.lock().unwrap();
    s.at = CGPoint { x: s.at.x + dx as f64, y: s.at.y + dy as f64 };
    let (kind, button) = moved(&s);
    let Some(e) = CGEvent::new_mouse_event(None, kind, s.at, button) else { return };
    CGEvent::set_integer_value_field(Some(&e), CGEventField::MouseEventDeltaX, dx as i64);
    CGEvent::set_integer_value_field(Some(&e), CGEventField::MouseEventDeltaY, dy as i64);
    post(Some(e), s.flags);
}

/// Ticks of the wheel, positive - away from the user ( up, right ): whole lines go out, the rest waits for more.
pub fn wheel(x: f32, y: f32) {
    let mut s = STATE.lock().unwrap();
    s.wheel.0 += x;
    s.wheel.1 += y;
    let (lx, ly) = (s.wheel.0.trunc() as i32, s.wheel.1.trunc() as i32);
    s.wheel.0 -= lx as f32;
    s.wheel.1 -= ly as f32;
    if lx == 0 && ly == 0 {
        return;
    }
    // positive wheel 1 scrolls up, positive wheel 2 scrolls left
    post(CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 2, ly, -lx, 0), s.flags);
}

pub fn touch(event: &Viewer_TouchEvent) {
    let kind = event.kind();
    for p in event.points.iter().flatten() {
        let held = STATE.lock().unwrap().touch;
        match kind {
            Viewer_TouchEvent_Kind::START if held.is_none() => {
                STATE.lock().unwrap().touch = Some(p.id());
                move_to(p.at);
                button(Viewer_MouseButton_Button::LEFT, true);
            }
            Viewer_TouchEvent_Kind::MOVE if held == Some(p.id()) => move_to(p.at),
            Viewer_TouchEvent_Kind::END | Viewer_TouchEvent_Kind::CANCEL if held == Some(p.id()) => {
                STATE.lock().unwrap().touch = None;
                move_to(p.at);
                button(Viewer_MouseButton_Button::LEFT, false);
            }
            _ => {}
        }
    }
}

/// The touches that are down are forgotten: the session ended.
pub fn touch_reset() {
    let held = STATE.lock().unwrap().touch.take();
    if held.is_some() {
        button(Viewer_MouseButton_Button::LEFT, false);
    }
}
