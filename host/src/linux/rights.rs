// The rights on Linux: the X server and the portal let the input reach every window as it is - nothing to ask for.

/// Why the host was not started again.
pub enum Refused {
    #[allow(dead_code)]
    Declined,
    Failed(String),
}

/// The host runs as root.
pub fn elevated() -> bool { unsafe { libc::geteuid() == 0 } }

/// The rights do not matter to the input.
pub fn matter() -> bool { false }

pub fn relaunch(_args: &[String]) -> Result<(), Refused> { Err(Refused::Failed(String::from("the input needs no other rights on Linux"))) }
