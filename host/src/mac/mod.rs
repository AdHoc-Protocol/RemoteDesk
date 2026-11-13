// The machine on macOS: the same functions as the other systems give. The screen and the sound by ScreenCaptureKit, H.264 by
// VideoToolbox ( the hardware of the Mac ), the input by CGEvent. macOS asks its user once for two permissions of Privacy &
// Security: Screen Recording ( the capture ) and Accessibility ( the input ).

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

#[path = "../unix/clip.rs"]
pub mod clip;
pub mod files;
pub mod input;
pub mod mode;
pub mod rights;
pub mod sound;
pub mod video;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: *const std::ffi::c_void;
}

/// What runs on the main thread: AppKit shows its windows there only.
type Job = Box<dyn FnOnce() + Send>;
static MAIN: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();

/// Runs `job` on the main thread and waits for what it gives.
pub fn on_main<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (back, answer) = channel();
    let sender = MAIN.get()?.lock().unwrap().clone();
    sender.send(Box::new(move || { let _ = back.send(job()); })).ok()?;
    answer.recv().ok()
}

/// The permissions: macOS shows its dialogs the first time, the user grants them in System Settings.
pub fn init() {
    if !objc2_core_graphics::CGPreflightScreenCaptureAccess() {
        println!("macOS asks for Screen Recording: grant it in System Settings > Privacy & Security, then start the host again");
        objc2_core_graphics::CGRequestScreenCaptureAccess();
    }
    unsafe {
        use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType};
        let key = &*(kAXTrustedCheckOptionPrompt as *const CFString);
        let options = CFDictionary::<CFString, CFType>::from_slices(&[key], &[CFBoolean::new(true).as_ref()]);
        if !AXIsProcessTrustedWithOptions(&*options as *const _ as *const std::ffi::c_void) {
            println!("macOS asks for Accessibility: grant it in System Settings > Privacy & Security, else the input of the viewer is not taken");
        }
    }
}

/// Runs the host on a thread of its own: the main thread serves AppKit.
pub fn main(run: fn()) {
    let (sender, jobs) = channel::<Job>();
    let _ = MAIN.set(Mutex::new(sender));
    std::thread::Builder::new().name("host".into()).spawn(run).expect("the thread of the host");
    for job in jobs {
        job();
    }
}
