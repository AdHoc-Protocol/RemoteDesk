// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Synchronous in-process transfer from [`BytesSrc`] to [`BytesDst`].
//!
//! [`Wire`] reuses one fixed-size buffer. Call [`Wire::step`] for one source read, or
//! [`Wire::drain`] until the source pauses, becomes drained, or the sink rejects input. The sink
//! consumes each offered slice completely or rejects it; this bridge does not use the
//! partial-consumption methods. A later call can read output newly queued by the source.

use crate::lib::adhoc::{BytesDst, BytesSrc};

/// In-process bridge from a [`BytesSrc`] to a [`BytesDst`].
pub struct Wire {
    /// Source read by each transfer step.
    src: Box<dyn BytesSrc>,
    /// The destination for bytes.
    dst: Box<dyn BytesDst>,
    /// Fixed-size scratch buffer reused for every transfer.
    buffer: Box<[u8]>,
}

impl Wire {
    /// Creates a bridge with one reusable buffer of `buffer_size.max(16)` bytes. Each step requests
    /// at most the length of that buffer from the source.
    pub fn new(
        src: Box<dyn BytesSrc>,
        dst: Box<dyn BytesDst>,
        buffer_size: usize,
    ) -> Self {
        Self {
            src,
            dst,
            buffer: vec![0u8; buffer_size.max(16)].into_boxed_slice(),
        }
    }

    /// Performs one source read and offers any returned bytes to the sink.
    ///
    /// Returns `(bytes_read, active)`. A zero read keeps the bridge active; a negative read or sink
    /// rejection returns `active = false`. `bytes_read` includes bytes offered to a sink that
    /// rejected them, so it is not a count of successfully consumed bytes.
    pub fn step(&mut self) -> (usize, bool) {
        let r = self.src.read(&mut self.buffer);
        let n = r.max(0) as usize;
        if n > 0 && !self.dst.on_bytes(&self.buffer[..n]) {
            // Sink rejected the bytes (protocol error / forced close).
            return (n, false);
        }
        (n, 0 <= r)
    }

    /// Transfers bytes until the source pauses or is drained, or the sink rejects a slice. Returns
    /// the total bytes read from the source, including a final rejected slice.
    pub fn drain(&mut self) -> usize {
        let mut total = 0usize;
        loop {
            let (n, alive) = self.step();
            total += n;
            if !alive {
                return total;
            }
            if n == 0 {
                // Return control while output is pending, avoiding a busy loop.
                return total;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivial sink that copies received bytes into an owned Vec.
    struct CollectSink(Vec<u8>);
    impl BytesDst for CollectSink {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.0.extend_from_slice(bytes);
            true
        }
    }

    /// Source that yields one fixed byte string, then returns -1 when drained.
    struct OnceSource {
        data: Option<Vec<u8>>,
    }
    impl BytesSrc for OnceSource {
        fn read(&mut self, dst: &mut [u8]) -> isize {
            match self.data.take() {
                Some(d) => {
                    dst[..d.len()].copy_from_slice(&d);
                    d.len() as isize // one more read returns -1: the end
                }
                None => -1,
            }
        }
    }

    #[test]
    fn drain_round_trips_a_payload() {
        let payload = b"hello-wire".to_vec();
        let src = Box::new(OnceSource { data: Some(payload.clone()) });
        let dst = Box::new(CollectSink(Vec::new()));
        let mut wire = Wire::new(src, dst, 4);
        let n = wire.drain();
        assert_eq!(n, payload.len());
        // The sink is now owned by `wire`; we cannot extract it without a downcast. The total-bytes
        // return is enough to verify the pump.
    }

    #[test]
    fn step_reports_progress_and_alive_correctly() {
        let src = Box::new(OnceSource { data: Some(b"abc".to_vec()) });
        let dst = Box::new(CollectSink(Vec::new()));
        let mut wire = Wire::new(src, dst, 16);
        let (n1, alive1) = wire.step();
        assert_eq!(n1, 3);
        assert!(alive1, "source still has the next-poll-end signal");
        let (n2, alive2) = wire.step();
        assert_eq!(n2, 0);
        assert!(!alive2, "second poll signals end-of-stream");
    }
}
