// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Receive errors and their process-wide handler.
//!
//! Errors include checksum mismatches, unknown packet identifiers, and lengths beyond protocol
//! limits. Install a handler during initialization; the default handler writes to the console.

use std::fmt::Display;

/// The errors of a receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RxOnError {
    /// Two `0xFF` in a row: a frame ends twice — the stream is out of step.
    FfffError = 0,
    /// The CRC of a pack does not match: it was damaged on the way, it is dropped.
    CrcError = 1,
    /// The bytes do not make what they should: a frame torn, an escape or a string broken.
    BytesDistortion = 3,
    /// A length, a count or a dimension above what the protocol allows.
    Overflow = 4,
    /// The pack id is of no pack this side receives.
    InvalidId = 5,
    /// A pack was refused by the rules of the data flow or of the application.
    Rejected = 6,
    Timeout = 7,
    Error = 8,
    /// Packs dropped: the state of the connection does not take them.
    DiscardedByState = 9,
    /// A pack for an actor that receives nothing.
    ActorNotReceivable = 10,
}

/// A handler of the errors of the receive: the error, where it happened - the connection, the pack
/// -, what is known of it, and the failure under it if there is one. A function, not an object:
/// nothing allocated, nothing counted - its state, when it needs any, is its own statics.
pub type RxHandler = fn(RxOnError, src: &dyn Display, arg: &dyn Display, ex: Option<&(dyn std::error::Error + 'static)>);

/// Writes the error and its packet and state context to the console. Includes the underlying error,
/// or a backtrace when `RUST_BACKTRACE` enables capture.
pub fn rx_to_console(e: RxOnError, src: &dyn Display, arg: &dyn Display, ex: Option<&(dyn std::error::Error + 'static)>) {
    match ex {
        Some(ex) => eprintln!("{e:?} at {src}\nArg: {arg}\n{ex}"),
        None => eprintln!("{e:?} at {src}\nArg: {arg}\n{}", std::backtrace::Backtrace::capture()),
    }
}

/// The handler of the process: null - the default. One load to read it, no lock.
static RX: std::sync::atomic::AtomicPtr<()> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// The handler of the errors of every receive of the process: set once, at the start.
pub fn set_rx_handler(f: RxHandler) {
    RX.store(f as *mut (), std::sync::atomic::Ordering::Release);
}

#[inline]
fn handler() -> RxHandler {
    let p = RX.load(std::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        rx_to_console
    } else {
        // SAFETY: only `set_rx_handler` stores here, and only a `RxHandler`
        unsafe { std::mem::transmute::<*mut (), RxHandler>(p) }
    }
}

/// An error of a receive, to its handler: `what` is known of it, where it happened is not.
#[cold]
pub fn rx(e: RxOnError, what: &dyn Display) {
    handler()(e, &"", what, None)
}

/// An error of a receive, to its handler, with all that is known of it.
#[cold]
pub fn rx_at(e: RxOnError, src: &dyn Display, arg: &dyn Display, ex: Option<&(dyn std::error::Error + 'static)>) {
    handler()(e, src, arg, ex)
}

/// What the errors of this thread were: the tests hear them so, side by side (the handler is one
/// for the process).
#[cfg(test)]
pub(crate) mod heard {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static HEARD: RefCell<Vec<RxOnError>> = const { RefCell::new(Vec::new()) };
    }

    fn hear(e: RxOnError, _src: &dyn Display, _what: &dyn Display, _ex: Option<&(dyn std::error::Error + 'static)>) {
        HEARD.with(|h| h.borrow_mut().push(e));
    }

    /// From now on the errors are heard by the thread they happen on.
    pub(crate) fn on() {
        set_rx_handler(hear);
    }

    /// The errors this thread heard, taken.
    pub(crate) fn take() -> Vec<RxOnError> {
        HEARD.with(|h| std::mem::take(&mut *h.borrow_mut()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::adhoc::connection::receiver::{Rx, RxHost, RxState};

    /// Receives nothing: every pack id is unknown.
    struct Nobody;

    impl RxHost for Nobody {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, _id: u32) -> bool {
            false
        }
        fn put_bytes(&mut self, _src: &mut Rx<'_>) -> bool {
            true
        }
        fn on_received(&mut self, _st: &mut RxState) {}
    }

    /// An unknown pack id breaks the stream and is told to the handler of the process (on the
    /// thread of the receive).
    #[test]
    fn an_unknown_id_is_told_to_the_handler() {
        heard::on();
        let mut st = RxState::new(1);
        assert!(st.write_all(&[7, 1, 2], &mut Nobody) < 0);
        assert_eq!(heard::take(), vec![RxOnError::InvalidId]);
    }
}
