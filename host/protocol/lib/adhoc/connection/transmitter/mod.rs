// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Incremental packet encoding into caller-provided output buffers.
//!
//! [`TxState`] retains pending numeric values, bit packing, and nested packet slots between calls.
//! [`Tx`] combines this state with the current buffer and dereferences to it. Packet
//! implementations write through [`BytesSrc::get_bytes`].
//!
//! [`TxState::read`] selects packets through [`TxHost`], completes pending output, and runs the
//! encoder until the buffer fills or no packet is available. Numeric encoding state can continue
//! across buffers. Strings resume when the packet re-enters [`Tx::put_string`]; the state stores a
//! position rather than a reference into the packet.

use std::ops::{Deref, DerefMut};

use crate::lib::adhoc::chain::{self, Stage};
use crate::lib::adhoc::connection::transmitter::chunked::TxChain;

/// Reports a field length or dimension above its protocol limit.
///
/// # Panics
///
/// Always panics with the field name, actual length, and limit. Call only after detecting overflow;
/// silently truncating a field would change the decoded value.
#[cold]
#[inline(never)]
#[track_caller]
pub fn overflow(what: &str, len: u64, max: u64) -> ! {
    panic!("{what}: {len}, above the limit of the description {max}")
}

/// What the transmitter does with the next buffer (`mode`).
pub mod mode {
    pub const OK: u8 = 0;
    pub const RETRY: u8 = 1;
    pub const VAL4: u8 = 2;
    pub const VAL8: u8 = 3;
    pub const BITS_BYTES: u8 = 4;
    pub const VARINT: u8 = 5;
    pub const BITS: u8 = 6;
    /// A 16-byte value parked whole: the low half in `u8`, the high one in `u8_`.
    pub const VAL16: u8 = 7;
    /// As `BITS_BYTES`, the value of 16 bytes: the low half in `u8`, the high one in `u8_`.
    pub const BITS_BYTES16: u8 = 8;
}

/// Numeric types whose array representation can be copied directly on little-endian targets.
///
/// # Safety
///
/// Every bit pattern of `size_of::<Self>()` bytes must be a valid value. Values must contain no
/// uninitialized padding, and their memory bytes on little-endian targets must match their wire
/// encoding. The conversion methods must read and write exactly `size_of::<Self>()` bytes.
pub unsafe trait Le: Copy {
    fn to_le_bytes_into(self, dst: &mut [u8]);
    fn from_le_slice(src: &[u8]) -> Self;
}

macro_rules! le {
    ($($t:ty),*) => {$(
        unsafe impl Le for $t {
            #[inline(always)]
            fn to_le_bytes_into(self, dst: &mut [u8]) {
                dst.copy_from_slice(&self.to_le_bytes());
            }
            #[inline(always)]
            fn from_le_slice(src: &[u8]) -> Self {
                <$t>::from_le_bytes(src.try_into().unwrap())
            }
        }
    )*};
}
le!(i16, u16, i32, u32, i64, u64, f32, f64);

/// Incrementally encodes a packet into [`Tx`]. Returns `true` when complete or `false` when the
/// buffer fills; saved state continues in the next output buffer.
pub trait BytesSrc {
    fn get_bytes(&self, dst: &mut Tx<'_>) -> bool;
}

/// A pack in the heap: a field of a pack in a cycle (`A` in `B`, `B` in `A`) - Rust has no type of
/// infinite size.
impl<T: BytesSrc + ?Sized> BytesSrc for Box<T> {
    #[inline(always)]
    fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
        (**self).get_bytes(dst)
    }
}

/// What the application hears of the packs a transmitter sends: `P` — the packs of the connection.
/// Set on the generated transmitter (its `handler`); the transmitter's thread calls it.
pub trait EventsHandler<P: ?Sized>: Send + Sync {
    /// A pack starts: its id is not written yet. A [`BytesCopy`] set in `tx.bytes_copy` now gets
    /// every byte of it; [`TxState::abandon`] now drops it - not a byte of it goes, the next pack
    /// does.
    fn on_serializing(&self, _tx: &mut TxState, _pack: &P) {}
    /// The pack is out.
    fn on_serialized(&self, _tx: &mut TxState, _pack: &P) {}
}

/// The bytes of the pack being sent as the transmitter writes them: to log them, to count them, to
/// keep them. Set in `TxState::bytes_copy` for one pack ([`EventsHandler::on_serializing`]); taken
/// away when the pack is out or dropped.
pub trait BytesCopy: Send + Sync {
    /// The bytes of the pack this write wrote - more follow in the next one.
    fn pass(&self, bytes: &[u8]);
    /// The last bytes of the pack: it is whole.
    fn done(&self, bytes: &[u8]);
    /// The pack was dropped before it was whole: what was copied of it is void.
    fn fail(&self);
}

/// The generated transmitter of a connection: the packs to send.
pub trait TxHost {
    /// Takes the next pack to send (`_OnSerializing`): false — nothing to send.
    fn on_serializing(&mut self) -> bool;
    /// Continues the pack being sent; its state is in the slots of `dst`.
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool;
    /// The pack is out (`_OnSerialized`).
    fn on_serialized(&mut self);
    /// The pack `on_serializing` took is guaranteed: its bytes go into the journal of the
    /// connection.
    fn guaranteed(&self) -> bool {
        false
    }
    /// The pack `on_serializing` took is a frame of a tunnel with a Noise of its own ( `[Noise]` of
    /// its description ): the Noise of the socket does not seal it again ([`TxState::clear`]).
    /// Default: no.
    #[inline(always)]
    fn clear(&mut self) -> bool {
        false
    }
    /// The application hears that the pack `on_serializing` took starts: its id is not written yet,
    /// a [`BytesCopy`] it sets in `tx` copies the pack whole.
    #[inline(always)]
    fn serializing(&mut self, _tx: &mut TxState) {}
    /// The application hears that the pack is out, after `on_serialized`; the pack goes after this.
    #[inline(always)]
    fn serialized(&mut self, _tx: &mut TxState) {}
    /// Where the speed the transmitter gives bytes at is kept, bytes a second ([`TxState::gave`]
    /// measures it once a read that gave bytes): a field of the generated connection. Default:
    /// none, not measured.
    #[inline(always)]
    fn speed(&self) -> Option<&std::sync::atomic::AtomicI64> {
        None
    }
    /// The keys of the Envelope stages of the connection, when it has any: a transmitter made with no socket ( the packs queued
    /// into the journal offline ) takes them here.
    #[inline(always)]
    fn envelope(&self) -> Option<std::sync::Arc<crate::lib::chain::Envelope>> {
        None
    }
    /// The queue is empty ([`on_serializing`](Self::on_serializing) gave nothing): the bytes of
    /// `Pulse_` into `dst` when the line has been silent
    /// ([`External::take_pulse`](crate::lib::adhoc::connection::external::External::take_pulse)).
    /// Returns how many; 0 - nothing to say. Never journaled. Default: nothing.
    #[inline(always)]
    fn pulse(&mut self, _dst: &mut [u8]) -> usize {
        0
    }
    /// The stages of the chain `id` this host runs sending by this connection.
    fn chain(_id: u32) -> &'static [Stage]
    where
        Self: Sized,
    {
        &[]
    }
}

/// One level of the pack being sent: the root pack, or a pack nested in it.
#[derive(Debug, Default, Clone)]
pub struct TxSlot {
    pub state: u32,
    pub fields_nulls: i32,
    pub index0: i32,
    pub index_max0: i32,
    pub index1: i32,
    pub index_max1: i32,
    pub index2: i32,
    pub index_max2: i32,
    pub index3: i32,
    pub index_max3: i32,
    /// The end of the group of 8 items whose nulls byte went out last: its items that are there go
    /// before the next group (`index_fix` of a collection with one null bit an item).
    pub index_fix0: i32,
    pub index_fix1: i32,
    pub index_fix2: i32,
    pub index_fix3: i32,
    /// The keys of a map whose values are null, kept between the info and the sending of those keys
    /// (`tmp`).
    pub tmp: i32,
    /// The entry of a map whose pass skips the entries of the other kind: with a value or with a
    /// null one.
    pub entry: i32,
    /// Where the pass of a set or a map of 1- and 2-byte keys is: a bitset has no `n`-th key at
    /// hand, the next one is a step.
    pub seq: crate::lib::collections::token::Seq,
}

impl TxSlot {
    #[inline(always)]
    pub fn index_max_0(&mut self, max: i32) -> i32 {
        self.index0 = 0;
        self.index_max0 = max;
        max
    }
    #[inline(always)]
    pub fn index_max_1(&mut self, max: i32) -> i32 {
        self.index1 = 0;
        self.index_max1 = max;
        max
    }
    #[inline(always)]
    pub fn index_max_2(&mut self, max: i32) -> i32 {
        self.index2 = 0;
        self.index_max2 = max;
        max
    }
    #[inline(always)]
    pub fn next_index0(&mut self) -> bool {
        self.index0 += 1;
        self.index0 < self.index_max0
    }
    #[inline(always)]
    pub fn next_index1(&mut self) -> bool {
        self.index1 += 1;
        self.index1 < self.index_max1
    }
    #[inline(always)]
    pub fn next_index2(&mut self) -> bool {
        self.index2 += 1;
        self.index2 < self.index_max2
    }
    #[inline(always)]
    pub fn next_index3(&mut self) -> bool {
        self.index3 += 1;
        self.index3 < self.index_max3
    }
    /// The next item of the group whose nulls byte went out (`next_index_fix`).
    #[inline(always)]
    pub fn next_index_fix0(&mut self) -> bool {
        self.index0 += 1;
        self.index0 < self.index_fix0
    }
    #[inline(always)]
    pub fn next_index_fix1(&mut self) -> bool {
        self.index1 += 1;
        self.index1 < self.index_fix1
    }
    #[inline(always)]
    pub fn next_index_fix2(&mut self) -> bool {
        self.index2 += 1;
        self.index2 < self.index_fix2
    }
    #[inline(always)]
    pub fn next_index_fix3(&mut self) -> bool {
        self.index3 += 1;
        self.index3 < self.index_fix3
    }
}

/// What a transmitter keeps from buffer to buffer.
pub struct TxState {
    pub mode: u8,
    pub u4: u32,
    pub u8: u64,
    pub u8_: u64,
    pub bytes_left: i32,
    pub bits: u32,
    pub bit: i32,
    bits_byte: usize,
    bits_transaction_bytes: usize,
    bits8_at: usize,
    // a string the buffer cut: 0 none, 1 its length is going out, 2 its characters
    str_phase: u8,
    str_pos: usize,
    str_low: u16,
    str_partial: bool,
    /// The ASCII head of the string being sent, found when its units were counted: copied with no
    /// second look.
    str_ascii: usize,
    // a file field the buffer cut (`put_file`): 0 none, 1 its length is going out, 2 its bytes; the
    // source and the bytes left
    file_phase: u8,
    file_src: Option<Box<dyn std::io::Read + Send>>,
    file_rem: u64,
    slots: Vec<TxSlot>,
    level: usize,
    depth: usize,
    in_pack: bool,
    /// The stages of the chains of the connection by their numbers ([`TxHost::chain`]).
    stages: fn(u32) -> &'static [Stage],
    /// The streams of chains in flight, by the level of the pack each one carries.
    chains: Vec<TxChain>,
    /// The pack being sent is guaranteed ([`TxHost::guaranteed`]): its bytes are for the journal.
    pub guaranteed: bool,
    /// The pack being sent has nothing to give now ([`Tx::hold`]): the transmitter is to be woken,
    /// not given more room.
    pub held: bool,
    /// The pack being sent is a frame of a tunnel with a Noise of its own ([`TxHost::clear`]): so
    /// are the bytes of the last read - a read gives bytes of one kind.
    pub clear: bool,
    /// The pack taken last is of the other kind than the bytes before it: it goes from the next
    /// read on, its kind with it.
    switch: bool,
    /// What the buffer of [`read_one`](Self::read_one) holds before it, of the packs before: -1
    /// nothing, 0 sealed bytes, 1 frames in the clear - a pack of the other kind waits.
    pub(crate) before: i8,
    /// The packs go into the journal only - the session is parked: one of no guarantee is not sent,
    /// the next `on_serializing` gives its place back.
    pub journal_only: bool,
    /// The keys of the session: the ChaCha20 stage of a pack chain takes the newest.
    pub keys: std::sync::Arc<crate::lib::adhoc::connection::keyring::Keyring>,
    /// The keys of the Envelope stages of the connection ( [`crate::lib::chain::Envelope`] ): what the packs it sends are sealed and signed with.
    pub envelope: Option<std::sync::Arc<crate::lib::chain::Envelope>>,
    /// Gets the bytes of the pack being sent: set for one pack, in
    /// [`EventsHandler::on_serializing`]; taken away when the pack is out ([`BytesCopy::done`]) or
    /// dropped ([`BytesCopy::fail`]).
    pub bytes_copy: Option<std::sync::Arc<dyn BytesCopy>>,
    /// The hook for the stream about to start ([`Tx::on_stream_complete`]): the stream takes it.
    pub(crate) stream_complete: Option<crate::lib::adhoc::chain::StreamComplete>,
    /// Optional supplier of further fragments for a string chain leaf.
    ///
    /// Each call receives the previous fragment buffer, cleared with capacity retained, and returns
    /// the next owned UTF-8 fragment or `None`. Fragments follow the field string without length
    /// prefixes or separators and decode as one string. The supplier is used only by string chain
    /// leaves and is dropped on completion or abandonment.
    pub strs: Option<Box<dyn FnMut(String) -> Option<String> + Send>>,
    /// The bytes given and the clock since the speed was measured last ([`TxHost::speed`]).
    speed_bytes: i64,
    speed_at: i64,
    /// The queue was empty: the next bytes start the measure.
    speed_dry: bool,
    /// The queue was found empty in the read [`gave`](Self::gave) follows: after it, the measure
    /// starts again.
    dried: bool,
}

/// A pack the copy follows dropped with the transmitter: the copy hears it.
impl Drop for TxState {
    fn drop(&mut self) {
        if let Some(c) = self.bytes_copy.take() {
            c.fail();
        }
        for c in &mut self.chains {
            if c.complete.is_some() {
                c.end(false); // a stream torn with the transmitter
            }
        }
    }
}

impl Default for TxState {
    fn default() -> Self {
        Self::new()
    }
}

/// The pack in flight, level by level: a line a level, indented by its depth, with the state of its
/// slot - `<` marks the level being sent; then the mode and the flags of the pack. Empty between
/// packs.
impl std::fmt::Debug for TxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.in_pack {
            return Ok(());
        }
        for (i, s) in self.slots[..self.depth.min(self.slots.len())].iter().enumerate() {
            writeln!(f, "{:\t<i$}level {i}\t{}{}", "", s.state, if i == self.level { " <" } else { "" })?;
        }
        write!(f, "mode {} bytes_left {}", self.mode, self.bytes_left)?;
        for (on, what) in [(self.guaranteed, " guaranteed"), (self.held, " held"), (self.journal_only, " journal_only")] {
            if on {
                f.write_str(what)?;
            }
        }
        Ok(())
    }
}

impl TxState {
    pub fn new() -> Self {
        Self {
            mode: mode::OK,
            u4: 0,
            u8: 0,
            u8_: 0,
            bytes_left: 0,
            bits: 0,
            bit: 0,
            bits_byte: 0,
            bits_transaction_bytes: 0,
            bits8_at: 0,
            str_phase: 0,
            str_pos: 0,
            str_low: 0,
            str_partial: false,
            str_ascii: 0,
            file_phase: 0,
            file_src: None,
            file_rem: 0,
            slots: vec![TxSlot::default()],
            level: 0,
            depth: 1,
            in_pack: false,
            stages: chain::no_stages,
            chains: Vec::new(),
            guaranteed: false,
            held: false,
            clear: false,
            switch: false,
            before: -1,
            journal_only: false,
            keys: Default::default(),
            envelope: None,
            bytes_copy: None,
            stream_complete: None,
            strs: None,
            speed_bytes: 0,
            speed_at: 0,
            speed_dry: true,
            dried: false,
        }
    }

    /// The speed the bytes are given at, into the field the host names ([`TxHost::speed`]): the
    /// bytes the transport took since the last measure, over the time since then - measured once 10
    /// ms at least went by. A wait for the transport or the peer is counted, the time the queue is
    /// empty is not. Not averaged: the speed now. The clock of the loop pass; `n` is
    /// what the read returns, returned.
    #[inline]
    pub fn gave(&mut self, n: isize, speed: Option<&std::sync::atomic::AtomicI64>) -> isize {
        if let (true, Some(speed)) = (0 < n, speed) {
            let now = crate::lib::network::engine::now_nanos(); // the clock of the loop pass: no reading a read
            if self.speed_dry {
                // the bytes of the first read after a pause are given at once: the measure starts
                // behind them
                self.speed_dry = false;
                self.speed_at = now;
                self.speed_bytes = 0;
            } else {
                self.speed_bytes += n as i64;
                let t = now - self.speed_at;
                if 10_000_000 <= t {
                    // what went since the last measure, over the time it took - waits of the
                    // transport and of the peer too
                    speed.store(self.speed_bytes * 1_000_000_000 / t, std::sync::atomic::Ordering::Relaxed);
                    self.speed_at = now;
                    self.speed_bytes = 0;
                }
            }
        }
        if self.dried {
            self.dried = false;
            self.speed_dry = true; // the next bytes start a new measure
        }
        n
    }

    /// A pack is being sent (`isOpen`).
    #[inline]
    pub fn is_open(&self) -> bool {
        self.in_pack
    }

    /// Drops the pack in flight (`Abandon` / `drop`).
    pub fn abandon(&mut self) {
        self.in_pack = false;
        self.switch = false;
        self.mode = mode::OK;
        self.u4 = 0;
        self.bytes_left = 0;
        self.str_phase = 0;
        self.str_partial = false;
        self.str_low = 0;
        self.level = 0;
        self.depth = 1;
        // the sources of the streams and the file in flight go now
        self.file_phase = 0;
        self.file_src = None;
        for c in &mut self.chains {
            c.end(false); // the hook of a stream torn hears it
        }
        self.stream_complete = None;
        self.strs = None;
        if let Some(c) = self.bytes_copy.take() {
            c.fail();
        }
    }

    /// Encodes ZigZag (`zig_zag`).
    #[inline(always)]
    pub fn zig_zag(src: i64, right: u32) -> u64 {
        ((src << 1) ^ (src >> right)) as u64
    }

    /// `Transmitter.Read`: serializes packs into `dst` until it is full or nothing is left. Returns
    /// the bytes written; 0 — a pack waits for room; -1 — nothing to send.
    pub fn read<H: TxHost>(&mut self, dst: &mut [u8], host: &mut H) -> isize {
        self.stages = H::chain;
        self.held = false;
        if self.switch {
            self.switch = false;
            self.clear = !self.clear; // the pack taken last starts here
        }
        let max = dst.len();
        let mut tx = Tx {
            buffer: dst,
            byte_: 0,
            byte_max: max,
            st: self,
        };
        let mut copy_from = 0; // where the bytes of the pack being copied begin in this buffer
        while tx.byte_ < tx.byte_max {
            if !tx.st.in_pack {
                if !host.on_serializing() {
                    tx.st.dried = true;
                    if tx.st.clear {
                        if tx.byte_ != 0 {
                            break; // Pulse_ is sealed: not behind frames in the clear
                        }
                        tx.st.clear = false;
                    }
                    tx.byte_ += host.pulse(&mut tx.buffer[tx.byte_..tx.byte_max]); // the queue is empty: the line has been silent
                    break;
                }
                tx.st.in_pack = true;
                tx.st.guaranteed = host.guaranteed();
                tx.st.slots[0].state = 0;
                tx.st.level = 0;
                tx.st.depth = 1;
                tx.st.mode = mode::OK;
                copy_from = tx.byte_;
                host.serializing(tx.st);
                if !tx.st.in_pack {
                    continue; // the handler dropped it: its place goes with the next pack taken
                }
                if host.clear() != tx.st.clear {
                    if tx.byte_ != 0 {
                        tx.st.switch = true; // the bytes before are of the other kind: the pack goes in the next read
                        break;
                    }
                    tx.st.clear = !tx.st.clear;
                }
            } else if !tx.resume() {
                break;
            }
            tx.st.mode = mode::OK;
            tx.st.level = 0;
            if !host.get_bytes(&mut tx) {
                break;
            }
            tx.st.in_pack = false;
            if let Some(c) = tx.st.bytes_copy.take() {
                c.done(&tx.buffer[copy_from..tx.byte_]);
            }
            host.on_serialized();
            host.serialized(tx.st);
        }
        if tx.st.in_pack {
            if let Some(c) = &tx.st.bytes_copy {
                c.pass(&tx.buffer[copy_from..tx.byte_]);
            }
        }
        let n = tx.byte_;
        if 0 < n {
            n as isize
        } else if self.in_pack {
            0
        } else {
            -1
        }
    }

    /// As [`read`](Self::read), but stops behind the end of a pack: the framing puts the CRC there.
    /// Returns the bytes written, whether a pack started in this call, and whether one ended — then
    /// it ends the written bytes. The bytes are -1 when nothing is to be sent.
    pub fn read_one<H: TxHost>(&mut self, dst: &mut [u8], host: &mut H) -> (isize, bool, bool) {
        self.stages = H::chain;
        self.held = false;
        if self.switch {
            self.switch = false;
            self.clear = !self.clear; // the pack taken last starts here
        }
        let max = dst.len();
        let mut started = false;
        let mut tx = Tx {
            buffer: dst,
            byte_: 0,
            byte_max: max,
            st: self,
        };
        if tx.byte_ < tx.byte_max {
            if !tx.st.in_pack {
                while !tx.st.in_pack {
                    loop {
                        if !host.on_serializing() {
                            tx.st.dried = true;
                            if !tx.st.journal_only && tx.st.before != 1 {
                                // the queue is empty: the line has been silent - a pack of its own, of no
                                // guarantee, sealed: not behind frames in the clear
                                let n = host.pulse(tx.buffer);
                                if 0 < n {
                                    tx.st.clear = false;
                                    tx.st.guaranteed = false;
                                    return (n as isize, true, true);
                                }
                            }
                            return (-1, false, false);
                        }
                        if !tx.st.journal_only || host.guaranteed() {
                            break;
                        }
                    }
                    tx.st.in_pack = true;
                    tx.st.guaranteed = host.guaranteed();
                    tx.st.slots[0].state = 0;
                    tx.st.level = 0;
                    tx.st.depth = 1;
                    tx.st.mode = mode::OK;
                    host.serializing(tx.st); // the handler may drop it (`abandon`): the next one
                }
                started = true;
                let clear = host.clear();
                if 0 <= tx.st.before && (tx.st.before == 1) != clear {
                    tx.st.switch = clear != tx.st.clear; // the bytes before it are of the other kind: the pack goes from the next buffer on
                    return (0, true, false);
                }
                tx.st.clear = clear;
            } else if !tx.resume() {
                return (tx.byte_ as isize, false, false);
            }
            tx.st.mode = mode::OK;
            tx.st.level = 0;
            if host.get_bytes(&mut tx) {
                tx.st.in_pack = false;
                let n = tx.byte_ as isize;
                if let Some(c) = tx.st.bytes_copy.take() {
                    c.done(&tx.buffer[..tx.byte_]); // one pack a call: from the start of the buffer
                }
                host.on_serialized();
                host.serialized(tx.st);
                return (n, started, true);
            }
        }
        if let Some(c) = &tx.st.bytes_copy {
            c.pass(&tx.buffer[..tx.byte_]);
        }
        (tx.byte_ as isize, started, false)
    }
}

impl Tx<'_> {
    /// Completes the value the previous buffer parked: false — this buffer has no room for it
    /// either.
    fn resume(&mut self) -> bool {
        match self.st.mode {
            mode::VAL4 => {
                if self.remaining() < self.st.bytes_left as usize {
                    return false;
                }
                let (v, b) = (self.st.u4, self.st.bytes_left);
                self.put_val4(v, b as usize);
            }
            mode::VAL8 => {
                if self.remaining() < self.st.bytes_left as usize {
                    return false;
                }
                let (v, b) = (self.st.u8, self.st.bytes_left);
                self.put_val8(v, b as usize);
            }
            mode::VAL16 => {
                if self.remaining() < 16 {
                    return false;
                }
                let (lo, hi) = (self.st.u8, self.st.u8_);
                self.put_uint128((hi as u128) << 64 | lo as u128);
            }
            mode::BITS_BYTES => {
                if self.remaining() < self.st.bits_transaction_bytes {
                    return false;
                }
                self.st.bits_byte = self.byte_;
                self.byte_ += 1;
                let (v, b) = (self.st.u8, self.st.bytes_left);
                self.put_val8(v, b as usize);
            }
            mode::BITS_BYTES16 => {
                if self.remaining() < self.st.bits_transaction_bytes {
                    return false;
                }
                self.st.bits_byte = self.byte_;
                self.byte_ += 1;
                let (lo, hi) = (self.st.u8, self.st.u8_);
                self.put_uint128((hi as u128) << 64 | lo as u128);
            }
            mode::VARINT => {
                let v = self.st.u8_;
                if !self.varint(v) {
                    return false;
                }
            }
            mode::BITS => {
                if self.remaining() < self.st.bits_transaction_bytes {
                    return false;
                }
                self.st.bits_byte = self.byte_;
                self.byte_ += 1;
            }
            _ => {}
        }
        true
    }
}

/// A transmitter over one buffer.
pub struct Tx<'a> {
    pub buffer: &'a mut [u8],
    pub byte_: usize,
    pub byte_max: usize,
    st: &'a mut TxState,
}

impl Deref for Tx<'_> {
    type Target = TxState;
    #[inline(always)]
    fn deref(&self) -> &TxState {
        self.st
    }
}

impl DerefMut for Tx<'_> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut TxState {
        self.st
    }
}

macro_rules! put_fixed {
    ($put:ident, $put_c:ident, $t:ty, $bytes:expr, $val:ident) => {
        /// Writes the value; the room is allocated already.
        #[inline(always)]
        pub fn $put(&mut self, src: $t) {
            self.put_raw(src.to_le_bytes());
        }

        /// Writes the value, or parks it whole for the next buffer and goes on at `next_case`.
        #[inline(always)]
        pub fn $put_c(&mut self, src: $t, next_case: u32) -> bool {
            if $bytes <= self.remaining() {
                self.$put(src);
                return true;
            }
            self.$val(src as _, $bytes, next_case);
            false
        }
    };
}

impl<'a> Tx<'a> {
    /// A transmitter of `st` over one `buffer`, the pack all in it; a pack over buffers one after
    /// another - [`Virtual`].
    pub fn new(buffer: &'a mut [u8], st: &'a mut TxState) -> Self {
        let n = buffer.len();
        Self { buffer, byte_: 0, byte_max: n, st }
    }

    #[inline(always)]
    pub fn remaining(&self) -> usize {
        self.byte_max - self.byte_
    }

    #[inline(always)]
    pub fn slot(&mut self) -> &mut TxSlot {
        let l = self.st.level;
        // SAFETY: `level < slots.len()` - `put_bytes` grows `slots` before it goes down a level.
        unsafe { self.st.slots.get_unchecked_mut(l) }
    }

    /// The outer index of the level (`slot().index0`) through a shared borrow: an item of a
    /// multi-dimensional field is taken with it inside the arguments of a call on the same
    /// transmitter / receiver.
    #[inline(always)]
    pub fn index0(&self) -> usize {
        // SAFETY: as in `slot`
        unsafe { self.st.slots.get_unchecked(self.st.level).index0 as usize }
    }

    #[inline(always)]
    pub fn state(&self) -> u32 {
        // SAFETY: as in `slot`
        unsafe { self.st.slots.get_unchecked(self.st.level).state }
    }

    #[inline(always)]
    pub fn set_state(&mut self, state: u32) {
        self.slot().state = state;
    }

    #[inline(always)]
    pub fn retry_at(&mut self, the_case: u32) {
        self.slot().state = the_case;
        self.st.mode = mode::RETRY;
    }

    /// The pack has nothing to give now (a relay waiting for its inbound side): the transmitter
    /// stops here, and whoever has the bytes wakes it.
    #[inline]
    pub fn hold(&mut self) {
        self.st.held = true;
        self.st.mode = mode::RETRY;
    }

    /// `bytes` of room, or the retry at `this_case` (`Allocate`).
    #[inline(always)]
    pub fn allocate(&mut self, bytes: usize, this_case: u32) -> bool {
        self.slot().state = this_case;
        if bytes <= self.remaining() {
            return true;
        }
        self.st.mode = mode::RETRY;
        false
    }

    // window

    /// The room of the buffer from the cursor, up to `bytes`, to write a one-byte array in place:
    /// closed with [`window_done`](Self::window_done).
    #[inline]
    pub fn window(&mut self, bytes: usize) -> &mut [u8] {
        let n = bytes.min(self.byte_max - self.byte_);
        &mut self.buffer[self.byte_..self.byte_ + n]
    }

    /// `written` bytes of the window are written: the cursor past them; fewer than `asked` - the
    /// buffer is full, the retry at `retry_case`.
    #[inline]
    pub fn window_done(&mut self, written: usize, asked: usize, retry_case: u32) -> usize {
        self.byte_ += written;
        if written < asked {
            self.retry_at(retry_case);
        }
        written
    }

    // nested

    /// [`put_bytes`](Self::put_bytes) with `u8` set to `src` first: the value the nested pack
    /// serializes.
    #[inline]
    pub fn put_bytes_u8<S: BytesSrc + ?Sized>(&mut self, src: u64, handler: &S, this_case: u32) -> bool {
        self.st.u8 = src;
        self.put_bytes(handler, this_case)
    }

    /// A pack nested in the one being sent: one level down. `this_case` — the case of the pack that
    /// calls it. A nested pack starts at case 1: its id (case 0) goes only with a pack sent on its
    /// own.
    pub fn put_bytes<S: BytesSrc + ?Sized>(&mut self, src: &S, this_case: u32) -> bool {
        let parent = self.st.level;
        let level = parent + 1;
        if self.st.depth <= level {
            if self.st.slots.len() <= level {
                self.st.slots.push(TxSlot::default());
            }
            self.st.slots[level].state = 1;
            self.st.depth = level + 1;
        }
        self.st.level = level;
        if src.get_bytes(self) {
            self.st.level = parent;
            self.st.depth = level;
            return true;
        }
        self.st.slots[parent].state = this_case;
        false
    }

    // chains

    /// The same transmitter over another buffer: a stage of a chain takes the bytes of the stages
    /// nearer the leaf there.
    #[inline]
    pub fn sub<'b>(&'b mut self, buffer: &'b mut [u8]) -> Tx<'b> {
        let n = buffer.len();
        Tx {
            buffer,
            byte_: 0,
            byte_max: n,
            st: &mut *self.st,
        } // the stage writes what it is given
    }

    // nulls

    /// The byte of the null fields: room for it and its first bits (`init_fields_nulls`).
    #[inline]
    pub fn init_fields_nulls(&mut self, field0_bit: i32, this_case: u32) -> bool {
        if !self.allocate(1, this_case) {
            return false;
        }
        self.slot().fields_nulls = field0_bit;
        true
    }

    #[inline(always)]
    pub fn set_fields_nulls(&mut self, field: i32) {
        self.slot().fields_nulls |= field;
    }

    #[inline(always)]
    pub fn flush_fields_nulls(&mut self) {
        let b = self.slot().fields_nulls as u8;
        self.put_byte(b);
    }

    #[inline(always)]
    pub fn is_null(&mut self, field: i32) -> bool {
        self.slot().fields_nulls & field == 0
    }

    // values

    #[cold]
    #[inline(never)]
    fn park4(&mut self, src: u32, bytes: usize, next_case: u32) {
        self.slot().state = next_case;
        self.st.bytes_left = bytes as i32;
        self.st.u4 = src;
        self.st.mode = mode::VAL4;
    }

    #[cold]
    #[inline(never)]
    fn park8(&mut self, src: u64, bytes: usize, next_case: u32) {
        self.slot().state = next_case;
        self.st.bytes_left = bytes as i32;
        self.st.u8 = src;
        self.st.mode = mode::VAL8;
    }

    /// `Int128` / `UInt128` parked whole: 16 bytes, little-endian, the low 8 first.
    #[cold]
    #[inline(never)]
    fn park16(&mut self, src: u128, bytes: usize, next_case: u32) {
        self.slot().state = next_case;
        self.st.bytes_left = bytes as i32;
        self.st.u8 = src as u64;
        self.st.u8_ = (src >> 64) as u64;
        self.st.mode = mode::VAL16;
    }

    #[inline(always)]
    pub fn put_bool(&mut self, src: bool) {
        self.put_byte(src as u8);
    }
    #[inline(always)]
    pub fn put_bool_c(&mut self, src: bool, next_case: u32) -> bool {
        self.put_byte_c(src as u8, next_case)
    }
    #[inline(always)]
    pub fn put_sbyte(&mut self, src: i8) {
        self.put_byte(src as u8);
    }
    #[inline(always)]
    pub fn put_sbyte_c(&mut self, src: i8, next_case: u32) -> bool {
        self.put_byte_c(src as u8, next_case)
    }
    /// The bytes of a value whose room is made already ( `allocate`, `remaining` ): no second check.
    #[inline(always)]
    fn put_raw<const N: usize>(&mut self, b: [u8; N]) {
        debug_assert!(self.byte_ + N <= self.byte_max && self.byte_max <= self.buffer.len());
        // SAFETY: the caller made the room: `byte_ + N <= byte_max`; `byte_max` is never past the
        // end of `buffer` - it is set from its length ( `new`, `sub`, `read` ) and only narrowed
        // ( a stream, a tunnel )
        unsafe { (self.buffer.as_mut_ptr().add(self.byte_) as *mut [u8; N]).write_unaligned(b) };
        self.byte_ += N;
    }

    #[inline(always)]
    pub fn put_byte(&mut self, src: u8) {
        self.put_raw([src]);
    }
    #[inline(always)]
    pub fn put_byte_c(&mut self, src: u8, next_case: u32) -> bool {
        if self.byte_ < self.byte_max {
            self.put_byte(src);
            return true;
        }
        self.park4(src as u32, 1, next_case);
        false
    }

    put_fixed!(put_ushort, put_ushort_c, u16, 2, park4);
    put_fixed!(put_short, put_short_c, i16, 2, park4_i16);
    put_fixed!(put_uint, put_uint_c, u32, 4, park4);
    put_fixed!(put_int, put_int_c, i32, 4, park4_i32);
    put_fixed!(put_ulong, put_ulong_c, u64, 8, park8);
    put_fixed!(put_long, put_long_c, i64, 8, park8_i64);
    put_fixed!(put_uint128, put_uint128_c, u128, 16, park16);
    put_fixed!(put_int128, put_int128_c, i128, 16, park16_i128);

    #[inline(always)]
    pub fn put_char(&mut self, src: u16) {
        self.put_ushort(src);
    }
    #[inline(always)]
    pub fn put_char_c(&mut self, src: u16, next_case: u32) -> bool {
        self.put_ushort_c(src, next_case)
    }
    #[inline(always)]
    pub fn put_float(&mut self, src: f32) {
        self.put_uint(src.to_bits());
    }
    #[inline(always)]
    pub fn put_float_c(&mut self, src: f32, next_case: u32) -> bool {
        self.put_uint_c(src.to_bits(), next_case)
    }
    #[inline(always)]
    pub fn put_double(&mut self, src: f64) {
        self.put_ulong(src.to_bits());
    }
    #[inline(always)]
    pub fn put_double_c(&mut self, src: f64, next_case: u32) -> bool {
        self.put_ulong_c(src.to_bits(), next_case)
    }

    fn park4_i16(&mut self, src: i16, bytes: usize, next_case: u32) {
        self.park4(src as u16 as u32, bytes, next_case)
    }
    fn park4_i32(&mut self, src: i32, bytes: usize, next_case: u32) {
        self.park4(src as u32, bytes, next_case)
    }
    fn park8_i64(&mut self, src: i64, bytes: usize, next_case: u32) {
        self.park8(src as u64, bytes, next_case)
    }
    fn park16_i128(&mut self, src: i128, bytes: usize, next_case: u32) {
        self.park16(src as u128, bytes, next_case)
    }

    /// `bytes` (1..=4) of `src` little-endian; the room is there.
    #[inline(always)]
    pub fn put_val4(&mut self, src: u32, bytes: usize) {
        self.put_val8(src as u64, bytes)
    }

    /// `bytes` (1..=8) of `src` little-endian; the room is there.
    #[inline(always)]
    pub fn put_val8(&mut self, src: u64, bytes: usize) {
        let at = self.byte_;
        debug_assert!(at + bytes <= self.byte_max && self.byte_max <= self.buffer.len() && bytes <= 8);
        let w = src.to_le_bytes();
        // SAFETY: the room of `bytes` is made ( `allocate`, `remaining` ), `byte_max` is in `buffer`
        // ( `put_raw` ); the whole word only when it fits before `byte_max` - the bytes past `bytes`
        // land in the free part of the window and are not counted
        unsafe {
            let p = self.buffer.as_mut_ptr().add(at);
            if at + 8 <= self.byte_max {
                (p as *mut [u8; 8]).write_unaligned(w);
            } else {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, bytes);
            }
        }
        self.byte_ = at + bytes;
    }

    /// `bytes` (1..=8) of `src`, or the whole value parked (`put_val(ulong, int, uint)`).
    #[inline(always)]
    pub fn put_val(&mut self, src: u64, bytes: usize, next_field_case: u32) -> bool {
        if self.remaining() < bytes {
            self.park8(src, bytes, next_field_case);
            return false;
        }
        self.put_val8(src, bytes);
        true
    }

    // bytes

    /// The items of an array of numbers, little-endian: one copy on a little-endian machine
    /// (`put_short` … in a loop). The caller has made the room: `size_of_val(src) <= remaining()`.
    #[inline]
    pub fn put_le<T: Le>(&mut self, src: &[T]) {
        let n = std::mem::size_of_val(src);
        let dst = &mut self.buffer[self.byte_..self.byte_ + n];
        #[cfg(target_endian = "little")]
        // SAFETY: `Le` - the bytes of the values are their little-endian encoding; `dst` is `n`
        // bytes, apart from `src`.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst.as_mut_ptr(), n)
        };
        #[cfg(not(target_endian = "little"))]
        for (d, v) in dst.chunks_exact_mut(std::mem::size_of::<T>()).zip(src) {
            v.to_le_bytes_into(d);
        }
        self.byte_ += n;
    }

    /// As many bytes of `src` as fit; fewer — the retry at `retry_case`
    /// (`put(byte[], int, int, uint)`).
    pub fn put_slice(&mut self, src: &[u8], retry_case: u32) -> usize {
        let mut n = src.len();
        if self.remaining() < n {
            n = self.remaining();
            self.retry_at(retry_case);
        }
        self.buffer[self.byte_..self.byte_ + n].copy_from_slice(&src[..n]);
        self.byte_ += n;
        n
    }

    // file

    // varint

    /// A varint, or the rest of it parked (`put_varint`).
    #[inline]
    pub fn put_varint(&mut self, src: u64, next_case: u32) -> bool {
        if self.varint(src) {
            return true;
        }
        self.slot().state = next_case;
        self.st.mode = mode::VARINT;
        false
    }

    /// Writes the varint of `src` as far as the buffer goes; false — the rest is in `u8_`.
    #[inline]
    pub fn varint(&mut self, mut src: u64) -> bool {
        let mut pos = self.byte_;
        let max = self.byte_max;
        if src < 0x80 && pos < max {
            // SAFETY: `pos < byte_max <= buffer.len()` ( `put_raw` )
            unsafe { *self.buffer.get_unchecked_mut(pos) = src as u8 }; // the common case: one byte
            self.byte_ = pos + 1;
            return true;
        }
        if src < 1 << 56 && 8 <= max - pos {
            // up to 8 bytes of the varint in one register, one store: the 7-bit groups spread to
            // bytes without a loop
            let n = ((64 - src.leading_zeros() as usize) + 6) / 7;
            let mut x = src;
            x = (x & 0x0FFF_FFFF) | ((x << 4) & 0x0FFF_FFFF_0000_0000);
            x = (x & 0x0000_3FFF_0000_3FFF) | ((x << 2) & 0x3FFF_0000_3FFF_0000);
            x = (x & 0x007F_007F_007F_007F) | ((x << 1) & 0x7F00_7F00_7F00_7F00);
            x |= 0x8080_8080_8080_8080 & ((1u64 << (8 * (n - 1))) - 1); // the continuation bits of all but the last byte
                                                                        // SAFETY: `pos + 8 <= byte_max <= buffer.len()` ( `put_raw` )
            unsafe { (self.buffer.as_mut_ptr().add(pos) as *mut [u8; 8]).write_unaligned(x.to_le_bytes()) };
            self.byte_ = pos + n;
            return true;
        }
        while pos < max {
            if src < 0x80 {
                self.buffer[pos] = src as u8;
                self.byte_ = pos + 1;
                return true;
            }
            self.buffer[pos] = 0x80 | src as u8;
            pos += 1;
            src >>= 7;
        }
        self.byte_ = pos;
        self.st.u8_ = src;
        false
    }

    // bits

    /// A bit transaction: `transaction_bytes` of room, its first byte reserved (`init_bits`).
    #[inline(always)]
    pub fn init_bits(&mut self, transaction_bytes: usize, this_case: u32) -> bool {
        self.st.bits_transaction_bytes = transaction_bytes;
        if self.remaining() < transaction_bytes {
            self.slot().state = this_case;
            self.st.mode = mode::RETRY;
            return false;
        }
        self.st.bits = 0;
        self.st.bit = 0;
        self.st.bits_byte = self.byte_;
        self.byte_ += 1;
        true
    }

    /// A bit transaction going on after a retry (`init_bits_`).
    pub fn init_bits_(&mut self, transaction_bytes: usize, this_case: u32) -> bool {
        self.st.bits_transaction_bytes = transaction_bytes;
        if transaction_bytes <= self.remaining() {
            return true;
        }
        self.slot().state = this_case;
        self.byte_ = self.st.bits_byte;
        self.st.mode = mode::BITS;
        false
    }

    /// `len_bits` of `src`; the room of the transaction is there (`put_bits(int, int)`).
    #[inline(always)]
    pub fn put_bits(&mut self, src: u32, len_bits: i32) {
        let st = &mut *self.st;
        st.bits |= src << st.bit;
        st.bit += len_bits;
        if st.bit < 9 {
            return;
        }
        self.buffer[st.bits_byte] = st.bits as u8;
        st.bits >>= 8;
        st.bit -= 8;
        st.bits_byte = self.byte_;
        if self.byte_ < self.byte_max {
            self.byte_ += 1;
        }
    }

    /// `len_bits` of `src`; false — the transaction has no room left, it goes on at
    /// `continue_at_case`.
    #[inline(always)]
    pub fn put_bits_c(&mut self, src: u32, len_bits: i32, continue_at_case: u32) -> bool {
        let st = &mut *self.st;
        st.bits |= src << st.bit;
        st.bit += len_bits;
        if st.bit < 9 {
            return true;
        }
        self.buffer[st.bits_byte] = st.bits as u8;
        st.bits >>= 8;
        st.bit -= 8;
        if self.byte_max - self.byte_ < st.bits_transaction_bytes {
            self.slot().state = continue_at_case;
            return false;
        }
        self.st.bits_byte = self.byte_;
        self.byte_ += 1;
        true
    }

    /// What is done with the source of the stream about to start: `hook(true)` when it is out
    /// whole, `hook(false)` when it is torn - the source is dropped either way. The code that gives
    /// the source calls it right before; the stream takes it once.
    #[inline]
    pub fn on_stream_complete(&mut self, hook: crate::lib::adhoc::chain::StreamComplete) {
        self.st.stream_complete = Some(hook);
    }

    #[inline(always)]
    pub fn put_bool_bit(&mut self, src: bool) {
        self.put_bits(src as u32, 1);
    }

    /// A bool in one bit; false — the transaction has no room left, it goes on at
    /// `continue_at_case` (`put_bits(bool, uint)`).
    #[inline(always)]
    pub fn put_bool_bit_c(&mut self, src: bool, continue_at_case: u32) -> bool {
        self.put_bits_c(src as u32, 1, continue_at_case)
    }

    /// A nullable bool in two bits: 00 null, 10 false, 11 true.
    #[inline(always)]
    pub fn put_nullable_bool_bits(&mut self, src: Option<bool>) {
        self.put_bits(
            match src {
                None => 0,
                Some(false) => 2,
                Some(true) => 3,
            },
            2,
        );
    }

    /// Ends the bit transaction (`end_bits`).
    #[inline(always)]
    pub fn end_bits(&mut self) {
        if 0 < self.st.bit {
            self.buffer[self.st.bits_byte] = self.st.bits as u8;
        } else {
            self.byte_ = self.st.bits_byte;
        }
    }

    pub fn put_nulls(&mut self, nulls: u32, nulls_bits: i32, continue_at_case: u32) -> bool {
        if self.put_bits_c(nulls, nulls_bits, continue_at_case) {
            return true;
        }
        self.st.mode = mode::BITS;
        false
    }

    pub fn continue_bits_at(&mut self, continue_at_case: u32) {
        self.slot().state = continue_at_case;
        self.byte_ = self.st.bits_byte;
        self.st.mode = mode::BITS;
    }

    /// `info_bits` of `info`, then `value_bytes` of `value` (`put_bits_bytes`).
    pub fn put_bits_bytes(&mut self, info: u32, info_bits: i32, value: u64, value_bytes: usize, continue_at_case: u32) -> bool {
        if self.put_bits_c(info, info_bits, continue_at_case) {
            self.put_val8(value, value_bytes);
            return true;
        }
        self.st.u8 = value;
        self.st.bytes_left = value_bytes as i32;
        self.st.mode = mode::BITS_BYTES;
        false
    }

    /// As [`put_bits_bytes`](Self::put_bits_bytes), the 16 bytes of an `Int128` / `UInt128`:
    /// little-endian, the low 8 first.
    pub fn put_bits_bytes16(&mut self, info: u32, info_bits: i32, value: u128, continue_at_case: u32) -> bool {
        if self.put_bits_c(info, info_bits, continue_at_case) {
            self.put_uint128(value);
            return true;
        }
        self.st.u8 = value as u64;
        self.st.u8_ = (value >> 64) as u64;
        self.st.mode = mode::BITS_BYTES16;
        false
    }

    /// As [`put_bits_bytes`](Self::put_bits_bytes), the bits of a `f64`
    /// (`put_bits_bytes(int, int, double, int, uint)`).
    #[inline(always)]
    pub fn put_bits_bytes_f64(&mut self, info: u32, info_bits: i32, value: f64, value_bytes: usize, continue_at_case: u32) -> bool {
        self.put_bits_bytes(info, info_bits, value.to_bits(), value_bytes, continue_at_case)
    }

    /// As [`put_bits_bytes`](Self::put_bits_bytes), the bits of a `f32`
    /// (`put_bits_bytes(int, int, float, int, uint)`).
    #[inline(always)]
    pub fn put_bits_bytes_f32(&mut self, info: u32, info_bits: i32, value: f32, value_bytes: usize, continue_at_case: u32) -> bool {
        self.put_bits_bytes(info, info_bits, value.to_bits() as u64, value_bytes, continue_at_case)
    }

    #[inline(always)]
    fn byte_count(src: u64, min: usize) -> usize {
        (((63 - (src | 1).leading_zeros()) >> 3) as usize + 1).min(min)
    }

    pub fn put_varint21(&mut self, src: u64, continue_at_case: u32) -> bool {
        let bytes = Self::byte_count(src, 2);
        self.put_bits_bytes(bytes as u32 - 1, 1, src, bytes, continue_at_case)
    }
    pub fn put_varint21_nulls(&mut self, src: u64, continue_at_case: u32, nulls: u32, nulls_bits: i32) -> bool {
        let bytes = Self::byte_count(src, 2);
        self.put_bits_bytes((bytes as u32 - 1) << nulls_bits | nulls, nulls_bits + 1, src, bytes, continue_at_case)
    }
    pub fn put_varint32(&mut self, src: u64, continue_at_case: u32) -> bool {
        let bytes = Self::byte_count(src, 3);
        self.put_bits_bytes(bytes as u32, 2, src, bytes, continue_at_case)
    }
    pub fn put_varint32_nulls(&mut self, src: u64, continue_at_case: u32, nulls: u32, nulls_bits: i32) -> bool {
        let bytes = Self::byte_count(src, 3);
        self.put_bits_bytes((bytes as u32) << nulls_bits | nulls, nulls_bits + 2, src, bytes, continue_at_case)
    }
    pub fn put_varint42(&mut self, src: u64, continue_at_case: u32) -> bool {
        let bytes = Self::byte_count(src, 4);
        self.put_bits_bytes(bytes as u32 - 1, 2, src, bytes, continue_at_case)
    }
    pub fn put_varint42_nulls(&mut self, src: u64, continue_at_case: u32, nulls: u32, nulls_bits: i32) -> bool {
        let bytes = Self::byte_count(src, 4);
        self.put_bits_bytes((bytes as u32 - 1) << nulls_bits | nulls, nulls_bits + 2, src, bytes, continue_at_case)
    }
    pub fn put_varint73(&mut self, src: u64, continue_at_case: u32) -> bool {
        let bytes = Self::byte_count(src, 7);
        self.put_bits_bytes(bytes as u32, 3, src, bytes, continue_at_case)
    }
    pub fn put_varint73_nulls(&mut self, src: u64, continue_at_case: u32, nulls: u32, nulls_bits: i32) -> bool {
        let bytes = Self::byte_count(src, 7);
        self.put_bits_bytes((bytes as u32) << nulls_bits | nulls, nulls_bits + 3, src, bytes, continue_at_case)
    }
    pub fn put_varint83(&mut self, src: u64, continue_at_case: u32) -> bool {
        let bytes = Self::byte_count(src, 8);
        self.put_bits_bytes(bytes as u32 - 1, 3, src, bytes, continue_at_case)
    }
    pub fn put_varint83_nulls(&mut self, src: u64, continue_at_case: u32, nulls: u32, nulls_bits: i32) -> bool {
        let bytes = Self::byte_count(src, 8);
        self.put_bits_bytes((bytes as u32 - 1) << nulls_bits | nulls, nulls_bits + 3, src, bytes, continue_at_case)
    }
    pub fn put_varint84(&mut self, src: u64, continue_at_case: u32) -> bool {
        let bytes = Self::byte_count(src, 8);
        self.put_bits_bytes(bytes as u32, 4, src, bytes, continue_at_case)
    }
    pub fn put_varint84_nulls(&mut self, src: u64, continue_at_case: u32, nulls: u32, nulls_bits: i32) -> bool {
        let bytes = Self::byte_count(src, 8);
        self.put_bits_bytes((bytes as u32) << nulls_bits | nulls, nulls_bits + 4, src, bytes, continue_at_case)
    }

    /// Items of a bit array straight from its list, while the stream is on a byte boundary: whole
    /// groups of 8.
    pub fn put_bits_list<T>(&mut self, src: &crate::lib::collections::bits_list::BitsList<T>, item: i32, items: i32) -> i32 {
        if self.st.bit & 7 != 0 {
            return 0;
        }
        let bl = src.bits_per_item() as i32;
        let pos = self.bits_flush();
        let gl = ((items >> 3) as i64).min(((self.byte_max - pos) / bl as usize) as i64) as i32;
        let n = (gl * bl) as usize;
        if gl != 0 {
            #[cfg(target_endian = "little")]
            self.buffer[pos..pos + n].copy_from_slice(src.bytes(item as usize, (gl << 3) as usize));
            #[cfg(not(target_endian = "little"))]
            for g in 0..gl {
                let v = src.raw((item + (g << 3)) as usize, 8);
                crate::lib::adhoc::helpers::write_uint_le(v, self.buffer, pos + (g * bl) as usize, bl as usize);
            }
        }
        self.bits_reserve(pos + n);
        gl << 3
    }

    /// One-bit items, every whole byte the buffer has room for at once, while the stream is on a
    /// byte boundary.
    pub fn put_bits_bitlist(&mut self, src: &crate::lib::collections::bit_list::BitList, item: i32, items: i32) -> i32 {
        if self.st.bit & 7 != 0 {
            return 0;
        }
        let pos = self.bits_flush();
        let g = ((items.max(0) >> 3) as usize).min(self.byte_max - pos);
        src.get_bytes(item as usize, &mut self.buffer[pos..pos + g]);
        self.bits_reserve(pos + g);
        (g << 3) as i32
    }

    /// Long arrays of bit items of a slice, whole groups of 8 through `code`, while the stream is on
    /// a byte boundary: one kernel over all of them. A short array goes by `bits8`.
    #[inline]
    pub fn put_bits_items<V: Copy>(&mut self, src: &[V], item: i32, items: i32, bits_per_item: i32, code: impl Fn(V) -> u8) -> i32 {
        #[cfg(target_endian = "little")]
        {
            if items < crate::lib::collections::bits_list::KERNEL_ITEMS || self.st.bit & 7 != 0 {
                return 0;
            }
            let pos = self.bits_flush();
            let g = ((items >> 3) as usize).min((self.byte_max - pos) / bits_per_item as usize);
            if g != 0 {
                // SAFETY: `g * bits_per_item` bytes from `pos` are inside `byte_max`
                unsafe { crate::lib::collections::bits_list::pack_items(bits_per_item as usize, self.buffer.as_mut_ptr().add(pos), &src[item as usize..item as usize + (g << 3)], &code) };
            }
            self.bits_reserve(pos + g * bits_per_item as usize);
            (g << 3) as i32
        }
        #[cfg(not(target_endian = "little"))]
        0
    }

    /// Optional items, a group of 8: its first byte holds their nulls, their values go right after -
    /// the stream is on a byte boundary and `need` bytes are there: the first byte reserved.
    #[inline(always)]
    pub fn nulls_group_open(&mut self, need: usize) -> bool {
        let st = &mut *self.st;
        if st.bit & 7 != 0 {
            return false;
        }
        let pos = if st.bit == 0 { st.bits_byte } else { self.byte_ };
        if self.byte_max - pos < need {
            return false;
        }
        if st.bit != 0 {
            self.buffer[st.bits_byte] = st.bits as u8;
        }
        st.bits = 0;
        st.bit = 0;
        self.byte_ = pos;
        true
    }

    /// The groups are written up to `byte_`: the bit stream goes on behind them; false - no room for
    /// its next transaction, it goes on at `this_case`.
    #[inline(always)]
    pub fn nulls_group_close(&mut self, need: usize, this_case: u32) -> bool {
        let pos = self.byte_;
        self.bits_reserve(pos);
        if self.byte_max - pos < need {
            self.continue_bits_at(this_case);
            return false;
        }
        true
    }

    /// Room for a chunk of bit items an abstract pack hands one by one (`bits8`).
    pub fn bits8(&mut self, items: i32, bits_per_item: i32) -> i32 {
        if self.st.bit & 7 != 0 {
            return 0;
        }
        let pos = self.bits_flush();
        let mut n = (items.min(64 / bits_per_item) as i64).min((((self.byte_max - pos) as i64) << 3) / bits_per_item as i64) as i32;
        n -= n % (8 >> bits_per_item.trailing_zeros());
        if n <= 0 {
            self.bits_reserve(pos);
            return 0;
        }
        self.st.bits8_at = pos;
        n
    }

    /// Writes the chunk `bits8` made room for.
    pub fn bits8_put(&mut self, items: u64, n: i32, bits_per_item: i32) {
        let bytes = (n * bits_per_item >> 3) as usize;
        let at = self.st.bits8_at;
        self.buffer[at..at + bytes].copy_from_slice(&items.to_le_bytes()[..bytes]);
        self.bits_reserve(at + bytes);
    }

    fn bits_flush(&mut self) -> usize {
        if self.st.bit == 0 {
            return self.st.bits_byte;
        }
        self.buffer[self.st.bits_byte] = self.st.bits as u8;
        self.byte_
    }

    fn bits_reserve(&mut self, pos: usize) {
        self.st.bits = 0;
        self.st.bit = 0;
        self.st.bits_byte = pos;
        self.byte_ = if pos < self.byte_max { pos + 1 } else { pos };
    }

    // string
}

/// A one-shot transmitter of one pack (`Transmitter.Virtual`): it serializes the pack set by
/// [`set_bytes_src`](Self::set_bytes_src) into the buffers [`read`](Self::read) is given, one after
/// another, of any size - into a file, a byte array, a test - with no connection.
pub struct Virtual<S: BytesSrc> {
    st: TxState,
    one: One<S>,
}

/// The pack of a [`Virtual`]: gone once serialized.
struct One<S>(Option<S>);

impl<S: BytesSrc> TxHost for One<S> {
    #[inline]
    fn on_serializing(&mut self) -> bool {
        self.0.is_some()
    }
    #[inline]
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        self.0.as_ref().is_none_or(|s| s.get_bytes(dst))
    }
    #[inline]
    fn on_serialized(&mut self) {
        self.0 = None;
    }
}

impl<S: BytesSrc> Default for Virtual<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: BytesSrc> Virtual<S> {
    pub fn new() -> Self {
        Self { st: TxState::new(), one: One(None) }
    }

    /// The pack to serialize: before [`read`](Self::read). A pack in progress is dropped.
    pub fn set_bytes_src(&mut self, src: S) {
        self.st.abandon();
        self.one.0 = Some(src);
    }

    /// The bytes of the pack into `dst`, from where the previous buffer stopped. Returns the bytes
    /// written; 0 - `dst` has no room for the next value; -1 - the pack is out whole, or none is
    /// set.
    #[inline]
    pub fn read(&mut self, dst: &mut [u8]) -> isize {
        self.st.read(dst, &mut self.one)
    }

    /// The pack is out whole, or none is set (`IsIdle`).
    #[inline]
    pub fn is_idle(&self) -> bool {
        self.one.0.is_none()
    }

    /// Drops the pack, and the part of it not written yet (`Reset_`).
    pub fn reset(&mut self) {
        self.st.abandon();
        self.one.0 = None;
    }
}

/// The packs a connection `T` queued, straight into the pump's output buffer, with no framing.
pub struct Transmitter<T>
where
    for<'a> &'a T: TxHost,
{
    conn: std::sync::Arc<T>,
    tx: TxState,
}

impl<T> Transmitter<T>
where
    for<'a> &'a T: TxHost,
{
    pub fn new(conn: std::sync::Arc<T>) -> Self {
        Self { conn, tx: TxState::new() }
    }

    /// The keys of the session: a ChaCha20 stage of a pack chain takes the newest there.
    pub fn keys(mut self, keys: std::sync::Arc<crate::lib::adhoc::connection::keyring::Keyring>) -> Self {
        self.tx.keys = keys;
        self
    }

    /// The keys of the Envelope stages of the connection ( [`crate::lib::chain::Envelope`] ).
    pub fn envelope(mut self, envelope: std::sync::Arc<crate::lib::chain::Envelope>) -> Self {
        self.tx.envelope = Some(envelope);
        self
    }
}

impl<T: Send + Sync> crate::lib::adhoc::BytesSrc for Transmitter<T>
where
    for<'a> &'a T: TxHost,
{
    #[inline]
    fn at_boundary(&self) -> bool {
        !self.tx.is_open()
    }

    #[inline]
    fn read(&mut self, dst: &mut [u8]) -> isize {
        let mut host: &T = &self.conn;
        let n = self.tx.read(dst, &mut host);
        self.tx.gave(n, host.speed())
    }

    #[inline]
    fn clear(&self) -> bool {
        self.tx.clear
    }

    /// The pack in flight is dropped, the stream goes on with the next one.
    #[inline]
    fn abandon(&mut self) {
        self.tx.abandon();
    }
}

pub use framing::Framing;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::adhoc::connection::receiver::{self as rx, Rx, RxHost, RxState};

    /// What a copy got: the bytes of the pack in progress, the packs whole, the packs dropped.
    #[derive(Default)]
    pub(crate) struct Rec {
        pub(crate) cur: std::sync::Mutex<Vec<u8>>,
        pub(crate) whole: std::sync::Mutex<Vec<Vec<u8>>>,
        pub(crate) failed: std::sync::atomic::AtomicUsize,
    }

    impl BytesCopy for Rec {
        fn pass(&self, bytes: &[u8]) {
            self.cur.lock().unwrap().extend_from_slice(bytes);
        }
        fn done(&self, bytes: &[u8]) {
            let mut c = std::mem::take(&mut *self.cur.lock().unwrap());
            c.extend_from_slice(bytes);
            self.whole.lock().unwrap().push(c);
        }
        fn fail(&self) {
            self.cur.lock().unwrap().clear();
            self.failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Packs of raw bytes with an id of 2 bytes; the application's handler sets the copy on every
    /// other pack, and hears every pack out.
    struct Raw {
        q: Vec<Vec<u8>>,
        cur: Vec<u8>,
        n: usize,
        rec: std::sync::Arc<Rec>,
        heard: Vec<(&'static str, usize)>,
    }

    impl TxHost for Raw {
        fn on_serializing(&mut self) -> bool {
            if self.q.is_empty() {
                return false;
            }
            self.cur = self.q.remove(0);
            true
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            if dst.state() == 0 {
                dst.slot().index0 = 0;
                if !dst.put_val(0x0201, 2, 1) {
                    return false; // the id goes whole into the next buffer
                }
            }
            let at = dst.slot().index0 as usize;
            let n = dst.put_slice(&self.cur[at..], 1);
            dst.slot().index0 = (at + n) as i32;
            at + n == self.cur.len()
        }
        fn on_serialized(&mut self) {}
        fn serializing(&mut self, tx: &mut TxState) {
            self.heard.push(("serializing", self.n));
            if self.n % 2 == 0 {
                tx.bytes_copy = Some(self.rec.clone());
            }
        }
        fn serialized(&mut self, tx: &mut TxState) {
            assert!(tx.bytes_copy.is_none(), "taken away when the pack is out");
            self.heard.push(("serialized", self.n));
            self.n += 1;
        }
    }

    /// The copy of a pack gets every byte of it - the id too - however the buffers cut it, `done`
    /// once; the handler hears each pack start and end; a pack dropped half way is `fail`ed.
    #[test]
    fn a_copy_gets_the_pack_as_it_went_out() {
        for window in [2usize, 3, 7, 64, 4096] {
            let packs: Vec<Vec<u8>> = (0..5).map(|i| (0..(i * 37 + 5)).map(|j| (j * 7 + i) as u8).collect()).collect();
            let mut h = Raw {
                q: packs.clone(),
                cur: vec![],
                n: 0,
                rec: Default::default(),
                heard: vec![],
            };
            let mut st = TxState::new();
            let mut wire = vec![];
            let mut buf = vec![0u8; window];
            loop {
                let n = st.read(&mut buf, &mut h);
                if n < 0 {
                    break;
                }
                wire.extend_from_slice(&buf[..n as usize]);
            }
            let framed: Vec<Vec<u8>> = packs.iter().map(|p| [&[1u8, 2][..], p].concat()).collect();
            assert_eq!(wire, framed.concat());
            assert_eq!(*h.rec.whole.lock().unwrap(), vec![framed[0].clone(), framed[2].clone(), framed[4].clone()], "window {window}");
            assert_eq!(h.heard.len(), 10);
            assert!(h.heard.chunks(2).enumerate().all(|(i, w)| w == [("serializing", i), ("serialized", i)]));
        }
        // dropped half way
        let mut h = Raw {
            q: vec![vec![9; 100]],
            cur: vec![],
            n: 0,
            rec: Default::default(),
            heard: vec![],
        };
        let mut st = TxState::new();
        let mut buf = vec![0u8; 10];
        assert_eq!(st.read(&mut buf, &mut h), 10);
        assert_eq!(h.rec.cur.lock().unwrap().len(), 10, "passed");
        st.abandon();
        assert_eq!(h.rec.failed.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(h.rec.whole.lock().unwrap().is_empty() && h.rec.cur.lock().unwrap().is_empty());
    }

    /// Frames of tunnels with a Noise of their own ( the packs that start with 7 here ) and the
    /// other packs: a read gives bytes of one kind, says which ( `clear` ), and leaves nothing out -
    /// unframed, and pack by pack with the bytes before in the buffer ( `before` ).
    #[test]
    fn a_read_gives_bytes_of_one_kind() {
        struct Kinds(Raw);
        impl TxHost for Kinds {
            fn on_serializing(&mut self) -> bool {
                self.0.on_serializing()
            }
            fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
                self.0.get_bytes(dst)
            }
            fn on_serialized(&mut self) {}
            fn clear(&mut self) -> bool {
                self.0.cur[0] == 7
            }
        }
        let packs: Vec<Vec<u8>> = (0..12u8).map(|i| vec![if i % 3 == 1 || i % 5 == 0 { 7 } else { i }; 3 + i as usize * 5]).collect();
        let want: Vec<(Vec<u8>, bool)> = packs.iter().map(|p| ([&[1u8, 2][..], p].concat(), p[0] == 7)).collect();
        for window in [3usize, 8, 64, 4096] {
            for one in [false, true] {
                let mut h = Kinds(Raw {
                    q: packs.clone(),
                    cur: vec![],
                    n: 0,
                    rec: Default::default(),
                    heard: vec![],
                });
                let mut st = TxState::new();
                let mut buf = vec![0u8; window];
                let (mut clear, mut sealed) = (vec![], vec![]);
                loop {
                    // pack by pack, as the source of a journal fills: the kind of the bytes before it in the buffer
                    let mut at = 0;
                    let mut kind = -1i8;
                    let mut got = false;
                    while at < window {
                        let n = if one {
                            st.before = kind;
                            st.read_one(&mut buf[at..], &mut h).0
                        } else {
                            st.read(&mut buf[at..], &mut h)
                        };
                        if n <= 0 {
                            break;
                        }
                        assert!(kind < 0 || (kind == 1) == st.clear, "one kind a buffer");
                        kind = st.clear as i8;
                        at += n as usize;
                        got = true;
                        if !one {
                            break;
                        }
                    }
                    st.before = -1;
                    if !got {
                        break;
                    }
                    if kind == 1 { &mut clear } else { &mut sealed }.extend_from_slice(&buf[..at]);
                }
                assert_eq!(clear, want.iter().filter(|p| p.1).flat_map(|p| p.0.clone()).collect::<Vec<u8>>(), "window {window} one {one}");
                assert_eq!(sealed, want.iter().filter(|p| !p.1).flat_map(|p| p.0.clone()).collect::<Vec<u8>>(), "window {window} one {one}");
            }
        }
    }

    /// The handler drops a pack as it starts: not a byte of it goes, the packs after it do -
    /// unframed and one pack a call alike.
    #[test]
    fn the_handler_drops_a_pack_as_it_starts() {
        struct Dropping(Raw);
        impl TxHost for Dropping {
            fn on_serializing(&mut self) -> bool {
                self.0.on_serializing()
            }
            fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
                self.0.get_bytes(dst)
            }
            fn on_serialized(&mut self) {}
            fn serializing(&mut self, tx: &mut TxState) {
                if self.0.cur[0] % 2 == 1 {
                    tx.abandon(); // the odd ones
                }
            }
        }
        let packs: Vec<Vec<u8>> = (0..6u8).map(|i| vec![i; 3 + i as usize]).collect();
        let kept: Vec<u8> = packs.iter().filter(|p| p[0] % 2 == 0).flat_map(|p| [&[1u8, 2][..], p].concat()).collect();
        for one in [false, true] {
            let mut h = Dropping(Raw {
                q: packs.clone(),
                cur: vec![],
                n: 0,
                rec: Default::default(),
                heard: vec![],
            });
            let mut st = TxState::new();
            let (mut wire, mut buf) = (vec![], vec![0u8; 5]);
            loop {
                let n = if one { st.read_one(&mut buf, &mut h).0 } else { st.read(&mut buf, &mut h) };
                if n < 0 {
                    break;
                }
                wire.extend_from_slice(&buf[..n as usize]);
            }
            assert_eq!(wire, kept, "one pack a call: {one}");
        }
    }

    /// The pack in flight as text: nothing between packs; mid-pack its level, the state of its
    /// slot, the mode.
    #[test]
    fn the_pack_in_flight_as_text() {
        struct H(bool);
        impl TxHost for H {
            fn on_serializing(&mut self) -> bool {
                std::mem::take(&mut self.0)
            }
            fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
                if dst.state() == 0 {
                    if !dst.put_ulong_c(u64::MAX, 4) {
                        return false; // the window is too small: parked in state 4
                    }
                    dst.set_state(5);
                }
                dst.hold();
                false
            }
            fn on_serialized(&mut self) {}
        }
        let mut st = TxState::new();
        assert_eq!(format!("{st:?}"), "");
        let mut buf = [0u8; 3];
        let mut h = H(true);
        assert_eq!(st.read(&mut buf, &mut h), 0, "the value goes whole into the next buffer");
        assert_eq!(format!("{st:?}"), format!("level 0\t4 <\nmode {} bytes_left 8", mode::VAL8));
        let mut buf = [0u8; 16];
        assert_eq!(st.read(&mut buf, &mut h), 8);
        assert!(format!("{st:?}").ends_with(" held"), "{st:?}");
    }

    /// A pack of a long, a varint, a string: through the virtual transmitter, a buffer after a
    /// buffer of every size, and back through the virtual receiver, a piece after a piece of every
    /// size - the values a buffer cut go on in the next one.
    #[test]
    fn a_pack_through_virtual_buffers_of_any_size() {
        #[derive(Clone, Default, Debug, PartialEq)]
        struct P {
            a: u64,
            v: u64,
            s: Option<String>,
        }
        impl BytesSrc for P {
            fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
                loop {
                    match dst.state() {
                        0 => {
                            if !dst.put_val(5, 2, 1) {
                                return false;
                            }
                            dst.set_state(1);
                        }
                        1 => {
                            if !dst.put_ulong_c(self.a, 2) {
                                return false;
                            }
                            dst.set_state(2);
                        }
                        2 => {
                            if !dst.put_varint(self.v, 3) {
                                return false;
                            }
                            dst.set_state(3);
                        }
                        3 => return dst.put_string(self.s.as_deref().unwrap_or(""), u32::MAX, 3),
                        _ => return true,
                    }
                }
            }
        }
        impl rx::BytesDst for P {
            fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
                loop {
                    match src.state() {
                        0 => {
                            if !src.has_8bytes(1) {
                                return false;
                            }
                            self.a = src.get_ulong();
                            src.set_state(2);
                        }
                        1 => {
                            self.a = src.get_ulong_();
                            src.set_state(2);
                        }
                        2 => {
                            if !src.try_get_varint(3) {
                                return false;
                            }
                            self.v = src.u8;
                            src.set_state(4);
                        }
                        3 => {
                            self.v = src.u8;
                            src.set_state(4);
                        }
                        4 => {
                            if !src.try_get_string(1000, 5) {
                                return false;
                            }
                            src.set_state(5);
                        }
                        5 => {
                            self.s = src.get_string();
                            return true;
                        }
                        _ => return true,
                    }
                }
            }
        }
        let p = P {
            a: 0x0102_0304_0506_0708,
            v: 0x3FFF_FFFF_FFFF,
            s: Some("virtual: кириллица, 日本".into()),
        };
        let mut whole = vec![0u8; 4096];
        let mut v = Virtual::new();
        v.set_bytes_src(p.clone());
        let n = v.read(&mut whole);
        assert!(0 < n && v.is_idle() && v.read(&mut whole) == -1);
        whole.truncate(n as usize);
        for size in 8..=whole.len() {
            // a fixed value goes whole into one buffer: the u64, 8 bytes
            let mut v = Virtual::new();
            v.set_bytes_src(p.clone());
            let mut out = Vec::new();
            let mut buf = vec![0u8; size];
            loop {
                let n = v.read(&mut buf);
                if n < 0 {
                    break;
                }
                assert!(0 < n, "buffer {size}: nothing written");
                assert!(out.len() < whole.len(), "buffer {size}: more than the pack");
                out.extend_from_slice(&buf[..n as usize]);
            }
            assert_eq!(out, whole, "buffer {size}");
            let mut r = rx::Virtual::new(2);
            r.set_bytes_dst(P::default(), 5);
            for piece in whole.chunks(size) {
                assert_eq!(r.write(piece), piece.len() as isize, "piece {size}");
            }
            assert_eq!(r.take(), Some(p.clone()), "piece {size}");
        }
        let mut r = rx::Virtual::new(2);
        r.set_bytes_dst(P::default(), 6);
        assert!(r.write(&whole) < 0, "the id of another pack");
    }

    /// UTF-16 units another runtime may send, unpaired surrogates included: the string received is
    /// what `String::from_utf16_lossy` makes of them, whatever the windows.
    #[test]
    fn string_units_as_utf16_lossy() {
        let cases: Vec<Vec<u16>> = vec![
            vec![0xD800],
            vec![0xDC00, b'a' as u16],
            vec![b'a' as u16, 0xD83D, 0xDE00, b'b' as u16],
            vec![0xD83D, 0xD83D, 0xDE00],
            vec![0xD83D, b'x' as u16, 0xDE00],
            "кириллица, 日本, ascii".encode_utf16().collect(),
            (0..500u16).map(|i| [0x41, 0x436, 0x65E5, 0xD83D, 0xDE00, 0x3FFF, 0x4000][(i % 7) as usize]).collect(),
        ];
        for units in cases {
            let mut wire = vec![];
            let mut put = |v: u64, wire: &mut Vec<u8>| {
                let mut x = v;
                while 0x80 <= x {
                    wire.push(0x80 | x as u8);
                    x >>= 7;
                }
                wire.push(x as u8);
            };
            put(units.len() as u64, &mut wire);
            for &u in &units {
                put(u as u64, &mut wire);
            }
            let expected = String::from_utf16_lossy(&units);
            for window in [1usize, 2, 3, 5, 8, 16, 1000] {
                let mut rs = RxState::new(1);
                let mut got = None;
                let mut first = true;
                for chunk in wire.chunks(window) {
                    let mut rx = Rx::new(chunk, &mut rs);
                    let done = if first {
                        first = false;
                        rx.try_get_string(u32::MAX, 0)
                    } else {
                        rx.resume() == 1
                    };
                    if done {
                        got = rx.get_string();
                        break;
                    }
                }
                assert_eq!(got.as_deref(), Some(expected.as_str()), "{units:x?} window {window}");
            }
        }
    }

    /// The varint of every length (1..=10 bytes), at every place in a buffer: the bytes as the
    /// 7-bit loop writes them, and read back whole - by the word path where 8 bytes are there, by
    /// the byte path at the end of the buffer.
    #[test]
    fn varint_word_paths_as_the_loop() {
        let mut values = vec![0u64, 1, 0x7F, 0x80, 0xFF, 0x3FFF, 0x4000, u64::MAX, u64::MAX >> 1];
        for k in 0..64 {
            values.push(1u64 << k);
            values.push((1u64 << k) - 1);
            values.push((1u64 << k) | 0x55);
        }
        for &v in &values {
            let mut reference = vec![];
            let mut x = v;
            while 0x80 <= x {
                reference.push(0x80 | x as u8);
                x >>= 7;
            }
            reference.push(x as u8);
            for room in [reference.len(), reference.len() + 3, 16, 32] {
                let mut st = TxState::new();
                let mut buf = vec![0xAAu8; room];
                let mut tx = Tx {
                    buffer: &mut buf,
                    byte_: 0,
                    byte_max: room,
                    st: &mut st,
                };
                assert!(tx.varint(v), "{v:x} room {room}");
                assert_eq!(tx.byte_, reference.len());
                assert_eq!(&buf[..reference.len()], &reference[..], "{v:x} room {room}");
                for lead in 0..3usize {
                    let mut wire = vec![0x11u8; lead];
                    wire.extend_from_slice(&reference);
                    wire.extend(std::iter::repeat_n(0xEEu8, room - reference.len()));
                    let mut rs = RxState::new(1);
                    let mut rx = Rx::new(&wire, &mut rs);
                    rx.byte_ = lead;
                    rx.u8 = 0;
                    rx.bytes_left = 0;
                    assert!(rx.varint(), "{v:x} lead {lead} room {room}");
                    assert_eq!(rx.u8, v, "{v:x} lead {lead} room {room}");
                    assert_eq!(rx.byte_, lead + reference.len());
                }
            }
        }
    }

    #[derive(Default, Debug, PartialEq, Clone)]
    struct P {
        a: u32,
        f: f64,
        s: String,
    }

    impl BytesSrc for P {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    0 => {
                        if !dst.put_val(7, 1, 1) {
                            return false;
                        }
                        dst.set_state(1);
                    }
                    1 => {
                        if !dst.put_uint_c(self.a, 2) {
                            return false;
                        }
                        dst.set_state(2);
                    }
                    2 => {
                        if !dst.put_double_c(self.f, 3) {
                            return false;
                        }
                        dst.set_state(3);
                    }
                    3 => return dst.put_string(&self.s, u32::MAX, 3),
                    _ => return true,
                }
            }
        }
    }

    impl rx::BytesDst for P {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            loop {
                match src.state() {
                    0 => {
                        if !src.has_4bytes(1) {
                            return false;
                        }
                        self.a = src.get_uint();
                        src.set_state(2);
                    }
                    1 => {
                        self.a = src.get_uint_();
                        src.set_state(2);
                    }
                    2 => {
                        if !src.has_8bytes(3) {
                            return false;
                        }
                        self.f = src.get_double();
                        src.set_state(4);
                    }
                    3 => {
                        self.f = src.get_double_();
                        src.set_state(4);
                    }
                    4 => {
                        if !src.try_get_string(1000, 5) {
                            return false;
                        }
                        src.set_state(5);
                    }
                    5 => {
                        self.s = src.get_string().unwrap_or_default();
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    struct Out {
        q: Vec<P>,
        cur: Option<P>,
    }

    impl TxHost for Out {
        fn on_serializing(&mut self) -> bool {
            self.cur = if self.q.is_empty() { None } else { Some(self.q.remove(0)) };
            self.cur.is_some()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            self.cur.as_ref().unwrap().get_bytes(dst)
        }
        fn on_serialized(&mut self) {
            self.cur = None;
        }
    }

    struct In {
        p: P,
        got: Vec<P>,
    }

    impl RxHost for In {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            id == 7
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            rx::BytesDst::put_bytes(&mut self.p, src)
        }
        fn on_received(&mut self, _st: &mut RxState) {
            self.got.push(std::mem::take(&mut self.p));
        }
    }

    #[test]
    fn round_trip_any_buffer() {
        let packs = vec![
            P { a: 1, f: 2.5, s: "ascii only".into() },
            P {
                a: u32::MAX,
                f: -0.0,
                s: "смесь ASCII и 😀 emoji".into(),
            },
            P { a: 0, f: f64::NAN, s: String::new() },
        ];
        for size in 8..64 {
            let mut st = TxState::new();
            let mut out = Out { q: packs.clone(), cur: None };
            let mut wire = Vec::new();
            let mut buf = vec![0u8; size];
            loop {
                let n = st.read(&mut buf, &mut out);
                if n < 0 {
                    break;
                }
                wire.extend_from_slice(&buf[..n as usize]);
            }
            let mut rs = RxState::new(1);
            let mut inp = In { p: P::default(), got: vec![] };
            for chunk in wire.chunks(size) {
                assert!(rs.write_all(chunk, &mut inp) >= 0);
            }
            assert_eq!(inp.got.len(), 3, "buffer {size}");
            for (a, b) in inp.got.iter().zip(&packs) {
                assert_eq!((a.a, &a.s), (b.a, &b.s), "buffer {size}");
                assert_eq!(a.f.to_bits(), b.f.to_bits(), "buffer {size}");
            }
        }
    }

    /// A pack of one string with the limit of its description.
    struct Limited(&'static str, u32);

    impl BytesSrc for Limited {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            if dst.state() == 0 {
                if !dst.put_val(7, 1, 1) {
                    return false;
                }
                dst.set_state(1);
            }
            dst.put_string(self.0, self.1, 1)
        }
    }

    struct OutLimited(Option<Limited>);

    impl TxHost for OutLimited {
        fn on_serializing(&mut self) -> bool {
            self.0.is_some()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            self.0.as_ref().unwrap().get_bytes(dst)
        }
        fn on_serialized(&mut self) {
            self.0 = None;
        }
    }

    #[test]
    fn string_at_the_limit_goes() {
        let mut st = TxState::new();
        let mut buf = [0u8; 64];
        assert!(0 < st.read(&mut buf, &mut OutLimited(Some(Limited("abcd", 4)))));
    }

    /// Above the limit of the description is an error of the code that filled the pack: a panic,
    /// never cut.
    #[test]
    #[should_panic(expected = "a string: 5, above the limit of the description 4")]
    fn string_above_the_limit_panics() {
        let mut st = TxState::new();
        let mut buf = [0u8; 64];
        st.read(&mut buf, &mut OutLimited(Some(Limited("abcde", 4))));
    }

    // chains

    /// A leaf of a chain: a number and a long string, its tail a number again.
    #[derive(Default, Debug, PartialEq, Clone)]
    struct L {
        a: u32,
        s: String,
        z: u64,
    }

    impl BytesSrc for L {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    1 => {
                        if !dst.put_uint_c(self.a, 2) {
                            return false;
                        }
                        dst.set_state(2);
                    }
                    2 => {
                        if !dst.put_string(&self.s, u32::MAX, 2) {
                            return false;
                        }
                        dst.set_state(3);
                    }
                    3 => {
                        if !dst.put_ulong_c(self.z, 4) {
                            return false;
                        }
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    impl rx::BytesDst for L {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            loop {
                match src.state() {
                    0 => {
                        if !src.has_4bytes(1) {
                            return false;
                        }
                        self.a = src.get_uint();
                        src.set_state(2);
                    }
                    1 => {
                        self.a = src.get_uint_();
                        src.set_state(2);
                    }
                    2 => {
                        if !src.try_get_string(300_000, 3) {
                            return false;
                        }
                        src.set_state(3);
                    }
                    3 => {
                        self.s = src.get_string().unwrap_or_default();
                        src.set_state(4);
                    }
                    4 => {
                        if !src.has_8bytes(5) {
                            return false;
                        }
                        self.z = src.get_ulong();
                        return true;
                    }
                    5 => {
                        self.z = src.get_ulong_();
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    /// A pack with a leaf in the chain 0 between two numbers; sent on its own it has the id 9,
    /// nested — not.
    #[derive(Default, Debug, PartialEq, Clone)]
    struct O {
        head: u8,
        l: L,
        tail: u64,
    }

    impl BytesSrc for O {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    0 => {
                        if !dst.put_val(9, 1, 1) {
                            return false;
                        }
                        dst.set_state(1);
                    }
                    1 => {
                        if !dst.put_byte_c(self.head, 2) {
                            return false;
                        }
                        dst.set_state(2);
                    }
                    2 => {
                        if !dst.put_chain(0, &self.l, 2) {
                            return false;
                        }
                        dst.set_state(3);
                    }
                    3 => {
                        if !dst.put_ulong_c(self.tail, 4) {
                            return false;
                        }
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    impl rx::BytesDst for O {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            loop {
                match src.state() {
                    0 => {
                        if !src.has_1bytes(1) {
                            return false;
                        }
                        self.head = src.get_byte();
                        src.set_state(2);
                    }
                    1 => {
                        self.head = src.get_byte_();
                        src.set_state(2);
                    }
                    2 => {
                        if !src.get_chain(0, &mut self.l, 2) {
                            return false;
                        }
                        src.set_state(3);
                    }
                    3 => {
                        if !src.has_8bytes(4) {
                            return false;
                        }
                        self.tail = src.get_ulong();
                        return true;
                    }
                    4 => {
                        self.tail = src.get_ulong_();
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    /// A pack with `O` in the chain 1: a chain in a chain.
    #[derive(Default, Debug, PartialEq, Clone)]
    struct Q {
        o: O,
        end: u32,
    }

    impl BytesSrc for Q {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    0 => {
                        if !dst.put_val(10, 1, 1) {
                            return false;
                        }
                        dst.set_state(1);
                    }
                    1 => {
                        if !dst.put_chain(1, &self.o, 1) {
                            return false;
                        }
                        dst.set_state(2);
                    }
                    2 => return dst.put_uint_c(self.end, 3),
                    _ => return true,
                }
            }
        }
    }

    impl rx::BytesDst for Q {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            loop {
                match src.state() {
                    0 => {
                        if !src.get_chain(1, &mut self.o, 0) {
                            return false;
                        }
                        src.set_state(1);
                    }
                    1 => {
                        if !src.has_4bytes(2) {
                            return false;
                        }
                        self.end = src.get_uint();
                        return true;
                    }
                    2 => {
                        self.end = src.get_uint_();
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    /// The stages of the test chains: `Z` — the chain 0 is `[Zstd]`, the chain 1 `[Zstd(6)]`;
    /// otherwise no stages.
    fn stages<const Z: bool>(id: u32) -> &'static [Stage] {
        if !Z {
            &[]
        } else if id == 0 {
            &[Stage::Stages(crate::lib::stages::Run {
                kind1: crate::lib::stages::ZSTD,
                level1: 3,
                kind2: 0,
                level2: 0,
            })]
        } else {
            &[Stage::Stages(crate::lib::stages::Run {
                kind1: crate::lib::stages::ZSTD,
                level1: 6,
                kind2: 0,
                level2: 0,
            })]
        }
    }

    enum Pk {
        O(O),
        Q(Q),
    }

    struct OutC<const Z: bool> {
        q: Vec<Pk>,
        cur: Option<Pk>,
    }

    impl<const Z: bool> TxHost for OutC<Z> {
        fn on_serializing(&mut self) -> bool {
            self.cur = if self.q.is_empty() { None } else { Some(self.q.remove(0)) };
            self.cur.is_some()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            match self.cur.as_ref().unwrap() {
                Pk::O(o) => o.get_bytes(dst),
                Pk::Q(q) => q.get_bytes(dst),
            }
        }
        fn on_serialized(&mut self) {
            self.cur = None;
        }
        fn chain(id: u32) -> &'static [Stage] {
            stages::<Z>(id)
        }
    }

    struct InC<const Z: bool> {
        id: u32,
        o: O,
        q: Q,
        got_o: Vec<O>,
        got_q: Vec<Q>,
    }

    impl<const Z: bool> RxHost for InC<Z> {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            self.id = id;
            id == 9 || id == 10
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            if self.id == 9 {
                rx::BytesDst::put_bytes(&mut self.o, src)
            } else {
                rx::BytesDst::put_bytes(&mut self.q, src)
            }
        }
        fn on_received(&mut self, _st: &mut RxState) {
            if self.id == 9 {
                self.got_o.push(std::mem::take(&mut self.o));
            } else {
                self.got_q.push(std::mem::take(&mut self.q));
            }
        }
        fn chain(id: u32) -> &'static [Stage] {
            stages::<Z>(id)
        }
    }

    fn text(n: usize, seed: u32) -> String {
        let mut x = seed.wrapping_mul(2654435761).max(1);
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                if i % 7 == 0 {
                    'я'
                } else {
                    (b'a' + (x % 26) as u8) as char
                }
            })
            .collect()
    }

    fn chain_round_trip<const Z: bool>(sizes: &[usize]) {
        let o = |n: usize, s: u32| O {
            head: s as u8,
            l: L {
                a: s,
                s: text(n, s),
                z: u64::MAX - s as u64,
            },
            tail: 0x0102_0304_0506_0708 ^ s as u64,
        };
        for &size in sizes {
            let os = vec![o(0, 1), o(10, 2), o(70_000, 3), o(200_000, 4)];
            let qs = vec![Q { o: o(5, 5), end: 77 }, Q { o: o(150_000, 6), end: u32::MAX }];
            let mut q: Vec<Pk> = os.iter().cloned().map(Pk::O).collect();
            q.extend(qs.iter().cloned().map(Pk::Q));
            let mut st = TxState::new();
            let mut out = OutC::<Z> { q, cur: None };
            let mut wire = Vec::new();
            let mut buf = vec![0u8; size];
            let mut zero = 0;
            loop {
                let n = st.read(&mut buf, &mut out);
                if n < 0 {
                    break;
                }
                zero = if n == 0 { zero + 1 } else { 0 };
                assert!(zero < 3, "zstd {Z} buffer {size}: the transmitter is stuck at {} bytes", wire.len());
                wire.extend_from_slice(&buf[..n as usize]);
            }
            let mut rs = RxState::new(1);
            let mut inp = InC::<Z> {
                id: 0,
                o: O::default(),
                q: Q::default(),
                got_o: vec![],
                got_q: vec![],
            };
            for w in wire.chunks(size) {
                assert!(rs.write_all(w, &mut inp) >= 0, "zstd {Z} buffer {size}: {:?}", rs.error);
            }
            assert!(inp.got_o == os, "zstd {Z} buffer {size}");
            assert!(inp.got_q == qs, "zstd {Z} buffer {size}");
        }
    }

    #[test]
    fn chain_plain_round_trip() {
        chain_round_trip::<false>(&[16, 17, 23, 64, 1000, 70_000, 200_000]);
    }

    #[test]
    fn chain_zstd_round_trip() {
        chain_round_trip::<true>(&[16, 17, 23, 64, 1000, 70_000, 200_000]);
    }

    /// A pack with a stream field between two numbers: the bytes come from a source and go into a
    /// sink.
    #[derive(Default)]
    struct Sp<const Z: bool> {
        head: u8,
        data: Vec<u8>,
        got: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        tail: u32,
    }

    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<const Z: bool> BytesSrc for Sp<Z> {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    0 => {
                        if !dst.put_val(11, 1, 1) {
                            return false;
                        }
                        dst.set_state(1);
                    }
                    1 => {
                        if !dst.put_byte_c(self.head, 2) {
                            return false;
                        }
                        dst.set_state(2);
                    }
                    2 => {
                        let d = self.data.clone();
                        if !dst.put_stream(if Z { Some(0) } else { None }, move || Box::new(std::io::Cursor::new(d)), 1 << 30, 2) {
                            return false;
                        }
                        dst.set_state(3);
                    }
                    3 => return dst.put_uint_c(self.tail, 4),
                    _ => return true,
                }
            }
        }
    }

    impl<const Z: bool> rx::BytesDst for Sp<Z> {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            loop {
                match src.state() {
                    0 => {
                        if !src.has_1bytes(1) {
                            return false;
                        }
                        self.head = src.get_byte();
                        src.set_state(2);
                    }
                    1 => {
                        self.head = src.get_byte_();
                        src.set_state(2);
                    }
                    2 => {
                        let g = self.got.clone();
                        if !src.get_stream(if Z { Some(0) } else { None }, move || Box::new(Sink(g)), 1 << 30, 2) {
                            return false;
                        }
                        src.set_state(3);
                    }
                    3 => {
                        if !src.has_4bytes(4) {
                            return false;
                        }
                        self.tail = src.get_uint();
                        return true;
                    }
                    4 => {
                        self.tail = src.get_uint_();
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    struct OutS<const Z: bool> {
        q: Vec<Sp<Z>>,
        cur: Option<Sp<Z>>,
    }

    impl<const Z: bool> TxHost for OutS<Z> {
        fn on_serializing(&mut self) -> bool {
            self.cur = if self.q.is_empty() { None } else { Some(self.q.remove(0)) };
            self.cur.is_some()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            self.cur.as_ref().unwrap().get_bytes(dst)
        }
        fn on_serialized(&mut self) {
            self.cur = None;
        }
        fn chain(id: u32) -> &'static [Stage] {
            stages::<Z>(id)
        }
    }

    struct InS<const Z: bool> {
        p: Sp<Z>,
        got: Vec<(u8, Vec<u8>, u32)>,
    }

    impl<const Z: bool> RxHost for InS<Z> {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            self.p = Sp::default();
            id == 11
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            rx::BytesDst::put_bytes(&mut self.p, src)
        }
        fn on_received(&mut self, _st: &mut RxState) {
            let g = std::mem::take(&mut *self.p.got.lock().unwrap());
            self.got.push((self.p.head, g, self.p.tail));
        }
        fn chain(id: u32) -> &'static [Stage] {
            stages::<Z>(id)
        }
    }

    fn stream_round_trip<const Z: bool>(sizes: &[usize]) {
        for &size in sizes {
            let packs: Vec<(u8, Vec<u8>, u32)> = vec![(1, vec![], 7), (2, text(10, 2).into_bytes(), 8), (3, text(100_000, 3).into_bytes(), u32::MAX), (4, vec![0; 300_000], 0)];
            let mut st = TxState::new();
            let mut out = OutS::<Z> {
                q: packs
                    .iter()
                    .map(|(h, d, t)| Sp {
                        head: *h,
                        data: d.clone(),
                        got: Default::default(),
                        tail: *t,
                    })
                    .collect(),
                cur: None,
            };
            let mut wire = Vec::new();
            let mut buf = vec![0u8; size];
            let mut zero = 0;
            loop {
                let n = st.read(&mut buf, &mut out);
                if n < 0 {
                    break;
                }
                zero = if n == 0 { zero + 1 } else { 0 };
                assert!(zero < 3, "zstd {Z} buffer {size}: the transmitter is stuck at {} bytes", wire.len());
                wire.extend_from_slice(&buf[..n as usize]);
            }
            let mut rs = RxState::new(1);
            let mut inp = InS::<Z> { p: Sp::default(), got: vec![] };
            for w in wire.chunks(size) {
                assert!(rs.write_all(w, &mut inp) >= 0, "zstd {Z} buffer {size}: {:?}", rs.error);
            }
            assert!(inp.got == packs, "zstd {Z} buffer {size}");
        }
    }

    type Ends = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, bool)>>>;

    /// A pack with a stream whose ends are heard: the hook is armed at every entry, as code that
    /// does not know whether the stream starts there - only the start takes it.
    struct Sh(Vec<u8>, Ends, std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl BytesSrc for Sh {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    0 => {
                        if !dst.put_val(13, 1, 1) {
                            return false;
                        }
                        dst.set_state(1);
                    }
                    1 => {
                        let e = self.1.clone();
                        dst.on_stream_complete(Box::new(move |whole| e.lock().unwrap().push(("tx", whole))));
                        let d = self.0.clone();
                        return dst.put_stream(None, move || Box::new(std::io::Cursor::new(d)), 1 << 30, 1);
                    }
                    _ => return true,
                }
            }
        }
    }

    impl rx::BytesDst for Sh {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            let e = self.1.clone();
            src.on_stream_complete(Box::new(move |whole| e.lock().unwrap().push(("rx", whole))));
            let g = self.2.clone();
            src.get_stream(None, move || Box::new(Sink(g)), 1 << 30, 0)
        }
    }

    struct OutH(Option<Sh>);

    impl TxHost for OutH {
        fn on_serializing(&mut self) -> bool {
            self.0.is_some()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            self.0.as_ref().unwrap().get_bytes(dst)
        }
        fn on_serialized(&mut self) {
            self.0 = None;
        }
    }

    struct InH(Sh, usize);

    impl RxHost for InH {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            id == 13
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            rx::BytesDst::put_bytes(&mut self.0, src)
        }
        fn on_received(&mut self, _st: &mut RxState) {
            self.1 += 1;
        }
    }

    /// The end of a stream is heard once, on both sides: whole - `true`; torn half way - `false`.
    #[test]
    fn the_end_of_a_stream_is_heard_once() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7) as u8).collect();
        for size in [7usize, 1000, 70_000] {
            let ends: Ends = Default::default();
            let mut st = TxState::new();
            let mut out = OutH(Some(Sh(data.clone(), ends.clone(), Default::default())));
            let mut wire = vec![];
            let mut buf = vec![0u8; size];
            loop {
                let n = st.read(&mut buf, &mut out);
                if n < 0 {
                    break;
                }
                wire.extend_from_slice(&buf[..n as usize]);
            }
            let got: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
            let mut rs = RxState::new(1);
            let mut inp = InH(Sh(vec![], ends.clone(), got.clone()), 0);
            for w in wire.chunks(size) {
                assert!(rs.write_all(w, &mut inp) >= 0);
            }
            assert!(inp.1 == 1 && *got.lock().unwrap() == data, "size {size}");
            assert_eq!(*ends.lock().unwrap(), vec![("tx", true), ("rx", true)], "size {size}");
            // torn half way, both sides
            let ends: Ends = Default::default();
            let mut st = TxState::new();
            let mut out = OutH(Some(Sh(data.clone(), ends.clone(), Default::default())));
            assert!(0 < st.read(&mut buf, &mut out));
            let mut rs = RxState::new(1);
            let mut inp = InH(Sh(vec![], ends.clone(), Default::default()), 0);
            assert!(rs.write_all(&wire[..wire.len() / 2], &mut inp) >= 0);
            st.abandon();
            rs.abandon();
            assert_eq!(*ends.lock().unwrap(), vec![("tx", false), ("rx", false)], "size {size}");
        }
    }

    /// A pack with a string through the chain 0 between two numbers.
    #[derive(Default, Debug, PartialEq, Clone)]
    struct Ss<const Z: bool, const U: bool> {
        a: u8,
        s: String,
        b: u32,
    }

    impl<const Z: bool, const U: bool> BytesSrc for Ss<Z, U> {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    0 => {
                        if !dst.put_val(12, 1, 1) {
                            return false;
                        }
                        dst.set_state(1);
                    }
                    1 => {
                        if !dst.put_byte_c(self.a, 2) {
                            return false;
                        }
                        dst.set_state(2);
                    }
                    2 => {
                        if !dst.put_string_chain(0, &self.s, U, u32::MAX, 2) {
                            return false;
                        }
                        dst.set_state(3);
                    }
                    3 => return dst.put_uint_c(self.b, 4),
                    _ => return true,
                }
            }
        }
    }

    impl<const Z: bool, const U: bool> rx::BytesDst for Ss<Z, U> {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            loop {
                match src.state() {
                    0 => {
                        if !src.has_1bytes(1) {
                            return false;
                        }
                        self.a = src.get_byte();
                        src.set_state(2);
                    }
                    1 => {
                        self.a = src.get_byte_();
                        src.set_state(2);
                    }
                    2 => {
                        if !src.get_string_chain(0, U, 1_000_000, 2) {
                            return false;
                        }
                        self.s = src.get_string().unwrap_or_default();
                        src.set_state(3);
                    }
                    3 => {
                        if !src.has_4bytes(4) {
                            return false;
                        }
                        self.b = src.get_uint();
                        return true;
                    }
                    4 => {
                        self.b = src.get_uint_();
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    struct OutSs<const Z: bool, const U: bool> {
        q: Vec<Ss<Z, U>>,
        cur: Option<Ss<Z, U>>,
    }

    impl<const Z: bool, const U: bool> TxHost for OutSs<Z, U> {
        fn on_serializing(&mut self) -> bool {
            self.cur = if self.q.is_empty() { None } else { Some(self.q.remove(0)) };
            self.cur.is_some()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            self.cur.as_ref().unwrap().get_bytes(dst)
        }
        fn on_serialized(&mut self) {
            self.cur = None;
        }
        fn chain(id: u32) -> &'static [Stage] {
            stages::<Z>(id)
        }
    }

    struct InSs<const Z: bool, const U: bool> {
        p: Ss<Z, U>,
        got: Vec<Ss<Z, U>>,
    }

    impl<const Z: bool, const U: bool> RxHost for InSs<Z, U> {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            id == 12
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            rx::BytesDst::put_bytes(&mut self.p, src)
        }
        fn on_received(&mut self, _st: &mut RxState) {
            self.got.push(std::mem::take(&mut self.p));
        }
        fn chain(id: u32) -> &'static [Stage] {
            stages::<Z>(id)
        }
    }

    fn string_round_trip<const Z: bool, const U: bool>(sizes: &[usize]) {
        for &size in sizes {
            let packs: Vec<Ss<Z, U>> = vec![
                Ss { a: 1, s: String::new(), b: 7 },
                Ss { a: 2, s: "ascii".into(), b: 8 },
                Ss {
                    a: 3,
                    s: "смесь и 😀 emoji ".repeat(9000),
                    b: u32::MAX,
                },
                Ss { a: 4, s: text(150_000, 9), b: 0 },
            ];
            let mut st = TxState::new();
            let mut out = OutSs::<Z, U> { q: packs.clone(), cur: None };
            let mut wire = Vec::new();
            let mut buf = vec![0u8; size];
            loop {
                let n = st.read(&mut buf, &mut out);
                if n < 0 {
                    break;
                }
                wire.extend_from_slice(&buf[..n as usize]);
            }
            let mut rs = RxState::new(1);
            let mut inp = InSs::<Z, U> { p: Ss::default(), got: vec![] };
            for w in wire.chunks(size) {
                assert!(rs.write_all(w, &mut inp) >= 0, "zstd {Z} utf {U} buffer {size}: {:?}", rs.error);
            }
            assert!(inp.got == packs, "zstd {Z} utf {U} buffer {size}");
        }
    }

    #[test]
    fn chain_string_round_trip() {
        string_round_trip::<false, false>(&[16, 17, 23, 1000, 70_000]);
        string_round_trip::<false, true>(&[16, 17, 23, 1000, 70_000]);
        string_round_trip::<true, false>(&[16, 17, 23, 1000, 70_000]);
        string_round_trip::<true, true>(&[16, 17, 23, 1000, 70_000]);
    }

    /// The string of the field, then the fragments of `strs` after it: one string on the wire.
    fn fragments_round_trip<const Z: bool, const U: bool>(sizes: &[usize]) {
        let parts: Vec<String> = vec!["".into(), "смесь и 😀 ".repeat(3000), "a".into(), text(70_000, 5), "".into(), "конец".into()];
        for &size in sizes {
            let mut st = TxState::new();
            let mut left = parts.clone().into_iter();
            let mut i = 0usize;
            st.strs = Some(Box::new(move |mut spent: String| {
                let p = left.next()?;
                i += 1;
                if i % 2 == 0 {
                    return Some(p); // moved in
                }
                spent.push_str(&p); // the buffer given back, filled again
                Some(spent)
            }));
            let head = Ss::<Z, U> { a: 5, s: "head ".into(), b: 9 };
            let mut out = OutSs::<Z, U> { q: vec![head.clone()], cur: None };
            let mut wire = Vec::new();
            let mut buf = vec![0u8; size];
            loop {
                let n = st.read(&mut buf, &mut out);
                if n < 0 {
                    break;
                }
                wire.extend_from_slice(&buf[..n as usize]);
            }
            assert!(st.strs.is_none(), "drained");
            let mut rs = RxState::new(1);
            let mut inp = InSs::<Z, U> { p: Ss::default(), got: vec![] };
            for w in wire.chunks(size) {
                assert!(rs.write_all(w, &mut inp) >= 0, "zstd {Z} utf {U} buffer {size}: {:?}", rs.error);
            }
            let whole = Ss::<Z, U> {
                s: String::from("head ") + &parts.concat(),
                ..head
            };
            assert!(inp.got == vec![whole], "zstd {Z} utf {U} buffer {size}");
        }
    }

    #[test]
    fn chain_string_fragments_join_on_the_wire() {
        fragments_round_trip::<false, false>(&[16, 17, 1000, 70_000]);
        fragments_round_trip::<false, true>(&[16, 17, 1000, 70_000]);
        fragments_round_trip::<true, false>(&[16, 17, 1000, 70_000]);
        fragments_round_trip::<true, true>(&[16, 17, 1000, 70_000]);
    }

    /// The fragments of `strs` out of the transmitter, MB a second: moved in, or copied into the
    /// buffer given back. Run with `cargo test --release -- --ignored strs_speed --nocapture`.
    #[test]
    #[ignore]
    fn strs_speed() {
        fn run<const U: bool>(what: &str, parts: &[String], moved: bool) {
            let bytes: usize = parts.iter().map(|p| p.len()).sum();
            let mut buf = vec![0u8; 64 * 1024];
            let mut best = f64::MAX;
            for _ in 0..7 {
                let mut st = TxState::new();
                let mut left = parts.to_vec().into_iter();
                st.strs = Some(if moved {
                    Box::new(move |_spent: String| left.next())
                } else {
                    Box::new(move |mut spent: String| {
                        let p = left.next()?;
                        spent.push_str(&p);
                        Some(spent)
                    })
                });
                let mut out = OutSs::<false, U> {
                    q: vec![Ss { a: 1, s: String::new(), b: 2 }],
                    cur: None,
                };
                let t = std::time::Instant::now();
                while 0 <= st.read(&mut buf, &mut out) {}
                best = best.min(t.elapsed().as_secs_f64());
            }
            println!("{what:<44} {:>8.0} MB/s", bytes as f64 / best / 1e6);
        }
        for (name, piece) in [("ASCII", "a1b2c3d4".repeat(512)), ("Cyrillic", "проверка".repeat(256)), ("CJK", "漢字の試験".repeat(273))] {
            let parts: Vec<String> = (0..25_000).map(|_| piece.clone()).collect();
            run::<true>(&format!("UTF-8 wire, {name}, moved"), &parts, true);
            run::<true>(&format!("UTF-8 wire, {name}, copied into the buffer"), &parts, false);
            run::<false>(&format!("varint wire, {name}, moved"), &parts, true);
            run::<false>(&format!("varint wire, {name}, copied into the buffer"), &parts, false);
        }
    }

    struct OutK(OutC<false>);
    impl TxHost for OutK {
        fn on_serializing(&mut self) -> bool {
            self.0.on_serializing()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            self.0.get_bytes(dst)
        }
        fn on_serialized(&mut self) {
            self.0.on_serialized()
        }
        fn chain(id: u32) -> &'static [Stage] {
            if id == 0 {
                &[Stage::Stages(crate::lib::stages::Run {
                    kind1: crate::lib::stages::CHACHA20_PACK,
                    level1: 0,
                    kind2: 0,
                    level2: 0,
                })]
            } else {
                &[
                    Stage::Stages(crate::lib::stages::Run {
                        kind1: crate::lib::stages::CHACHA20_PACK,
                        level1: 0,
                        kind2: 0,
                        level2: 0,
                    }),
                    Stage::Stages(crate::lib::stages::Run {
                        kind1: crate::lib::stages::CHACHA20_PACK,
                        level1: 0,
                        kind2: 0,
                        level2: 0,
                    }),
                ]
            }
        }
    }

    struct InK(InC<false>);
    impl RxHost for InK {
        fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool {
            self.0.on_receiving(src, id)
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            self.0.put_bytes(src)
        }
        fn on_received(&mut self, st: &mut RxState) {
            self.0.on_received(st)
        }
        fn chain(id: u32) -> &'static [Stage] {
            OutK::chain(id)
        }
    }

    #[test]
    fn chain_chacha_round_trip() {
        use crate::lib::adhoc::connection::keyring::Key;
        let key = Key::new(1, std::array::from_fn(|i| i as u8), std::array::from_fn(|i| (i * 7) as u8));
        let o = |n: usize, s: u32| O {
            head: s as u8,
            l: L {
                a: s,
                s: text(n, s),
                z: u64::MAX - s as u64,
            },
            tail: 0x0102_0304_0506_0708 ^ s as u64,
        };
        for size in [16usize, 17, 23, 64, 1000, 70_000] {
            let os = vec![o(0, 1), o(10, 2), o(70_000, 3)];
            let qs = vec![Q { o: o(5, 5), end: 77 }, Q { o: o(150_000, 6), end: u32::MAX }];
            let mut q: Vec<Pk> = os.iter().cloned().map(Pk::O).collect();
            q.extend(qs.iter().cloned().map(Pk::Q));
            let mut st = TxState::new();
            st.keys.add(key.clone());
            let mut out = OutK(OutC::<false> { q, cur: None });
            let mut wire = Vec::new();
            let mut buf = vec![0u8; size];
            loop {
                let n = st.read(&mut buf, &mut out);
                if n < 0 {
                    break;
                }
                wire.extend_from_slice(&buf[..n as usize]);
            }
            let mut rs = RxState::new(1);
            rs.keys.add(key.clone());
            let mut inp = InK(InC::<false> {
                id: 0,
                o: O::default(),
                q: Q::default(),
                got_o: vec![],
                got_q: vec![],
            });
            for w in wire.chunks(size) {
                assert!(rs.write_all(w, &mut inp) >= 0, "buffer {size}: {:?}", rs.error);
            }
            assert!(inp.0.got_o == os, "buffer {size}");
            assert!(inp.0.got_q == qs, "buffer {size}");
            // the bytes are not in clear
            let probe = text(70_000, 3);
            assert!(!wire.windows(64).any(|w| w == &probe.as_bytes()[..64]), "buffer {size}: plaintext on the wire");
        }
    }

    /// Packs encrypted with a key the receiver does not have are dropped, the stream goes on: the
    /// packs before them, with a key it has, are received, and no error is raised.
    #[test]
    fn chain_chacha_unknown_key_drops_the_pack() {
        use crate::lib::adhoc::connection::keyring::Key;
        let known = Key::new(1, std::array::from_fn(|i| i as u8), std::array::from_fn(|i| (i * 7) as u8));
        let unknown = Key::new(2, std::array::from_fn(|i| (i * 3) as u8), std::array::from_fn(|i| (i * 5) as u8));
        let o = |n: usize, s: u32| O {
            head: s as u8,
            l: L {
                a: s,
                s: text(n, s),
                z: u64::MAX - s as u64,
            },
            tail: 0x0102_0304_0506_0708 ^ s as u64,
        };
        let os = vec![o(0, 1), o(10, 2), o(3000, 3)];
        let qs = vec![Q { o: o(5, 5), end: 77 }];
        let wire_of = |key: &Key| {
            let mut q: Vec<Pk> = os.iter().cloned().map(Pk::O).collect();
            q.extend(qs.iter().cloned().map(Pk::Q));
            let mut st = TxState::new();
            st.keys.add(key.clone());
            let mut out = OutK(OutC::<false> { q, cur: None });
            let mut wire = Vec::new();
            let mut buf = vec![0u8; 64];
            loop {
                let n = st.read(&mut buf, &mut out);
                if n < 0 {
                    break;
                }
                wire.extend_from_slice(&buf[..n as usize]);
            }
            wire
        };
        let mut wire = wire_of(&known);
        wire.extend(wire_of(&unknown));
        for size in [16usize, 64, 1000, 1 << 16] {
            let mut rs = RxState::new(1);
            rs.keys.add(known.clone());
            let mut inp = InK(InC::<false> {
                id: 0,
                o: O::default(),
                q: Q::default(),
                got_o: vec![],
                got_q: vec![],
            });
            for w in wire.chunks(size) {
                assert!(rs.write_all(w, &mut inp) >= 0, "buffer {size}: {:?}", rs.error);
            }
            assert!(rs.error.is_none(), "buffer {size}: {:?}", rs.error);
            assert!(inp.0.got_o == os, "buffer {size}: the packs of the known key, and only those");
            assert!(inp.0.got_q == qs, "buffer {size}");
        }
    }

    #[test]
    fn chain_stream_plain_round_trip() {
        stream_round_trip::<false>(&[16, 17, 23, 64, 1000, 70_000, 200_000]);
    }

    #[test]
    fn chain_stream_zstd_round_trip() {
        stream_round_trip::<true>(&[16, 17, 23, 64, 1000, 70_000, 200_000]);
    }

    /// One pack: a File field of `len` declared bytes, `have` of them in its source.
    struct OneFile {
        len: u64,
        have: usize,
        sent: bool,
    }
    impl TxHost for OneFile {
        fn on_serializing(&mut self) -> bool {
            !std::mem::replace(&mut self.sent, true)
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            let (len, have) = (self.len, self.have);
            dst.put_file(|| (Box::new(std::io::Cursor::new((0..have).map(|i| i as u8).collect::<Vec<u8>>())), len), i64::MAX, 0)
        }
        fn on_serialized(&mut self) {}
    }

    fn file_wire(len: u64, have: usize, window: usize) -> Vec<u8> {
        let mut st = TxState::new();
        let mut h = OneFile { len, have, sent: false };
        let mut buf = vec![0u8; window];
        let mut wire = vec![];
        loop {
            let n = st.read(&mut buf, &mut h);
            if n <= 0 {
                return wire;
            }
            wire.extend_from_slice(&buf[..n as usize]);
        }
    }

    /// A File field goes whole whatever the window: its length, then exactly its bytes.
    #[test]
    fn a_file_goes_whole() {
        for window in [1usize, 7, 300, 10_000] {
            let wire = file_wire(5000, 5000, window);
            assert_eq!(&wire[..2], &[0x88, 0x27], "window {window}: the varint of 5000");
            assert!(wire[2..].iter().enumerate().all(|(i, b)| *b == i as u8) && wire.len() == 5002, "window {window}");
        }
    }

    /// A source at its end before the declared length never makes it up: an error of the sender,
    /// not a pause for good.
    #[test]
    #[should_panic(expected = "ended with 10 of its bytes still owed")]
    fn a_file_whose_source_ends_early_is_an_error() {
        file_wire(20, 10, 100);
    }
}

pub mod broadcaster;
pub mod chunked;
pub mod framing;
pub mod on_error;
pub mod resumable;
mod stream;
mod string;
pub mod tunnel;
