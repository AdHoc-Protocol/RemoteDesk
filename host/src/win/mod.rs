// The machine on Windows: the same functions as the other systems give, made of the APIs of Windows.

mod capture;
mod encode;
mod gpu;

pub mod clip;
pub mod files;
pub mod input;
pub mod marks;
pub mod mode;
pub mod rights;
pub mod sound;
pub mod video;

/// Before anything: a crash told, not a window closed in silence; the coordinates of the desktop in physical pixels - the capture
/// and the input agree on them.
pub fn init() {
    crate::lib::network::failure::catch_crashes();
    capture::dpi_aware();
}

/// Runs the host on this thread.
pub fn main(run: fn()) { run() }
