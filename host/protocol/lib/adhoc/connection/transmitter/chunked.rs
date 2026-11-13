// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Incremental encoding of chunked streams through transform chains.
//!
//! The wire format is `[length: u16 LE][bytes]` repeated, followed by `0x0000`. Each chunk contains
//! at most [`MAX`] bytes; [`ABORT`] terminates an unsuccessful stream. The transmitter retains
//! stream and stage state at each nesting level and reuses it for later streams.

use crate::lib::adhoc::chain::{CustomTx, Stage};
use crate::lib::adhoc::connection::transmitter::Tx;

/// The largest chunk payload: a length is two bytes, and two of its values are not lengths.
pub const MAX: usize = 0xFFFE;

/// The chunk length that ends a stream as void.
pub const ABORT: u32 = 0xFFFF;

/// A stage of the sending side.
pub(crate) enum TxStage {
    /// A run of built-in stages, in the native library.
    Stages(crate::lib::stages::Tx),
    Custom(Box<dyn CustomTx>),
}

impl TxStage {
    fn new(stage: Stage) -> Self {
        match stage {
            Stage::Stages(run) => TxStage::Stages(crate::lib::stages::Tx::new(run)),
            Stage::Custom(c) => TxStage::Custom((c.tx)(c.args)),
        }
    }

    #[inline]
    fn arm(&mut self) {
        match self {
            TxStage::Stages(s) => s.rearm(),
            TxStage::Custom(c) => c.rearm(),
        }
    }
}

/// A stream of a chain being sent: one level of nesting.
#[derive(Default)]
pub(crate) struct TxChain {
    spec: &'static [Stage],
    pub(crate) stages: Vec<TxStage>,
    /// The body is out: the terminator is due.
    pub(crate) terminator: bool,
    /// A value of the leaf parked by the end of a window of the chain (`mode`): the next window
    /// completes it first.
    pub(crate) leaf_mode: u8,
    /// The leaf of a stream of bytes (`Stream` field, a trimmed one): its source, the bytes taken
    /// of it, its cap `[S(N)]`.
    pub(crate) raw: Option<Box<dyn std::io::Read + Send>>,
    pub(crate) total: u64,
    pub(crate) max: u64,
    /// The leaf of a string: the bytes of it already out.
    pub(crate) str_pos: usize,
    /// Current string fragment supplied by [`TxState::strs`](super::TxState::strs). Returns its
    /// allocation to the supplier after consumption. `on_frag` marks the transition from the field
    /// string to supplied fragments.
    pub(crate) frag: String,
    pub(crate) on_frag: bool,
    /// What is done with the source when the stream ends: armed with the stream.
    pub(crate) complete: Option<crate::lib::adhoc::chain::StreamComplete>,
}

impl TxChain {
    /// The stream ends: the source goes, and the hook armed with the stream, if any, hears whether
    /// it was whole.
    pub(crate) fn end(&mut self, completed: bool) {
        self.raw = None;
        if let Some(h) = self.complete.take() {
            h(completed);
        }
    }

    /// The keys of the stream the chain carries, in place of the connection's ( [`crate::lib::stages::Keys`] ): the chain of a
    /// tunnel is keyed by the tunnel, not by its carrier. Set after [`arm`](Self::arm), which lets them go.
    #[inline]
    pub(crate) fn keys(&mut self, keys: &crate::lib::stages::Keys) {
        for s in &mut self.stages {
            if let TxStage::Stages(s) = s {
                s.keys(keys);
            }
        }
    }

    /// Arms the chain for a new stream of `spec`: the stages are made once and armed again.
    pub(crate) fn arm(&mut self, spec: &'static [Stage]) {
        if self.spec != spec || self.stages.len() != spec.len() {
            self.stages.clear();
            self.stages.extend(spec.iter().map(|s| TxStage::new(*s)));
            self.spec = spec;
        } else {
            for s in &mut self.stages {
                s.arm();
            }
        }
        self.terminator = false;
        self.leaf_mode = crate::lib::adhoc::connection::transmitter::mode::OK;
        self.raw = None;
        self.total = 0;
        self.str_pos = 0;
        self.frag.clear();
        self.on_frag = false;
    }
}

/// The bytes of the chunk body: the outermost stage pulls through the stages from `leaf` — the pack
/// or the source of bytes the chain carries. `true` — the stream is out.
pub(crate) fn pull<L: FnMut(&mut Tx<'_>) -> bool>(dst: &mut Tx<'_>, stages: &mut [TxStage], leaf: &mut L) -> bool {
    match stages.split_last_mut() {
        None => leaf(dst),
        Some((last, inner)) => match last {
            TxStage::Stages(s) => s.get_bytes(dst, |t| pull(t, inner, leaf)),
            TxStage::Custom(c) => c.get_bytes(dst, &mut |t| pull(t, inner, leaf)),
        },
    }
}

/// The leaf of a stream of bytes: the source into the window, as much as it gives. `true` — the
/// source ended; a source that has nothing now (`WouldBlock`) parks the stream.
pub(crate) fn raw_leaf(dst: &mut Tx<'_>, src: &mut Option<Box<dyn std::io::Read + Send>>, total: &mut u64, max: u64) -> bool {
    let Some(r) = src.as_mut() else { return true };
    while dst.byte_ < dst.byte_max {
        let (b, m) = (dst.byte_, dst.byte_max);
        match r.read(&mut dst.buffer[b..m]) {
            Ok(0) => {
                *src = None; // the end: the source is dropped
                return true;
            }
            Ok(n) => {
                *total += n as u64;
                assert!(*total <= max, "Stream would exceed max size of {max} bytes.");
                dst.byte_ += n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(e) => panic!("the source of a stream: {e}"),
        }
    }
    false
}

// receive

/// The source of a stream pack sent on its own: taken once, when its bytes start. A `File` pack
/// knows its length up front.
#[derive(Default)]
pub struct StreamOut {
    src: parking_lot::Mutex<Option<Box<dyn std::io::Read + Send>>>,
    len: u64,
}

impl StreamOut {
    /// A `Stream` pack: the bytes of `src` up to its end.
    pub fn new(src: Box<dyn std::io::Read + Send>) -> Self {
        Self { src: parking_lot::Mutex::new(Some(src)), len: 0 }
    }

    /// A `File` pack: exactly `len` bytes of `src`.
    pub fn file(src: Box<dyn std::io::Read + Send>, len: u64) -> Self {
        Self { src: parking_lot::Mutex::new(Some(src)), len }
    }

    /// The source, once.
    pub fn take(&self) -> Box<dyn std::io::Read + Send> {
        self.src.lock().take().expect("the source of a stream pack is taken once")
    }

    /// The source and its length, once (a `File` pack).
    pub fn take_file(&self) -> (Box<dyn std::io::Read + Send>, u64) {
        (self.take(), self.len)
    }
}
