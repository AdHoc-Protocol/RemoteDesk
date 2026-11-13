// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Transmit errors and their process-wide handler.
//!
//! Reports packets dropped during encoding or transmission. Install a handler during
//! initialization; the default handler writes to the console.

use std::fmt::Display;

/// The errors of a transmit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TxOnError {
    /// A pack was refused by the rules of the data flow or of the application.
    Rejected = 0,
    /// A length above what the protocol allows.
    Overflow = 1,
    Timeout = 2,
    Error = 3,
    /// A pack still queued when its connection closed: never sent.
    DiscardedOnClose = 4,
    /// A pack dropped: the state of the connection does not send it.
    DiscardedByState = 5,
    /// A pack sent by an actor that sends nothing.
    ActorNotTransmittable = 6,
}

/// A handler of the errors of the transmit: the error, where it happened - the connection, the pack
/// -, what is known of it, and the failure under it if there is one. A function, not an object:
/// nothing allocated, nothing counted - its state, when it needs any, is its own statics.
pub type TxHandler = fn(TxOnError, src: &dyn Display, arg: &dyn Display, ex: Option<&(dyn std::error::Error + 'static)>);

/// Writes the error and its packet and state context to the console. Includes the underlying error,
/// or a backtrace when `RUST_BACKTRACE` enables capture.
pub fn tx_to_console(e: TxOnError, src: &dyn Display, arg: &dyn Display, ex: Option<&(dyn std::error::Error + 'static)>) {
    match ex {
        Some(ex) => eprintln!("{e:?} at {src}\nArg: {arg}\n{ex}"),
        None => eprintln!("{e:?} at {src}\nArg: {arg}\n{}", std::backtrace::Backtrace::capture()),
    }
}

/// The handler of the process: null - the default. One load to read it, no lock.
static TX: std::sync::atomic::AtomicPtr<()> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// The handler of the errors of every transmit of the process: set once, at the start.
pub fn set_tx_handler(f: TxHandler) {
    TX.store(f as *mut (), std::sync::atomic::Ordering::Release);
}

#[inline]
fn handler() -> TxHandler {
    let p = TX.load(std::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        tx_to_console
    } else {
        // SAFETY: only `set_tx_handler` stores here, and only a `TxHandler`
        unsafe { std::mem::transmute::<*mut (), TxHandler>(p) }
    }
}

/// An error of a transmit, to its handler: `what` is known of it, where it happened is not.
#[cold]
pub fn tx(e: TxOnError, what: &dyn Display) {
    handler()(e, &"", what, None)
}

/// An error of a transmit, to its handler, with all that is known of it.
#[cold]
pub fn tx_at(e: TxOnError, src: &dyn Display, arg: &dyn Display, ex: Option<&(dyn std::error::Error + 'static)>) {
    handler()(e, src, arg, ex)
}
