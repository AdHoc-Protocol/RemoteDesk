// The machine of RemoteDesk: shows its desktop to a viewer page and takes its input.
//
// protocol/ is the code AdHoc generates from D:\rdp\RemoteDesk.cs ( deployed there by D:\rdp\generated\RemoteDesk.md ): its
// modules must sit at the root of the crate as `lib` and `gen`.

#[allow(dead_code, unused_imports, unused_variables, unused_mut, non_snake_case, non_camel_case_types, non_upper_case_globals, unreachable_code)]
#[path = "../protocol/lib/mod.rs"]
pub mod lib;

#[allow(dead_code, unused_imports, unused_variables, unused_mut, non_snake_case, non_camel_case_types, non_upper_case_globals, unreachable_code)]
#[path = "../protocol/gen/mod.rs"]
pub mod gen;

#[cfg(windows)]
#[path = "win/mod.rs"]
mod os;

#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod os;

#[cfg(target_os = "macos")]
#[path = "mac/mod.rs"]
mod os;

mod audio;
mod clipboard;
mod elevate;
mod files;
mod input;
mod keymap;
mod direct;
mod menu;
mod portmap;
mod meet;
mod resize;
mod session;
mod video;

fn main() {
    os::init();
    os::main(session::run);
}
