// The rights on Android: an app has no administrator to become - what the input needs is the service of accessibility.

/// Why the host was not started again.
pub enum Refused {
    #[allow(dead_code)]
    Declined,
    Failed(String),
}

pub fn elevated() -> bool { false }

/// The rights do not matter to the input.
pub fn matter() -> bool { false }

pub fn relaunch(_args: &[String]) -> Result<(), Refused> { Err(Refused::Failed(String::from("an app on Android has no administrator to become"))) }
