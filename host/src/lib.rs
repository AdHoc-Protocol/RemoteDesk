// The machine of RemoteDesk: shows its desktop to a viewer page and takes its input.
//
// A library, so that the program of a computer ( src/main.rs ) and the app of a phone ( ../android, which loads it as
// librdp_host.so ) run the same host. protocol/ is the code AdHoc generates from D:\rdp\RemoteDesk.cs ( deployed there by
// D:\rdp\generated\RemoteDesk.md ): its modules must sit at the root of the crate as `lib` and `gen`.

#[allow(dead_code, unused_imports, unused_variables, unused_mut, non_snake_case, non_camel_case_types, non_upper_case_globals, unreachable_code)]
#[path = "../protocol/lib/mod.rs"]
pub mod lib;

#[allow(dead_code, unused_imports, unused_variables, unused_mut, non_snake_case, non_camel_case_types, non_upper_case_globals, unreachable_code)]
#[path = "../protocol/gen/mod.rs"]
pub mod gen;

#[macro_use]
mod console; // first: its say! is for the modules after it

#[cfg(windows)]
#[path = "win/mod.rs"]
pub mod os;

#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
pub mod os;

#[cfg(target_os = "macos")]
#[path = "mac/mod.rs"]
pub mod os;

#[cfg(target_os = "android")]
#[path = "android/mod.rs"]
pub mod os;

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
pub mod session;
mod video;
