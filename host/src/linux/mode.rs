// The resolution of a display on Linux. X11: RandR - the largest mode of the output of the display that fits, the refresh rate kept
// when the output has it; restored to the mode it had. Wayland: the compositor keeps it - the portals change no mode.

use std::sync::Mutex;

use x11rb::protocol::randr::{ConnectionExt as _, Rotation};

use super::video::Display;
use super::{wayland, x11};

/// The CRTCs changed: ( crtc, the mode it had ), restored at the end.
static CHANGED: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// Every display changed gets the mode it had back.
pub fn restore_all() {
    for (crtc, mode) in CHANGED.lock().unwrap().drain(..) {
        set(crtc, mode);
    }
}

/// `display` gets the mode it had back, if it was changed.
pub fn restore_one(display: &Display) {
    let Some((crtc, _)) = crtc_of(display) else { return };
    let mut changed = CHANGED.lock().unwrap();
    if let Some(i) = changed.iter().position(|c| c.0 == crtc) {
        let (crtc, mode) = changed.swap_remove(i);
        set(crtc, mode);
        println!("display {}: the resolution restored", display.name);
    }
}

/// The CRTC of the first output of `display`, and the timestamp of the configuration.
fn crtc_of(display: &Display) -> Option<(u32, u32)> {
    let x = x11::x().filter(|x| x.randr && !wayland())?;
    let resources = x.conn.randr_get_screen_resources_current(x.root).ok()?.reply().ok()?;
    let output = x.conn.randr_get_output_info(*display.outputs.first()?, resources.config_timestamp).ok()?.reply().ok()?;
    (output.crtc != 0).then_some((output.crtc, resources.config_timestamp))
}

/// The CRTC `crtc` in `mode`, where it is, turned as it is.
fn set(crtc: u32, mode: u32) -> bool {
    let Some(x) = x11::x() else { return false };
    let Some(resources) = x.conn.randr_get_screen_resources_current(x.root).ok().and_then(|c| c.reply().ok()) else { return false };
    let Some(info) = x.conn.randr_get_crtc_info(crtc, resources.config_timestamp).ok().and_then(|c| c.reply().ok()) else { return false };
    x.conn
        .randr_set_crtc_config(crtc, x11rb::CURRENT_TIME, resources.config_timestamp, info.x, info.y, mode, info.rotation, &info.outputs)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|r| u8::from(r.status) == 0)
}

/// The largest mode of `display` that fits in `width` × `height` pixels.
pub fn fit(display: &Display, width: u32, height: u32) {
    if wayland() {
        return println!("display {}: the resolution is the compositor's on Wayland", display.name);
    }
    let Some(x) = x11::x().filter(|x| x.randr) else { return };
    let Some((crtc, stamp)) = crtc_of(display) else { return };
    let Some(resources) = x.conn.randr_get_screen_resources_current(x.root).ok().and_then(|c| c.reply().ok()) else { return };
    let Some(info) = x.conn.randr_get_crtc_info(crtc, stamp).ok().and_then(|c| c.reply().ok()) else { return };
    let Some(output) = x.conn.randr_get_output_info(info.outputs.first().copied().unwrap_or(0), stamp).ok().and_then(|c| c.reply().ok()) else { return };

    let turned = u16::from(info.rotation) & (u16::from(Rotation::ROTATE90) | u16::from(Rotation::ROTATE270)) != 0;
    let shown = |w: u16, h: u16| if turned { (h as u32, w as u32) } else { (w as u32, h as u32) };
    let refresh = |m: &x11rb::protocol::randr::ModeInfo| if m.htotal == 0 || m.vtotal == 0 { 0 } else { m.dot_clock as u64 * 100 / (m.htotal as u64 * m.vtotal as u64) };
    let modes: Vec<_> = resources.modes.iter().filter(|m| output.modes.contains(&m.id)).collect();
    let Some(current) = modes.iter().find(|m| m.id == info.mode) else { return };
    let rate = refresh(current);
    let best = modes
        .iter()
        .filter(|m| {
            let (w, h) = shown(m.width, m.height);
            w <= width && h <= height
        })
        .max_by_key(|m| {
            let (w, h) = shown(m.width, m.height);
            (w as u64 * h as u64, (refresh(m).abs_diff(rate) < 100) as u8, refresh(m))
        })
        .or_else(|| modes.iter().min_by_key(|m| m.width as u32 * m.height as u32));
    let Some(best) = best else { return };
    if best.id == info.mode {
        return;
    }
    let (cw, ch) = shown(current.width, current.height);
    let (w, h) = shown(best.width, best.height);
    {
        let mut changed = CHANGED.lock().unwrap();
        if !changed.iter().any(|c| c.0 == crtc) {
            changed.push((crtc, info.mode));
        }
    }
    if set(crtc, best.id) {
        println!("display {}: {cw}×{ch} → {w}×{h} for a window of {width}×{height}", display.name);
    } else {
        eprintln!("display {}: {w}×{h} is not taken", display.name);
    }
}
