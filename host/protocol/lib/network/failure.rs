// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Transport failures and their reporting context.
//!
//! A host reports socket errors, protocol failures, callback panics, and journal errors to its
//! failure handler. I/O loops report their own failures to the engine handler.

use std::sync::{Arc, LazyLock};

/// The source - a connection, a host, a loop - and the failure.
pub type OnFailure = Arc<dyn Fn(&dyn std::fmt::Display, &(dyn std::error::Error + 'static)) + Send + Sync>;

/// The handler of a host given none: the source, then the failure, to the console.
pub fn print_console() -> OnFailure {
    static PRINT: LazyLock<OnFailure> = LazyLock::new(|| {
        Arc::new(|src: &dyn std::fmt::Display, e: &(dyn std::error::Error + 'static)| {
            println!("onFailure {src}");
            println!("{e}");
        })
    });
    PRINT.clone()
}

/// A panic as the failure it is: what it said, when it said it in text.
pub(crate) fn panicked(why: &(dyn std::any::Any + Send)) -> std::io::Error {
    let text = why.downcast_ref::<&str>().copied().or_else(|| why.downcast_ref::<String>().map(String::as_str)).unwrap_or("not text");
    std::io::Error::other(format!("panicked: {text}"))
}

/// A peer that breaks the protocol.
pub(crate) fn protocol(why: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, why.into())
}
