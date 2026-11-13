// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Receive storage and default buffer sizing for transport connections.
//!
//! [`RxBuf`] keeps received bytes available while a sink applies backpressure. A relay may take the
//! allocation and replace it with storage of the same length. Output copies retained bytes into the
//! transport's transmit buffer.

/// A buffer of the transport: a fixed run of bytes.
pub type Buf = Box<[u8]>;

/// The size of the send and of the receive buffer of a connection by default.
pub const BUFFER_SIZE: usize = 1024;

use std::cell::{Cell, UnsafeCell};

// RxBuf: the receive buffer of a connection

/// Receive allocation retained while a connection consumes its input.
///
/// [`take`](Self::take) exchanges the allocation with relay storage of equal length, avoiding a
/// payload copy. The transport accesses this state on its owning loop. The buffer can move to
/// another thread, but cannot be shared between threads: it is `Send`, not `Sync`.
pub struct RxBuf {
    bytes: UnsafeCell<Buf>,
    /// Where the bytes received end, when the storage may be taken over - nothing behind a region
    /// that runs to here is the connection's; 0 - it may not (a WebSocket's frames behind the region,
    /// a decrypting layer's bytes).
    filled: Cell<usize>,
}

impl RxBuf {
    /// A buffer of `size` bytes.
    pub fn new(size: usize) -> Box<RxBuf> {
        Box::new(RxBuf {
            bytes: UnsafeCell::new(vec![0u8; size].into_boxed_slice()),
            filled: Cell::new(0),
        })
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        // SAFETY: the storage is replaced only by `take`, on the loop of the connection
        unsafe { (&*self.bytes.get()).len() }
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline(always)]
    pub fn as_ptr(&self) -> *mut u8 {
        // SAFETY: as in `len`
        unsafe { (&mut *self.bytes.get()).as_mut_ptr() }
    }

    /// Borrows bytes in `[at, end)`.
    ///
    /// # Safety
    ///
    /// Bounds must satisfy `at <= end && end <= self.len()`. The storage must remain allocated and
    /// must not be exchanged through `take` for the returned lifetime. No overlapping bytes may be
    /// mutated while the slice is borrowed.
    #[inline(always)]
    pub unsafe fn slice(&self, at: usize, end: usize) -> &[u8] {
        assert!(at <= end && end <= self.len(), "receive buffer range is out of bounds");
        std::slice::from_raw_parts(self.as_ptr().add(at), end - at)
    }

    /// Mutably borrows bytes in `[at, end)`.
    ///
    /// # Safety
    ///
    /// Bounds must satisfy `at <= end && end <= self.len()`. The storage must remain allocated and
    /// must not be exchanged through `take` for the returned lifetime. The caller must ensure
    /// exclusive access to the selected range, with no overlapping reads or writes.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slice_mut(&self, at: usize, end: usize) -> &mut [u8] {
        assert!(at <= end && end <= self.len(), "receive buffer range is out of bounds");
        std::slice::from_raw_parts_mut(self.as_ptr().add(at), end - at)
    }

    /// The bytes received end at `end`, and the storage may be taken over by a region that runs to
    /// there; 0 - it may not.
    #[inline(always)]
    pub fn set_filled(&self, end: usize) {
        self.filled.set(end);
    }

    /// A window ending at `end` runs to the end of the bytes received: its storage may be taken
    /// over ([`take`](Self::take)).
    #[inline(always)]
    pub fn ends_at(&self, end: *const u8) -> bool {
        let f = self.filled.get();
        f != 0 && self.as_ptr().wrapping_add(f) as *const u8 == end
    }

    /// As [`take`](Self::take), `with` of any length: the whole-record buffer of a chain, and back.
    #[inline]
    pub(crate) fn swap(&self, with: Buf) -> Buf {
        self.filled.set(0);
        // SAFETY: on the loop of the connection, between its reads
        unsafe { std::mem::replace(&mut *self.bytes.get(), with) }
    }

    /// Exchanges the receive allocation with `with`, returning the previous allocation without
    /// copying payload bytes. The replacement must have the same length. Call only between buffer
    /// accesses on the owning transport loop, after outstanding slices are no longer used.
    ///
    /// # Panics
    ///
    /// Panics if the replacement has a different length.
    #[inline]
    pub fn take(&self, with: Buf) -> Buf {
        assert_eq!(with.len(), self.len(), "replacement receive buffer has a different length");
        self.filled.set(0);
        // SAFETY: on the loop of the connection, between its reads
        unsafe { std::mem::replace(&mut *self.bytes.get(), with) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::adhoc::BytesDst;

    #[test]
    fn receive_buffers_can_move_but_cannot_be_shared_between_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<RxBuf>();

        trait AmbiguousIfSync<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
        // Type inference becomes ambiguous if RxBuf implements Sync.
        let _ = <RxBuf as AmbiguousIfSync<_>>::check;
    }

    #[test]
    fn safe_sink_calls_reject_invalid_buffer_ranges() {
        struct Sink;
        impl BytesDst for Sink {
            fn on_bytes(&mut self, _: &[u8]) -> bool {
                true
            }
        }

        let buffer = RxBuf::new(8);
        for (at, end) in [(0, 9), (5, 4)] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Sink.on_block(&buffer, at, end)));
            assert!(result.is_err(), "accepted invalid buffer range {at}..{end}");
        }
    }

    #[test]
    fn invalid_replacement_keeps_the_original_buffer() {
        let buffer = RxBuf::new(8);
        let original = buffer.as_ptr();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| buffer.take(vec![0; 4].into_boxed_slice())));
        assert!(result.is_err());
        assert_eq!(buffer.len(), 8);
        assert_eq!(buffer.as_ptr(), original);
    }
}
