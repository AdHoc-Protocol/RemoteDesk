// The resolution of a display on Windows: the largest mode of the display that fits, the refresh rate kept when the display has
// it. The change is temporary - Windows does not store it - and is restored to what is stored.

use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_FULLSCREEN, CDS_TYPE, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};

use super::video::Display;

/// The devices whose resolution the page changed: restored at the end.
static CHANGED: Mutex<Vec<Vec<u16>>> = Mutex::new(Vec::new());

/// Every display changed gets its stored resolution back.
pub fn restore_all() {
    for device in CHANGED.lock().unwrap().drain(..) {
        restore(&device);
    }
}

fn device(display: &Display) -> Vec<u16> { format!("\\\\.\\{}", display.name).encode_utf16().chain(Some(0)).collect() }

/// `display` gets its stored resolution back, if it was changed.
pub fn restore_one(display: &Display) {
    let device = device(display);
    let mut changed = CHANGED.lock().unwrap();
    if let Some(i) = changed.iter().position(|d| *d == device) {
        restore(&changed.swap_remove(i));
        println!("display {}: the resolution restored", display.name);
    }
}

/// The largest mode of `display` that fits in `width` × `height` pixels.
pub fn fit(display: &Display, width: u32, height: u32) {
    let device = device(display);
    unsafe {
        let mut current = mode();
        if !EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut current).as_bool() {
            return;
        }
        let modes: Vec<DEVMODEW> = (0..)
            .map_while(|i| {
                let mut m = mode();
                EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut m).as_bool().then_some(m)
            })
            .filter(|m| m.dmBitsPerPel == current.dmBitsPerPel)
            .collect();
        // a rotated display may list its modes in the orientation of the panel: then they are turned to compare
        let (cw, ch) = (current.dmPelsWidth, current.dmPelsHeight);
        let listed_as_shown = modes.iter().any(|m| m.dmPelsWidth == cw && m.dmPelsHeight == ch);
        let shown = |m: &DEVMODEW| if listed_as_shown || cw == ch { (m.dmPelsWidth, m.dmPelsHeight) } else { (m.dmPelsHeight, m.dmPelsWidth) };

        let fits = |m: &&DEVMODEW| {
            let (w, h) = shown(m);
            w <= width && h <= height
        };
        let best = modes
            .iter()
            .filter(fits)
            .max_by_key(|m| {
                let (w, h) = shown(m);
                (w as u64 * h as u64, (m.dmDisplayFrequency == current.dmDisplayFrequency) as u8, m.dmDisplayFrequency)
            })
            .or_else(|| modes.iter().min_by_key(|m| { let (w, h) = shown(m); w as u64 * h as u64 }));
        let Some(best) = best else { return };
        let (w, h) = shown(best);
        if (w, h) == (cw, ch) {
            return;
        }

        let mut wanted = current;
        wanted.dmPelsWidth = if listed_as_shown || cw == ch { best.dmPelsWidth } else { best.dmPelsHeight };
        wanted.dmPelsHeight = if listed_as_shown || cw == ch { best.dmPelsHeight } else { best.dmPelsWidth };
        wanted.dmDisplayFrequency = best.dmDisplayFrequency;
        wanted.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        let result = ChangeDisplaySettingsExW(PCWSTR(device.as_ptr()), Some(&wanted), None, CDS_FULLSCREEN, None);
        if result == DISP_CHANGE_SUCCESSFUL {
            println!("display {}: {cw}×{ch} → {w}×{h} for a window of {width}×{height}", display.name);
            let mut changed = CHANGED.lock().unwrap();
            if !changed.contains(&device) {
                changed.push(device);
            }
        } else {
            eprintln!("display {}: {w}×{h} is not taken ( {} )", display.name, result.0);
        }
    }
}

fn mode() -> DEVMODEW { DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() } }

/// The resolution Windows has stored for `device` ( UTF-16, ending in 0 ).
fn restore(device: &[u16]) {
    unsafe {
        let _ = ChangeDisplaySettingsExW(PCWSTR(device.as_ptr()), None, None, CDS_TYPE(0), None);
    }
}
