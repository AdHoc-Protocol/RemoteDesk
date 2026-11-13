// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Incremental decoding of chunked streams through transform chains.
//!
//! Stages run from the wire toward the destination. The receiver retains the stream and stage state
//! at each nesting level, reusing the stages for subsequent streams.

use super::string::RxText;
use crate::lib::adhoc::chain::{CustomRx, Stage};
use crate::lib::adhoc::connection::receiver::Rx;

/// A stage of the receiving side.
pub(crate) enum RxStage {
    /// A run of built-in stages, in the native library.
    Stages(crate::lib::stages::Rx),
    Custom(Box<dyn CustomRx>),
}

impl RxStage {
    fn new(stage: Stage) -> Self {
        match stage {
            Stage::Stages(run) => RxStage::Stages(crate::lib::stages::Rx::new(run)),
            Stage::Custom(c) => RxStage::Custom((c.rx)(c.args)),
        }
    }

    #[inline]
    fn arm(&mut self) {
        match self {
            RxStage::Stages(s) => s.rearm(),
            RxStage::Custom(c) => c.rearm(),
        }
    }
}

/// The leaf of a stream being received: the pack the chain carries.
#[derive(Default)]
pub(crate) struct RxLeaf {
    /// A value of the leaf parked by the end of a window of the chain (`mode`): the next bytes
    /// complete it first.
    pub(crate) mode: u8,
    /// The pack is whole: what the stream still brings is the end of the stages' framing and the
    /// terminator.
    pub(crate) done: bool,
}

/// The chunk framing of a stream being received.
#[derive(Default)]
pub(crate) struct RxFrame {
    /// Bytes left of the current chunk.
    pub(crate) rem: u32,
    /// The length prefix being read: its bytes so far and the value they make.
    pub(crate) len_got: u8,
    pub(crate) len: u32,
    /// The stream of the end of a plain tunnel ( `Rx::get_tunnel_stream` ): an `ABORT` ends the
    /// stream, torn, not the connection that carries it. `void` - it ended so.
    pub(crate) tunnel: bool,
    pub(crate) void: bool,
}

/// A stream of a chain being received: one level of nesting.
#[derive(Default)]
pub(crate) struct RxChain {
    spec: &'static [Stage],
    pub(crate) stages: Vec<RxStage>,
    pub(crate) frame: RxFrame,
    pub(crate) leaf: RxLeaf,
    /// The leaf of a stream of bytes (`Stream` field, a trimmed one): its sink, the bytes given to
    /// it, its cap `[S(N)]`.
    pub(crate) raw: Option<Box<dyn std::io::Write + Send>>,
    pub(crate) total: u64,
    pub(crate) max: u64,
    /// The leaf of a string.
    pub(crate) text: RxText,
    /// What is done with the sink when the stream ends: armed with the stream.
    pub(crate) complete: Option<crate::lib::adhoc::chain::StreamComplete>,
}

impl RxChain {
    /// The stream ends: the hook armed with it, if any, decides - it runs instead of the default,
    /// flushing the whole sink. Whole only when `completed` and its sink took all of it: one that
    /// failed on the way was let go then ([`raw_sink`]) - the hook hears it was not. A flush that
    /// fails is told the handler of the receive errors, as a write that fails: the connection goes
    /// on.
    pub(crate) fn end(&mut self, completed: bool) {
        let raw = self.raw.take();
        let whole = completed && raw.is_some();
        match self.complete.take() {
            Some(h) => {
                drop(raw);
                h(whole);
            }
            None => {
                if let (true, Some(mut d)) = (whole, raw) {
                    if let Err(e) = std::io::Write::flush(&mut d) {
                        crate::lib::adhoc::connection::receiver::on_error::rx_at(
                            crate::lib::adhoc::connection::receiver::on_error::RxOnError::Error,
                            &"the sink of a stream",
                            &"its flush failed",
                            Some(&e),
                        );
                    }
                } // a torn sink goes unflushed
            }
        }
    }

    /// The keys of the stream the chain carries, in place of the connection's ( [`crate::lib::stages::Keys`] ): the chain of a
    /// tunnel is keyed by the tunnel, not by its carrier. Set after [`arm`](Self::arm), which lets them go.
    #[inline]
    pub(crate) fn keys(&mut self, keys: &crate::lib::stages::Keys) {
        for s in &mut self.stages {
            if let RxStage::Stages(s) = s {
                s.keys(keys);
            }
        }
    }

    /// Arms the chain for a new stream of `spec`.
    pub(crate) fn arm(&mut self, spec: &'static [Stage]) {
        if self.spec != spec || self.stages.len() != spec.len() {
            self.stages.clear();
            self.stages.extend(spec.iter().map(|s| RxStage::new(*s)));
            self.spec = spec;
        } else {
            for s in &mut self.stages {
                s.arm();
            }
        }
        self.frame = RxFrame::default();
        self.leaf = RxLeaf::default();
        self.raw = None;
        self.total = 0;
    }
}

/// The bytes of the window — a chunk body, or nothing to flush — go wire to leaf through the stages
/// to `leaf`: the pack or the sink of bytes the chain carries.
pub(crate) fn push<L: FnMut(&mut Rx<'_>)>(src: &mut Rx<'_>, stages: &mut [RxStage], leaf: &mut L) {
    match stages.split_last_mut() {
        None => leaf(src),
        Some((last, inner)) => match last {
            RxStage::Stages(s) => s.put_bytes(src, |r| push(r, inner, leaf)),
            RxStage::Custom(c) => c.put_bytes(src, &mut |r| push(r, inner, leaf)),
        },
    }
}

/// The leaf of a stream of bytes: the window into the sink, whole.
pub(crate) fn raw_sink(src: &mut Rx<'_>, dst: &mut Option<Box<dyn std::io::Write + Send>>, total: &mut u64, max: u64) {
    let n = src.byte_max - src.byte_;
    if n == 0 {
        return;
    }
    *total += n as u64;
    if max < *total {
        src.error = Some(crate::lib::adhoc::connection::receiver::RxError::Overflow { got: *total, max });
        return;
    }
    if let Some(d) = dst.as_mut() {
        if let Err(e) = d.write_all(&src.buffer[src.byte_..src.byte_max]) {
            // the sink of the application failed: it is told, and the rest of the stream is read
            // unseen - the connection goes on
            crate::lib::adhoc::connection::receiver::on_error::rx_at(
                crate::lib::adhoc::connection::receiver::on_error::RxOnError::Error,
                &"the sink of a stream",
                &"failed: the rest of the stream is read unseen",
                Some(&e),
            );
            *dst = None;
        }
    }
    src.byte_ = src.byte_max;
}

// string

// chacha20

/// Where the bytes of a stream pack received go: the application assigns it; until then a pack of
/// the kind cannot be received.
pub struct Producer {
    name: &'static str,
    make: parking_lot::RwLock<Option<std::sync::Arc<dyn Fn(&dyn std::any::Any) -> Box<dyn std::io::Write + Send> + Send + Sync>>>,
}

impl Producer {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            make: parking_lot::RwLock::new(None),
        }
    }

    /// Assigns the producer of the sinks: `scope` is the connection the stream comes on, `downcast_ref` gets it.
    pub fn set(&self, make: impl Fn(&dyn std::any::Any) -> Box<dyn std::io::Write + Send> + Send + Sync + 'static) {
        *self.make.write() = Some(std::sync::Arc::new(make));
    }

    /// A sink for the pack that starts on the connection `scope`.
    pub fn make(&self, scope: &dyn std::any::Any) -> Box<dyn std::io::Write + Send> {
        let f = self.make.read().clone().unwrap_or_else(|| panic!("The producer of {} is not assigned", self.name));
        f(scope)
    }
}

#[cfg(test)]
mod abandon_tests {
    use crate::lib::adhoc::connection::receiver::RxState;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Arc;

    /// A sink that tells how it ended: 1 — flushed and dropped, 2 — dropped unflushed.
    struct Sink(Arc<AtomicU8>, bool);

    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.1 = true;
            Ok(())
        }
    }

    impl Drop for Sink {
        fn drop(&mut self) {
            self.0.store(if self.1 { 1 } else { 2 }, Ordering::Release);
        }
    }

    /// A pack abandoned in the middle of a stream lets its sink go at once, unflushed: it was torn.
    #[test]
    fn abandon_drops_the_sink_of_a_torn_stream() {
        let ended = Arc::new(AtomicU8::new(0));
        let mut st = RxState::new(1);
        st.chains.push(Default::default());
        st.chains[0].raw = Some(Box::new(Sink(ended.clone(), false)));
        st.abandon();
        assert_eq!(ended.load(Ordering::Acquire), 2);
    }
}
