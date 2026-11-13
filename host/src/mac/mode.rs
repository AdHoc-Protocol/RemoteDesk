// The resolution of a display on macOS: the largest mode of the display that fits, the refresh rate kept when it has it. The
// change is for this session of the user only; the restore gives every display the configuration macOS has stored.

use std::sync::Mutex;

use objc2_core_foundation::CFArray;
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCompleteDisplayConfiguration, CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDisplayConfigRef, CGDisplayCopyAllDisplayModes,
    CGDisplayCopyDisplayMode, CGDisplayMode, CGRestorePermanentDisplayConfiguration,
};

use super::video::Display;

/// A display was changed: the stored configuration is restored at the end.
static CHANGED: Mutex<bool> = Mutex::new(false);

/// Every display gets the configuration macOS has stored.
pub fn restore_all() {
    if std::mem::take(&mut *CHANGED.lock().unwrap()) {
        CGRestorePermanentDisplayConfiguration();
    }
}

/// macOS restores all the displays at once.
pub fn restore_one(display: &Display) {
    if *CHANGED.lock().unwrap() {
        restore_all();
        println!("display {}: the resolution restored", display.name);
    }
}

/// The largest mode of `display` that fits in `width` × `height` pixels.
pub fn fit(display: &Display, width: u32, height: u32) {
    let Some(current) = CGDisplayCopyDisplayMode(display.id) else { return };
    let Some(modes) = (unsafe { CGDisplayCopyAllDisplayModes(display.id, None) }) else { return };
    let modes: &CFArray<CGDisplayMode> = unsafe { modes.cast_unchecked() };
    let size = |m: &CGDisplayMode| (CGDisplayMode::pixel_width(Some(m)) as u32, CGDisplayMode::pixel_height(Some(m)) as u32);
    let rate = |m: &CGDisplayMode| CGDisplayMode::refresh_rate(Some(m)).round() as u32;
    let (cw, ch) = size(&current);
    let now_rate = rate(&current);
    let all: Vec<_> = modes.iter().collect();
    let best = all
        .iter()
        .filter(|m| {
            let (w, h) = size(m);
            w <= width && h <= height
        })
        .max_by_key(|m| {
            let (w, h) = size(m);
            (w as u64 * h as u64, (rate(m) == now_rate) as u8)
        })
        .or_else(|| all.iter().min_by_key(|m| size(m).0 as u64 * size(m).1 as u64));
    let Some(best) = best else { return };
    let (w, h) = size(best);
    if (w, h) == (cw, ch) {
        return;
    }
    unsafe {
        let mut config: CGDisplayConfigRef = std::ptr::null_mut();
        if CGBeginDisplayConfiguration(&mut config).0 != 0 {
            return;
        }
        let _ = CGConfigureDisplayWithDisplayMode(config, display.id, Some(best), None);
        if CGCompleteDisplayConfiguration(config, CGConfigureOption::ForSession).0 == 0 {
            *CHANGED.lock().unwrap() = true;
            println!("display {}: {cw}×{ch} → {w}×{h} for a window of {width}×{height}", display.name);
        } else {
            eprintln!("display {}: {w}×{h} is not taken", display.name);
        }
    }
}
