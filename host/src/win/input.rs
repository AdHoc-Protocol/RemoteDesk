// The input on Windows: SendInput - keys by their scan codes ( the layout is the machine's ), text as Unicode, the mouse in the
// coordinates of the whole virtual screen; touch by a synthetic pointer device. The keyboard layout of the window in front.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Controls::{CreateSyntheticPointerDevice, HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE,
    POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PT_TOUCH, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

use crate::gen::__::*;
use crate::keymap::Key;
use super::video::Display;

/// The displays as the capture saw them last, in pixels of the virtual screen.
struct Layout {
    /// ( screen, x, y, width, height ): `count` of them.
    displays: [(u8, i32, i32, u32, u32); 16],
    count:    usize,
    /// The virtual screen: left, top, width - 1 and height - 1, at least 1.
    screen:   (i32, i32, i64, i64),
}

static LAYOUT: Mutex<Layout> = Mutex::new(Layout { displays: [(0, 0, 0, 0, 0); 16], count: 0, screen: (0, 0, 1, 1) });

/// The capture enumerated the displays: the fractional coordinates of the page are of these.
pub fn set_displays(displays: &[Display]) {
    let mut layout = LAYOUT.lock().unwrap();
    layout.count = displays.len().min(16);
    for (slot, d) in layout.displays.iter_mut().zip(displays) {
        *slot = (d.screen, d.x, d.y, d.width, d.height);
    }
    layout.screen = unsafe {
        let (w, h) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), (w as i64 - 1).max(1), (h as i64 - 1).max(1))
    };
}

impl Layout {
    /// A point of a display in pixels of the virtual screen.
    fn pixel(&self, at: FractionalCoordinate) -> Option<(i32, i32)> {
        let &(_, x, y, w, h) = self.displays[..self.count].iter().find(|d| d.0 == at.screen())?;
        Some((x + (at.x() as i64 * (w.max(1) - 1) as i64 / 65535) as i32, y + (at.y() as i64 * (h.max(1) - 1) as i64 / 65535) as i32))
    }
}

/// The touches that are down are forgotten: the session ended.
pub fn touch_reset() { TOUCH.lock().unwrap().contacts.clear(); }

// ── keys ──

/// The states of Caps Lock and Num Lock.
pub fn locks() -> (Option<bool>, Option<bool>) { (Some(locked(VK_CAPITAL)), Some(locked(VK_NUMLOCK))) }

/// A key down or up, by its scan code.
pub fn key(k: &Key, down: bool) {
    if k.win != 0 {
        scan_key(k.win, down);
    }
}

fn locked(vk: VIRTUAL_KEY) -> bool { unsafe { GetKeyState(vk.0 as i32) & 1 != 0 } }

fn scan_key(scan: u16, down: bool) {
    let mut flags = KEYEVENTF_SCANCODE;
    if scan & 0xff00 == 0xe000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    send(&[keyboard(0, scan & 0xff, flags)]);
}

/// Text, as Unicode: what a soft keyboard or an input method typed.
pub fn text(text: &str) {
    let mut inputs = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        inputs.push(keyboard(0, unit, KEYEVENTF_UNICODE));
        inputs.push(keyboard(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    send(&inputs);
}

fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type:    INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

// ── the mouse ──

/// The pointer to a point of a display.
pub fn move_to(at: FractionalCoordinate) {
    if let Some(input) = absolute(at, MOUSEEVENTF_MOVE, 0) {
        send(&[input]);
    }
}

/// A button at a point: one input moves the pointer there, then presses or lets go.
pub fn button_at(at: FractionalCoordinate, b: Viewer_MouseButton_Button, down: bool) {
    let Some((flags, data)) = button_flags(b, down) else { return move_to(at) };
    if let Some(input) = absolute(at, MOUSEEVENTF_MOVE | flags, data) {
        send(&[input]);
    }
}

pub fn button(b: Viewer_MouseButton_Button, down: bool) {
    if let Some((flags, data)) = button_flags(b, down) {
        send(&[mouse(0, 0, data, flags)]);
    }
}

fn button_flags(b: Viewer_MouseButton_Button, down: bool) -> Option<(MOUSE_EVENT_FLAGS, i32)> {
    Some(match (b, down) {
        (Viewer_MouseButton_Button::LEFT, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (Viewer_MouseButton_Button::LEFT, false) => (MOUSEEVENTF_LEFTUP, 0),
        (Viewer_MouseButton_Button::RIGHT, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (Viewer_MouseButton_Button::RIGHT, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (Viewer_MouseButton_Button::MIDDLE, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (Viewer_MouseButton_Button::MIDDLE, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (Viewer_MouseButton_Button::BACK, true) => (MOUSEEVENTF_XDOWN, 1),    // XBUTTON1
        (Viewer_MouseButton_Button::BACK, false) => (MOUSEEVENTF_XUP, 1),
        (Viewer_MouseButton_Button::FORWARD, true) => (MOUSEEVENTF_XDOWN, 2), // XBUTTON2
        (Viewer_MouseButton_Button::FORWARD, false) => (MOUSEEVENTF_XUP, 2),
        _ => return None,
    })
}

/// The pointer moved by this much: the pointer is locked in the page.
pub fn delta(dx: i32, dy: i32) { send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)]); }

/// Ticks of the wheel, positive - away from the user ( up, right ): 120 a notch on Windows.
pub fn wheel(x: f32, y: f32) {
    let mut inputs = [mouse(0, 0, 0, MOUSEEVENTF_WHEEL); 2];
    let mut n = 0;
    if y != 0.0 {
        inputs[n] = mouse(0, 0, (y * 120.0).round() as i32, MOUSEEVENTF_WHEEL);
        n += 1;
    }
    if x != 0.0 {
        inputs[n] = mouse(0, 0, (x * 120.0).round() as i32, MOUSEEVENTF_HWHEEL);
        n += 1;
    }
    send(&inputs[..n]);
}

/// A mouse input at a point, in the 0 … 65535 of the whole virtual screen.
fn absolute(at: FractionalCoordinate, flags: MOUSE_EVENT_FLAGS, data: i32) -> Option<INPUT> {
    let layout = LAYOUT.lock().unwrap();
    let (px, py) = layout.pixel(at)?;
    let (vx, vy, vw1, vh1) = layout.screen;
    let nx = ((px - vx) as i64 * 65535 + vw1 / 2) / vw1;
    let ny = ((py - vy) as i64 * 65535 + vh1 / 2) / vh1;
    Some(mouse(nx as i32, ny as i32, data, flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK))
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type:    INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn send(inputs: &[INPUT]) {
    if !inputs.is_empty() {
        unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    }
}

// ── touch ──

struct Touch {
    device:   Option<HSYNTHETICPOINTERDEVICE>,
    contacts: HashMap<u8, POINTER_TOUCH_INFO>,
}

// SAFETY: the handle of the device is used only under the lock
unsafe impl Send for Touch {}

static TOUCH: std::sync::LazyLock<Mutex<Touch>> = std::sync::LazyLock::new(|| Mutex::new(Touch { device: None, contacts: HashMap::new() }));

pub fn touch(event: &Viewer_TouchEvent) {
    let mut touch = TOUCH.lock().unwrap();
    if touch.device.is_none() {
        match unsafe { CreateSyntheticPointerDevice(PT_TOUCH, 10, POINTER_FEEDBACK_DEFAULT) } {
            Ok(d) => touch.device = Some(d),
            Err(e) => return eprintln!("input: no synthetic touch device: {e}"),
        }
    }
    let kind = event.kind();
    let layout = LAYOUT.lock().unwrap();

    // every contact goes in every frame of touch: the changed ones as the event says, the others as they were
    for (_, info) in touch.contacts.iter_mut() {
        info.pointerInfo.pointerFlags = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
    }
    for p in event.points.iter().flatten() {
        let Some((x, y)) = layout.pixel(p.at) else { continue };
        let flags = match kind {
            Viewer_TouchEvent_Kind::START => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            Viewer_TouchEvent_Kind::MOVE => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            Viewer_TouchEvent_Kind::END => POINTER_FLAG_UP,
            _ => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
        };
        let (rx, ry) = (p.radius_x.max(2) as i32, p.radius_y.max(2) as i32);
        let mut info = POINTER_TOUCH_INFO::default();
        info.pointerInfo = POINTER_INFO { pointerType: PT_TOUCH, pointerId: p.id() as u32, ptPixelLocation: POINT { x, y }, pointerFlags: flags, ..Default::default() };
        info.touchMask = TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE;
        info.rcContact = RECT { left: x - rx, top: y - ry, right: x + rx, bottom: y + ry };
        info.orientation = p.angle as u32;
        info.pressure = p.pressure as u32 * 1024 / 255;
        touch.contacts.insert(p.id(), info);
    }
    drop(layout);
    // the device takes 10 contacts
    let mut frame = [POINTER_TYPE_INFO::default(); 10];
    let mut n = 0;
    for (slot, info) in frame.iter_mut().zip(touch.contacts.values()) {
        *slot = POINTER_TYPE_INFO { r#type: PT_TOUCH, Anonymous: POINTER_TYPE_INFO_0 { touchInfo: *info } };
        n += 1;
    }
    if let Some(device) = touch.device {
        if let Err(e) = unsafe { InjectSyntheticPointerInput(device, &frame[..n]) } {
            eprintln!("input: touch: {e}");
        }
    }
    // the contacts that went up are gone after this frame
    let up = POINTER_FLAGS(POINTER_FLAG_UP.0);
    touch.contacts.retain(|id, info| !(info.pointerInfo.pointerFlags.0 & up.0 != 0 && event.points.iter().flatten().any(|p| p.id() == *id)));
}
