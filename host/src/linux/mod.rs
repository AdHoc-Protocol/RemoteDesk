// The machine on Linux: the same functions as the other systems give. Two kinds of desktop:
//   X11     - the X server itself: the displays ( RandR ), the capture ( MIT-SHM, DAMAGE ), the pointer ( XFIXES ), the input ( XTEST );
//   Wayland - the portals of the desktop: one session of RemoteDesktop with ScreenCast, its frames by PipeWire. The user of the
//             machine allows it once; the allowance is kept in `portal.token` beside the program.
// H.264 by openh264, the sound by PulseAudio ( PipeWire serves it too ).

use std::sync::LazyLock;

mod h264;
mod portal;
#[cfg(feature = "native")]
mod pw;
mod x11;

#[path = "../unix/clip.rs"]
pub mod clip;
pub mod files;
pub mod input;
pub mod marks;
pub mod mode;
pub mod rights;
pub mod sound;
pub mod video;

/// The desktop is of Wayland: capture and input go through the portals. Else the X server is asked.
pub fn wayland() -> bool {
    static WAYLAND: LazyLock<bool> =
        LazyLock::new(|| std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") || std::env::var_os("WAYLAND_DISPLAY").is_some());
    *WAYLAND
}

pub fn init() {
    if wayland() {
        println!("the desktop is of Wayland: the user of the machine is asked once to share the screen and the input");
    } else if x11::x().is_none() {
        eprintln!("no X server: DISPLAY is not set or does not answer");
    }
}

/// Runs the host on this thread.
pub fn main(run: fn()) { run() }
