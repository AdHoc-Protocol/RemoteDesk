// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Journaling and replay of guaranteed packet bytes.
//!
//! [`Resumable`] retains unacknowledged bytes in a hot memory buffer and segment files. A peer
//! acknowledgement releases the retained prefix; a resume request replays the journal from the
//! requested position. Positions count guaranteed bytes from a randomized session starting point.
//! Unguaranteed packets bypass the journal.
//!
//! The connection's [`source`] coordinates packet boundaries, replay, backpressure, and control
//! messages. Encoded packets can also be submitted through [`Resumable::send`]. Apply
//! acknowledgements and resume requests at packet boundaries. Incremental sources use
//! [`Resumable::journal`] and the available-room checks to bound unacknowledged output.
//!
//! Datagram transports use [`Resumable::reported`], [`Resumable::turn`], and
//! [`Resumable::replay_from_sent`] to acknowledge truncated positions and schedule replay. Stream
//! and datagram modes share journal storage and prefix release.
//!
//! While parked, a session flushes its hot buffer to disk. I/O failures are returned as `io::Error`
//! and invalidate the journal. Dropping the journal attempts to remove its segment files; abrupt
//! process termination can leave files behind.
//!
//! From creation, and after a loss that parks the session, the stage is awaiting: guaranteed packs
//! are journalled only, until the position the peer names is applied at a pack boundary.
//!
//! A session starts on a connection - the first one, or the next after a loss - in this order, and
//! nothing leaves the stage out of it:
//! 1. Identification. Until the session is known, nobody can tell whether something of it is parked
//!    and from where it goes on. The side that made the connection knows its peer and identifies its
//!    session at once ( `identify` / `reclaim` on connect ); then it sends its login - unguaranteed
//!    packs, the only ones that leave the stage before the two sides agreed. The side that took the
//!    connection identifies the session by the login ( or at the connect, when it knows the peer
//!    otherwise ).
//! 2. Agreement. From the identification on the side that took the connection nothing leaves its
//!    stage, guaranteed or not, but what agrees the positions: the packs of the control over a
//!    stream, the headers of the datagrams over a datagram transport. Each side names the position
//!    its decoder has got to in the peer's journal - a pack in progress counted - and each journal
//!    goes on from the position named. The side that made the connection sends nothing more either
//!    once it has heard the peer's position, until its answer is out ( over datagrams the position
//!    heard is the agreement itself: its journal goes on from it at once ).
//! 3. Only then do the packs flow: the replay of the journal first, then the queue.
//!
//! Why nothing in between: a parked session keeps the pack its decoder had in progress, and the
//! decoder takes the bytes that follow as the rest of that pack. A pack sent between the
//! identification and the agreement - even an unguaranteed one - would be glued into it, and the
//! stream would be broken from there on. For the same reason the receiving side takes nothing of
//! what comes before the position it named: the datagrams of a series that runs when it asks for a
//! new one ( `ask` of the UDP transport ), the packs that are not of the agreement over a stream (
//! the greeter of the receiver ). Over datagrams the first series of a connection is the one
//! exception: it holds the login and the unguaranteed packs behind it, starting with a pack, and a
//! decoder between two packs reads it on until the series asked for starts - one that holds a pack
//! in progress does not. Here the rule is kept by [`source`] - the side that took the
//! connection - and by the control's `agreeing` - over a stream.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::lib::adhoc::connection::control::{get_i32, get_i64, set_i32, set_i64, Control};


// Constants

/// Default segment: the most the buffer in memory grows to while the connection is open. 4 MB.
pub const SEGMENT: usize = 1 << 22;

/// The size of one file of the journal. 4 MB.
pub const FILE: usize = 1 << 22;

/// The buffer in memory at the start, and whenever the connection is closed. 2 KB.
pub const HOT: usize = 1 << 11;

/// The least room progress is made in.
pub const ROOM: usize = 16;

/// "No position" marker for the replay in progress.
pub const NONE: i64 = -1;

/// The largest admissible capacity: the positions of a session must stay within a signed 32-bit
/// offset of each other.
pub const MAX_CAPACITY: i64 = 1 << 31;

// Resumable

/// The journaling stage. Positions satisfy `base_ ≤ acked_ ≤ sent_ ≤ next_`:
///
/// * `base_` — the first byte still held (everything below is acked and given back);
/// * `acked_` — everything below is acknowledged by the peer;
/// * `sent_` — everything below was handed to the transport;
/// * `next_` — one past the last byte produced.
///
/// The files hold `[base_, hot_base())`, the hot buffer holds `[hot_base(), next_)`; every file but
/// the last is `file_size` bytes.
#[derive(Debug)]
pub struct Resumable {
    /// The largest backlog held, in bytes: the files plus the buffer in memory.
    capacity: i64,
    /// Size of one segment: the most the buffer in memory grows to.
    segment: usize,
    /// How many bytes the peer has not confirmed yet the connection holds at most: in memory, under
    /// a segment.
    in_flight: i64,
    /// The hot buffer's first size, and its size while parked.
    hot_size: usize,
    /// The size of one file of the journal.
    file_size: usize,

    base_: i64,
    acked_: i64,
    sent_: i64,
    next_: i64,

    /// The segment files, oldest first. The last may still be open for writing (`ch`).
    files: Vec<PathBuf>,
    /// The last file, open while it is still being written.
    ch: Option<File>,
    /// Bytes in the last file.
    written: usize,
    /// The buffer in memory, a ring: what it holds starts at `hot_off`, `hot_used` bytes, and goes round its end - a byte
    /// given back frees its place where it lies, nothing held is ever moved to make room. In the files of its session: the
    /// ring file mapped.
    hot: Hot,
    hot_off: usize,
    hot_used: usize,
    /// Open on the file a replay is reading, and its index in `files`.
    replay_ch: Option<File>,
    replay_index: Option<usize>,

    /// True until the peer's position is known: packets are journalled only, nothing guaranteed
    /// leaves the stage.
    awaiting_: bool,
    /// True while the position the peer named is not of this journal: packets are journalled only
    /// until the peer has been told where the stream goes on from.
    swallow: bool,
    /// True while there is no connection: the hot buffer does not grow, the journal lives on the
    /// disk.
    parked_: bool,
    /// Where the replay in progress started, [`NONE`] while none is.
    replaying: i64,

    /// Root of this instance's segment files.
    dir: PathBuf,

    /// How many replays began, and how many bytes they sent: a measure of what the losses cost.
    replays: u64,
    replayed: u64,

    /// `base_` stands on the start of a pack: nothing was given back in the middle of one yet.
    whole: bool,

    /// The transmitter of a guaranteed pack a lost socket cut in the middle: the next socket of the
    /// session, or the offline pull, goes on with it into the journal. Of the session: its end
    /// drops it.
    kept: Option<crate::lib::adhoc::connection::transmitter::TxState>,
    /// The number of the session: a step at its end ([`close`](Self::close) without keeping). A
    /// socket of a session over keeps nothing for the next.
    session: u32,

    /// The ends of the packs journalled above the base, in order. A stream confirms in the middle
    /// of a pack, but gives a pack back only once the confirmation passed its end: the journal
    /// starts at the start of a pack, so a peer that has none of the journal is told a position a
    /// pack starts at ([`resume_at`](Self::resume_at)).
    ends: std::collections::VecDeque<i64>,
    /// The end of the last pack the confirmation passed: the journal may start there.
    whole_end: i64,
    /// A datagram transport runs this stage: the peer may take the stream up from the middle of a
    /// pack.
    datagram: bool,

    /// The header of the files of the session ([`Control`]), mapped: every position is stored into it where it changes - see
    /// the region of the files of the session. Null: the journal lives in memory, its backlog in files of this process.
    head: *mut u8,
    /// `<dir>/<name>`: `<name>.ring` - the ring, mapped -, `<name>.<position>.seg` - the backlog beyond it.
    base_file: PathBuf,
    /// The journal left the files of its session for the next process ([`stop`](Self::stop)): they are not deleted here.
    stopped: bool,

    /// `resumable_sync_ms` of the description: -1 - only the fall of the process is outlived ([`sync`](Self::sync)).
    sync_ms: i32,
    sync_period: std::time::Duration,
    /// The bytes of the journal below are on the disk.
    synced: i64,
    /// When the last sync was.
    synced_at: std::time::Instant,
    /// The ring is laid out otherwise than the disk has it: what it holds goes through whole.
    sync_all: bool,
    /// The ring as the header on the disk has it: the positions it holds, where they start in it, its size.
    d_hot_base: i64,
    d_upto: i64,
    d_off: usize,
    d_cap: usize,
    /// The ring file and the header, written through to the disk.
    ring_through: Option<File>,
    head_through: Option<File>,
}

// SAFETY: the journal is reached by one thread at a time ([`Journal`] `owner`); `head` and the mapping of `hot` are pages of
// the process that live as long as their mappings, which the journal and its session keep
unsafe impl Send for Resumable {}

impl Resumable {
    /// A stage with the default journal root (`temp_dir/AdHoc/journal/<pid>/`) and a random start
    /// position in `[1<<48, 1<<61)`.
    ///
    /// # Panics
    ///
    /// If `capacity > MAX_CAPACITY`.
    pub fn new(capacity: i64) -> Self {
        Self::with_dir(capacity, default_dir())
    }

    /// As [`new`](Self::new), with an explicit journal root — embedders that isolate journals per
    /// session, and tests.
    pub fn with_dir(capacity: i64, dir: PathBuf) -> Self {
        Self::with_geometry(capacity, dir, seed_start(), HOT, FILE, SEGMENT)
    }

    /// As [`with_dir`](Self::with_dir), with an explicit start position instead of a random one.
    /// For tests and for embedders that seed positions themselves.
    pub fn with_start(capacity: i64, dir: PathBuf, start: i64) -> Self {
        Self::with_geometry(capacity, dir, start, HOT, FILE, SEGMENT)
    }

    /// At most `bytes` the peer has not confirmed yet are in flight: at so many no pack is taken
    /// from the queue until the peer confirms. What is in flight when a socket is lost is sent
    /// again on the next one. Under a segment.
    pub fn in_flight(mut self, bytes: i64) -> Self {
        self.in_flight = bytes.min(self.segment as i64 - (self.segment as i64 >> 3));
        self
    }

    /// Full constructor, geometry included. Hidden: the geometry is a protocol constant; tests
    /// shrink it to exercise the file paths without megabytes of I/O.
    #[doc(hidden)]
    pub fn with_geometry(
        capacity: i64,
        dir: PathBuf,
        start: i64,
        hot_size: usize,
        file_size: usize,
        segment: usize,
    ) -> Self {
        assert!(
            capacity <= MAX_CAPACITY,
            "the positions of a session must stay within a signed 32-bit offset of each other"
        );
        assert!(ROOM <= hot_size && hot_size <= segment && hot_size <= file_size);
        Self {
            capacity,
            segment,
            in_flight: segment as i64 - (segment as i64 >> 3),
            hot_size,
            file_size,
            base_: start,
            acked_: start,
            sent_: start,
            next_: start,
            files: Vec::new(),
            ch: None,
            written: 0,
            hot: Hot::mem(hot_size),
            hot_off: 0,
            hot_used: 0,
            replay_ch: None,
            replay_index: None,
            awaiting_: true,
            swallow: false,
            parked_: true,
            replaying: NONE,
            dir,
            replays: 0,
            replayed: 0,
            whole: true,
            kept: None,
            session: 0,
            ends: std::collections::VecDeque::new(),
            whole_end: start,
            datagram: false,
            head: std::ptr::null_mut(),
            base_file: PathBuf::new(),
            stopped: false,
            sync_ms: -1,
            sync_period: std::time::Duration::ZERO,
            synced: start,
            synced_at: std::time::Instant::now(),
            sync_all: false,
            d_hot_base: start,
            d_upto: start,
            d_off: 0,
            d_cap: 0,
            ring_through: None,
            head_through: None,
        }
    }

    /// `resumable_sync_ms` of the description: the journal outlives the power too - it goes to the disk, written through, when
    /// a pack is journalled whole and the last sync is so many milliseconds old, and when the queue is empty ( 0 - every pack ).
    /// -1 - only the fall of the process is outlived.
    pub fn sync_ms(mut self, ms: i32) -> Self {
        self.sync_ms = ms;
        self.sync_period = std::time::Duration::from_millis(ms.max(0) as u64);
        self
    }

    // Positions

    /// The first byte still held.
    #[inline]
    pub fn base(&self) -> i64 {
        self.base_
    }
    /// Everything below is acknowledged by the peer.
    #[inline]
    pub fn acked(&self) -> i64 {
        self.acked_
    }
    /// Everything below was handed to the transport.
    #[inline]
    pub fn sent(&self) -> i64 {
        self.sent_
    }
    /// One past the last byte produced.
    #[inline]
    pub fn next(&self) -> i64 {
        self.next_
    }
    /// The backlog held, in bytes: `next_ - base_`.
    #[inline]
    pub fn backlog(&self) -> i64 {
        self.next_ - self.base_
    }
    /// The largest backlog this stage holds.
    #[inline]
    pub fn capacity(&self) -> i64 {
        self.capacity
    }
    /// How many replays began / how many bytes they sent.
    #[inline]
    pub fn replays(&self) -> (u64, u64) {
        (self.replays, self.replayed)
    }
    /// True while nothing guaranteed leaves the stage (waiting for the peer's position, or
    /// swallowing until the peer is told).
    #[inline]
    pub fn is_holding(&self) -> bool {
        self.awaiting_ || self.swallow
    }
    /// True while the position the peer names is not applied yet: the agreement of the order in the
    /// description of this module is not over.
    #[inline]
    pub fn is_awaiting(&self) -> bool {
        self.awaiting_
    }

    /// At a read of the source: while holding nothing is replayed and what the transmitter produces
    /// is journalled only - what went out is the whole stream, so a report of the peer up to its
    /// end is taken.
    #[inline]
    pub(crate) fn hold_at_next(&mut self) {
        if self.is_holding() {
            self.sent_ = self.next_;
        }
    }

    /// The position the replay in progress started from; [`NONE`] — none.
    #[inline]
    pub(crate) fn replaying(&self) -> i64 {
        self.replaying
    }

    /// The journal holds `position`: the stream can go on from it.
    #[inline]
    pub(crate) fn holds(&self, position: i64) -> bool {
        self.base_ <= position && position <= self.next_
    }

    /// A replay is due: the answer to the peer's position goes right before its bytes - they may go
    /// on with a pack the peer holds in part -, a report waits behind them.
    #[inline]
    pub(crate) fn replay_due(&self) -> bool {
        self.replaying != NONE
    }

    /// A replay is under way and part of it is out: it may stop in the middle of a pack, no record
    /// goes between its bytes.
    #[inline]
    pub(crate) fn replay_begun(&self) -> bool {
        self.replaying != NONE && self.replaying < self.sent_
    }
    /// True while there is no connection.
    #[inline]
    pub fn is_parked(&self) -> bool {
        self.parked_
    }

    // Sending encoded packets through the journal.

    /// Submits an encoded guaranteed packet in windows of at most `window.max(1)` bytes. Each
    /// window is journalled before being passed to `sink`. While holding, bytes are journalled
    /// without being sent.
    ///
    /// Returns `Ok(false)` without consuming input when capacity or the in-flight limit requires
    /// backpressure. Control records can still bypass the journal. Capacity is checked before the
    /// packet; a packet already started may exceed it.
    ///
    /// A journal write error invalidates storage. A sink error stops immediately, retaining only
    /// the prefix appended so far, including the rejected window. The remaining packet suffix has
    /// not been journalled; the caller must preserve it separately if serialization is to continue.
    pub fn send<F>(&mut self, packet: &[u8], window: usize, mut sink: F) -> io::Result<bool>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        if !self.swallow {
            // The announcement of a lost position gets out even at the cap — while swallowing there
            // is no limit.
            let free = self.capacity - (self.next_ - self.acked_);
            if free < ROOM as i64 {
                return Ok(false); // the cap: a pack waits where it is
            }
            // The connection is open: what the peer has not reported is held in memory, a segment
            // of it. Counted from what is acknowledged, not from the base. Not while the peer has
            // not named its position on this socket: nothing leaves then anyway, and the queue
            // behind - the login that lets the peer know this side - must move.
            if !self.awaiting_ && self.in_flight <= self.next_ - self.acked_ {
                return Ok(false); // wait for a report
            }
        }
        let emit = !self.is_holding();
        for bytes in packet.chunks(window.max(1)) {
            if let Err(e) = self.append(bytes) {
                // A failed write ends the session.
                self.drop_journal();
                return Err(e);
            }
            if emit {
                sink(bytes)?;
            }
            self.sent_ = self.next_;
        }
        self.pack_ended()?;
        Ok(true)
    }

    // ack / resume — what the peer says (stream transport)

    /// Acknowledges guaranteed bytes below `position` and releases their journal storage. Positions
    /// outside the retained journal are ignored. Acknowledgement is capped at `sent_`.
    pub fn ack(&mut self, position: i64) {
        if position < self.base_ || self.next_ < position {
            return;
        }
        if self.acked_ < position {
            self.acked_ = position.min(self.sent_);
            self.release();
        }
    }

    /// Replays retained bytes from `position` into `sink`.
    ///
    /// Returns `Ok(-1)` after a valid replay. If the position is outside the journal, output is
    /// suppressed and the returned position must be sent to the peer in a loss record. Call
    /// `resume` again with that position after sending the record. Invoke between packet sends.
    ///
    /// Journal read errors and errors returned by `sink` propagate as `io::Error`.
    pub fn resume<F>(&mut self, position: i64, mut sink: F) -> io::Result<i64>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        if position < self.base_ || self.next_ < position {
            self.swallow = true;
            self.awaiting_ = false;
            self.replaying = NONE;
            self.close_replay();
            return Ok(if self.next_ < position {
                self.next_
            } else {
                self.base_
            });
        }
        self.sent_ = position;
        self.replaying = position;
        self.replays += 1;
        if position < self.acked_ {
            self.acked_ = position;
        }
        if position < self.next_ {
            self.replay(&mut sink)?;
        }
        self.replaying = NONE;
        self.awaiting_ = false;
        self.swallow = false;
        Ok(-1)
    }

    /// Nobody to wait for: the stream continues from the current position, as if the peer had named
    /// it.
    pub fn resume_here<F>(&mut self, sink: F) -> io::Result<i64>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        let next = self.next_;
        self.resume(next, sink)
    }

    // datagram transport
    //
    // A datagram transport runs this stage itself: its loop is the transmit runner and the reader
    // of the peer's reports at once, so what the peer says is applied where it is heard. The peer
    // names bytes, not packs: how many bytes of this journal it has received. The journal is given
    // back up to that byte, and after a loss the stream goes on from that byte — in the middle of a
    // pack, if the loss cut one. Where the packs start the stage does not know and does not need
    // to.

    /// Acknowledges the datagram position represented by the low 32 bits in `pos`.
    ///
    /// Expands the position relative to `next_` using signed wrapping subtraction, caps it at
    /// `next_`, and releases the acknowledged prefix. If replay has moved `sent_` behind the
    /// acknowledgement, advances it so those bytes are not sent again.
    pub fn reported(&mut self, pos: i32) {
        self.datagram = true;
        let at = (self.next_ + pos.wrapping_sub(self.next_ as i32) as i64).min(self.next_);
        if at <= self.acked_ {
            return;
        }
        self.acked_ = at;
        if self.sent_ < at {
            self.sent_ = at; // the stream was turned back past what the peer holds: that is not sent again
        }
        self.whole = false;
        self.release();
    }

    /// Selects the journal position for subsequent datagram replay.
    ///
    /// Returns `-1` when `position` is retained; replay is then available through
    /// [`replay_from_sent`](Self::replay_from_sent). Otherwise returns a replacement packet
    /// boundary to report to the peer. If the retained prefix starts inside a packet, discards that
    /// incomplete prefix first. The caller must finish an active guaranteed packet before turning
    /// the journal.
    pub fn turn(&mut self, position: i64) -> i64 {
        self.awaiting_ = false;
        self.swallow = false;
        if position < self.base_ || self.next_ < position {
            self.close_replay();
            self.replaying = NONE;
            if !self.whole {
                // What is held starts in the middle of a pack the peer has no head of.
                self.discard();
                self.base_ = self.next_;
                self.whole = true;
            }
            self.acked_ = self.base_;
            self.sent_ = self.base_;
            return self.base_;
        }
        if position < self.acked_ {
            self.acked_ = position;
        }
        self.close_replay();
        self.sent_ = position;
        self.replaying = position;
        if position < self.next_ {
            self.replays += 1;
        } else {
            self.replaying = NONE;
        }
        -1
    }

    /// Copies replay backlog `[sent_, next_)` into `sink`, advancing `sent_` as bytes are
    /// delivered.
    pub fn replay_from_sent<F>(&mut self, sink: F) -> io::Result<()>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        self.replay(sink)
    }

    /// True while `base_` stands on the start of a pack.
    #[inline]
    pub fn is_whole(&self) -> bool {
        self.whole
    }

    // The source first serves replay at packet boundaries, then journals guaranteed bytes as the
    // transmitter produces them. Peer acknowledgements and resume requests are applied before
    // starting the next packet.

    /// Room for one more pack: below the cap, and the unacknowledged bytes below most of a segment
    /// - not while the peer has not named its position on this socket: nothing guaranteed leaves
    /// then anyway, and what does - the login that lets the peer know this side - must. While
    /// swallowing — always: the announcement of a lost position gets out.
    pub fn has_room(&self) -> bool {
        self.swallow
            || ROOM as i64 <= self.capacity - (self.next_ - self.acked_)
                && (self.awaiting_ || self.next_ - self.acked_ < self.in_flight)
    }

    /// The bytes a read of the transmitter may produce: what is left under the cap - a pack in the
    /// middle too waits there. While swallowing - no limit: the announcement of a lost position
    /// gets out.
    pub fn free(&self) -> usize {
        if self.swallow { usize::MAX } else { (self.capacity - (self.next_ - self.acked_)).max(0) as usize }
    }

    /// Bytes of a guaranteed pack as the transmitter produces them: into the journal. `true` — they
    /// go on to the transport too; `false` — held: the peer's position is not known yet, or it is
    /// told the stream goes on elsewhere.
    pub fn journal(&mut self, bytes: &[u8]) -> io::Result<bool> {
        if let Err(e) = self.append(bytes) {
            self.drop_journal(); // a failed write ends the session
            return Err(e);
        }
        if self.is_holding() || self.sent_ + bytes.len() as i64 != self.next_ {
            return Ok(false); // a replay is due before them: they go out with it
        }
        self.sent_ = self.next_;
        Ok(true)
    }

    /// The peer names the position it wants the stream continued from (`CONTINUE`). `-1` — the
    /// journal holds it: the replay of `[position, next_)` is due
    /// ([`replay_into`](Self::replay_into)). Otherwise the position to tell the peer (`LOST`), and
    /// nothing guaranteed leaves the stage until [`resume_at`](Self::resume_at) with it.
    pub fn resume_at(&mut self, position: i64) -> i64 {
        if position < self.base_ || self.next_ < position {
            self.swallow = true;
            self.awaiting_ = false;
            self.replaying = NONE;
            self.close_replay();
            return if self.next_ < position { self.next_ } else { self.base_ };
        }
        self.sent_ = position;
        if position < self.acked_ {
            self.acked_ = position;
        }
        self.awaiting_ = false;
        self.swallow = false;
        if position < self.next_ {
            self.replaying = position;
            self.replays += 1;
        }
        -1
    }

    /// The replay that is due, as much as `out` takes (from `sent_`). 0 — none.
    pub fn replay_into(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.next_ <= self.sent_ || self.is_holding() || out.is_empty() {
            if self.replaying != NONE {
                self.replaying = NONE;
                self.close_replay();
            }
            return Ok(0);
        }
        let p = self.sent_;
        let hot_base = self.hot_base();
        let n = if p < hot_base {
            match self.replay_file(out, p) {
                Ok(n) => n,
                Err(e) => {
                    self.drop_journal(); // a failed read ends the session
                    return Err(e);
                }
            }
        } else {
            self.replay_hot(out, p, hot_base)
        };
        self.sent_ += n as i64;
        self.replayed += n as u64;
        Ok(n)
    }

    // close / park / online — the connection goes and comes

    /// A new socket of the session: what the peer has not acknowledged goes again, from the
    /// position it names (its `CONTINUE`); until then nothing guaranteed leaves the stage. The
    /// journal stays where it is.
    pub fn rewind(&mut self) {
        self.awaiting_ = true;
        self.swallow = false;
        self.sent_ = self.acked_;
        self.replaying = NONE;
        self.close_replay();
    }

    /// Closes the current transport session.
    ///
    /// With `keep_session`, retains unacknowledged bytes, rewinds `sent_` to `acked_`, parks
    /// storage, and awaits the peer position after reconnecting. Otherwise clears the journal and
    /// chooses new session positions. Socket and protocol teardown remain the caller
    /// responsibility.
    pub fn close(&mut self, keep_session: bool) -> io::Result<()> {
        self.parked_ = true;
        self.awaiting_ = true;
        self.swallow = false;
        self.sent_ = self.acked_;
        self.replaying = NONE;
        self.close_replay();
        if keep_session {
            // A failed write ends the session.
            if let Err(e) = self.park() {
                self.drop_journal();
                return Err(e);
            }
        } else {
            self.kept = None; // a pack cut in the middle is of the session over: the next session starts on a pack boundary
            self.session = self.session.wrapping_add(1);
            self.detach(true); // the session is over: the files of its journal go ( Control::unbind deletes the rest )
            self.drop_journal();
        }
        Ok(())
    }

    /// A guaranteed pack the end of a socket cut: its transmitter waits for the next socket of the
    /// session.
    pub(crate) fn keep(&mut self, tx: crate::lib::adhoc::connection::transmitter::TxState) {
        self.kept = Some(tx);
    }

    /// The transmitter a former socket of the session left in the middle of a guaranteed pack:
    /// taken.
    pub(crate) fn kept(&mut self) -> Option<crate::lib::adhoc::connection::transmitter::TxState> {
        self.kept.take()
    }

    /// The number of the session ([`session`](Self::session) field).
    #[inline]
    pub(crate) fn session(&self) -> u32 {
        self.session
    }

    /// Moves a retained session to parked storage.
    ///
    /// Flushes an expanded hot buffer to files and restores its initial size. Data that fits in the
    /// initial buffer can remain in memory.
    pub fn park(&mut self) -> io::Result<()> {
        if self.hot.len() == self.hot_size {
            return Ok(());
        }
        if self.hot_size - ROOM < self.hot_used || !self.head.is_null() {
            while 0 < self.hot_used {
                self.write()?;
            }
        }
        if !self.head.is_null() {
            return self.shrink(); // the ring file takes its first size: what it held is in the files
        }
        let mut first = Hot::mem(self.hot_size);
        self.hot_into(&mut first);
        self.hot = first;
        Ok(())
    }

    /// Marks the transport online, allowing the hot buffer to grow up to one segment.
    pub fn online(&mut self) {
        self.parked_ = false;
    }


    // The journal itself

    /// The position of the first byte in `hot`: past every file, the last one as far as it is
    /// written.
    fn hot_base(&self) -> i64 {
        if self.files.is_empty() {
            self.base_
        } else {
            self.base_
                + (self.files.len() - 1) as i64 * self.file_size as i64
                + self.written as i64
        }
    }

    /// The place in the ring of the byte `i` places past its start.
    #[inline(always)]
    fn hot_at(&self, i: usize) -> usize {
        let at = self.hot_off + i;
        if at < self.hot.len() { at } else { at - self.hot.len() }
    }

    /// What the ring holds, in order, to the front of `to` - at most two copies: its part up to the end of the ring, the part
    /// round it. The ring starts at 0 then.
    fn hot_into(&mut self, to: &mut [u8]) {
        let first = self.hot_used.min(self.hot.len() - self.hot_off);
        to[..first].copy_from_slice(&self.hot[self.hot_off..self.hot_off + first]);
        to[first..self.hot_used].copy_from_slice(&self.hot[..self.hot_used - first]);
        self.hot_off = 0;
    }

    /// Appends `bytes` to the hot buffer, growing or spilling it as necessary: at most two copies a call, where the free place
    /// of the ring runs round its end.
    fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.hot_used == self.hot.len() {
                self.flush()?;
            }
            let at = self.hot_at(self.hot_used);
            let n = (self.hot.len() - self.hot_used).min(self.hot.len() - at).min(bytes.len());
            if 0 <= self.sync_ms && !self.head.is_null() {
                self.overwrite(at, n)?; // the disk names that place as the journal's: it gets the journal first
            }
            self.hot[at..at + n].copy_from_slice(&bytes[..n]);
            self.hot_used += n;
            self.next_ += n as i64;
            bytes = &bytes[n..];
        }
        Ok(())
    }

    /// The ring is full: online it doubles, up to one segment; else - parked, a segment - its head goes to the disk.
    fn flush(&mut self) -> io::Result<()> {
        if !self.parked_ && self.hot.len() < self.segment {
            if !self.head.is_null() {
                return self.grow((self.hot.len() << 1).min(self.segment));
            }
            let mut to = Hot::mem((self.hot.len() << 1).min(self.segment));
            self.hot_into(&mut to);
            self.hot = to;
            return Ok(());
        }
        self.write()
    }

    /// Writes the hot buffer head to the last journal file, creating a file when needed: its place in the ring is free then.
    fn write(&mut self) -> io::Result<()> {
        if self.ch.is_none() {
            let (file, ch) = if self.head.is_null() {
                std::fs::create_dir_all(&self.dir)?;
                let file = self.dir.join(format!("{:016x}.seg", next_file_tag()));
                let ch = OpenOptions::new().read(true).write(true).create_new(true).open(&file)?;
                (file, ch)
            } else {
                // the file of the session that starts at the position of the head of the ring: on the disk as it is written,
                // when the journal outlives the power
                let file = self.seg(self.hot_base());
                let mut o = OpenOptions::new();
                o.read(true).write(true).create(true).truncate(true);
                if 0 <= self.sync_ms {
                    os::through(&mut o);
                }
                let ch = o.open(&file)?;
                (file, ch)
            };
            self.ch = Some(ch);
            self.files.push(file);
            self.written = 0;
        }
        let n = self.hot_used.min(self.file_size - self.written);
        let first = n.min(self.hot.len() - self.hot_off); // up to the end of the ring, the rest round it
        let ch = self.ch.as_mut().unwrap();
        ch.write_all(&self.hot[self.hot_off..self.hot_off + first])?;
        ch.write_all(&self.hot[..n - first])?;
        self.written += n;
        if self.written == self.file_size {
            self.ch = None; // full: the next write starts a new file
        }
        self.hot_off = self.hot_at(n);
        self.hot_used -= n;
        let h = self.head;
        if !h.is_null() {
            // SAFETY: the header is mapped while `head` is set
            unsafe {
                // the bytes are in the file: the ring starts past them
                set_i64(h, Control::H_HOT_BASE, self.hot_base());
                set_i32(h, Control::H_HOT_OFF, self.hot_off as i32);
            }
        }
        Ok(())
    }

    /// Copies journal bytes starting at `sent_` into `sink`, reading from segment files or the hot
    /// buffer.
    fn replay<F>(&mut self, mut sink: F) -> io::Result<()>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        let mut chunk = vec![0u8; 64 * 1024];
        while self.sent_ < self.next_ {
            let p = self.sent_;
            let hot_base = self.hot_base();
            let n = if p < hot_base {
                match self.replay_file(&mut chunk, p) {
                    Ok(n) => n,
                    Err(e) => {
                        // A failed read ends the session.
                        self.drop_journal();
                        return Err(e);
                    }
                }
            } else {
                self.replay_hot(&mut chunk, p, hot_base)
            };
            // A transport failure does not touch the journal — the replay resumes from sent_ on the
            // next connection.
            sink(&chunk[..n])?;
            self.sent_ += n as i64;
            self.replayed += n as u64;
        }
        self.replaying = NONE;
        self.close_replay();
        Ok(())
    }

    fn replay_hot(&mut self, chunk: &mut [u8], p: i64, hot_base: i64) -> usize {
        let rel = (p - hot_base) as usize;
        let n = chunk.len().min(self.hot_used - rel);
        let at = self.hot_at(rel);
        let first = n.min(self.hot.len() - at); // up to the end of the ring, the rest round it
        chunk[..first].copy_from_slice(&self.hot[at..at + first]);
        chunk[first..n].copy_from_slice(&self.hot[..n - first]);
        n
    }

    /// Reads file-backed replay through a cached read handle. The current write file may be opened
    /// simultaneously for replay.
    fn replay_file(&mut self, chunk: &mut [u8], p: i64) -> io::Result<usize> {
        let rel = p - self.base_;
        let index = (rel / self.file_size as i64) as usize;
        let off = (rel % self.file_size as i64) as u64;
        let last = index == self.files.len() - 1;
        let avail = (if last { self.written } else { self.file_size }) as u64 - off;
        let n = (chunk.len() as u64).min(avail) as usize;
        if self.replay_index != Some(index) {
            self.replay_ch = Some(File::open(&self.files[index])?);
            self.replay_index = Some(index);
        }
        let f = self.replay_ch.as_mut().unwrap();
        f.seek(SeekFrom::Start(off))?;
        f.read_exact(&mut chunk[..n])?;
        Ok(n)
    }

    /// Releases bytes below `acked_` and advances `base_` to the first retained byte.
    ///
    /// Attempts to delete fully acknowledged files, then releases an acknowledged prefix of the hot
    /// buffer when no files remain.
    ///
    /// In the files of the session: the end of the pack goes to the header - the next process takes the journal up to it -,
    /// and the disk gets the journal when the last sync is `sync_ms` old. A failed sync ends the session, as a failed write.
    #[inline]
    pub fn pack_ended(&mut self) -> io::Result<()> {
        self.ends.push_back(self.next_);
        let h = self.head;
        if h.is_null() {
            return Ok(());
        }
        // SAFETY: the header is mapped while `head` is set
        unsafe {
            set_i64(h, Control::H_NEXT, self.next_); // the end of the last pack journalled whole
            set_i32(h, Control::H_OPEN, 0); // the pack begun is the journal's
        }
        if 0 <= self.sync_ms && self.sync_period <= self.synced_at.elapsed() {
            if let Err(e) = self.sync() {
                self.drop_journal();
                return Err(e);
            }
        }
        Ok(())
    }

    /// The queue is empty: what the last syncs passed by goes to the disk now. A failed sync ends the session.
    pub(crate) fn drained(&mut self) -> io::Result<()> {
        if self.sync_ms < 0 || self.head.is_null() || self.next_ <= self.synced {
            return Ok(());
        }
        let r = self.sync();
        if r.is_err() {
            self.drop_journal();
        }
        r
    }

    /// Where the journal may start: the end of the last pack the confirmation passed - over
    /// datagrams, the confirmation itself.
    fn releasable(&mut self) -> i64 {
        while let Some(&end) = self.ends.front() {
            if self.acked_ < end {
                break;
            }
            self.whole_end = end;
            self.ends.pop_front();
        }
        if self.datagram { self.acked_ } else { self.whole_end.max(self.base_) }
    }

    fn release(&mut self) {
        self.release_();
        if !self.head.is_null() {
            self.store();
        }
    }

    fn release_(&mut self) {
        let upto = self.releasable();
        if upto == self.next_ {
            // everything is acknowledged: hold nothing
            self.discard();
            self.base_ = self.next_;
            return;
        }
        let file = self.file_size as i64;
        // The full files below the acknowledged position; the one still being written stays.
        while !self.files.is_empty()
            && (self.ch.is_none() || 1 < self.files.len())
            && self.base_ + file <= upto
        {
            if self.replay_index == Some(0) {
                self.close_replay(); // an open file cannot be deleted on Windows
            }
            delete_file(&self.files[0]);
            self.files.remove(0);
            self.base_ += file;
            if let Some(i) = self.replay_index {
                if 0 < i {
                    self.replay_index = Some(i - 1);
                }
            }
        }
        if !self.files.is_empty() {
            let in_memory = self.hot_base();
            if upto < in_memory {
                return;
            }
            // Everything the files hold is acknowledged: the one still being written goes too.
            self.discard_files();
            self.base_ = in_memory;
        }
        if upto <= self.base_ {
            return;
        }
        // Given back where it lies: its place in the ring is free, what stays is not moved.
        let k = (upto - self.base_) as usize;
        self.hot_off = self.hot_at(k);
        self.hot_used -= k;
        self.base_ = upto;
    }

    fn close_replay(&mut self) {
        self.replay_ch = None;
        self.replay_index = None;
    }

    /// Let go of everything held: the files, the file being written, the buffer in memory.
    fn discard(&mut self) {
        self.discard_files();
        self.hot_off = 0;
        self.hot_used = 0;
    }

    fn discard_files(&mut self) {
        self.close_replay();
        self.ch = None;
        for f in self.files.drain(..) {
            delete_file(&f);
        }
        self.written = 0;
    }

    /// Clears the journal and moves positions beyond any bytes the peer could retain from this
    /// session. Waits for a new peer position before guaranteed output resumes.
    fn drop_journal(&mut self) {
        self.discard();
        if self.hot.len() != self.hot_size {
            if self.head.is_null() {
                self.hot = Hot::mem(self.hot_size); // the session is over
            } else if self.shrink().is_err() {
                self.detach(true); // the ring file failed: the journal goes on in memory
            }
        }
        self.base_ = self.next_ + 1;
        self.acked_ = self.base_;
        self.sent_ = self.base_;
        self.next_ = self.base_;
        self.whole_end = self.base_;
        self.ends.clear();
        self.awaiting_ = true;
        self.swallow = false;
        self.whole = true; // `drop()`: base stands on a pack start again
        if self.head.is_null() {
            return;
        }
        self.store();
        if 0 <= self.sync_ms {
            let _ = self.sync(); // the next sync writes it
        }
    }

    // The files of the session
    //
    // The journal lives in the files of its session ( Control ): <name>.ring is the ring mapped - the transmitter's bytes are
    // copied into the file's pages, nothing more -, the backlog beyond it is in <name>.<position>.seg, and every position is
    // stored into the header where it changes - the end of the journal once a pack is journalled whole ( pack_ended ), the
    // rest where bytes are given back or move to a file. A process that falls leaves all of it with the system, and the next
    // one takes the journal up to the end of the last pack journalled whole: a pack it had in part is not of the journal - a
    // peer that has a part of it is told so ( Lost_ ) and drops it. A pack is copied into the journal before the transport
    // writes it to the socket, so the peer has nothing past that end but such a part. The layout of the header is the one of
    // the Java lib.

    /// The generated `save`: the journal is in the files of the session, nothing of it goes to `out`.
    pub fn save(&mut self, _out: &mut Vec<u8>) -> io::Result<bool> {
        Ok(true)
    }

    /// The generated `load`: the journal is taken up from the files of the session, by [`Control::load`].
    pub fn load(&mut self, _from: &mut &[u8]) -> io::Result<()> {
        Ok(())
    }

    /// The journal is in the files of its session.
    #[inline]
    pub(crate) fn files_kept(&self) -> bool {
        !self.head.is_null()
    }

    /// `<base_file><suffix>`.
    fn named(&self, suffix: &str) -> PathBuf {
        let mut s = self.base_file.clone().into_os_string();
        s.push(suffix);
        PathBuf::from(s)
    }

    /// The file of the journal that starts at `position`.
    fn seg(&self, position: i64) -> PathBuf {
        self.named(&format!(".{position:x}.seg"))
    }

    /// The end of the last pack journalled whole: where the next process takes the journal up to.
    #[inline]
    fn whole_next(&self) -> i64 {
        self.ends.back().copied().unwrap_or(self.whole_end).max(self.base_)
    }

    /// The positions to the header: after a step that moved more than the end of the journal.
    fn store(&self) {
        let h = self.head;
        // SAFETY: the caller has the header mapped ( `head` set )
        unsafe {
            set_i64(h, Control::H_BASE, self.base_);
            set_i64(h, Control::H_ACKED, self.acked_);
            set_i64(h, Control::H_NEXT, self.whole_next());
            set_i64(h, Control::H_HOT_BASE, self.hot_base());
            set_i32(h, Control::H_HOT_OFF, self.hot_off as i32);
            set_i32(h, Control::H_HOT_CAP, self.hot.len() as i32);
            set_i64(h, Control::H_WHOLE_END, self.whole_end);
            set_i32(h, Control::H_JFLAGS, self.whole as i32 | (self.datagram as i32) << 1);
        }
    }

    /// The ring grows to `cap` where it is: the ring file mapped anew at that size, the head of the ring that went round its
    /// end follows its tail, past the old end - what was is not touched, so a process that falls meanwhile leaves the ring as
    /// the header has it.
    fn grow(&mut self, cap: usize) -> io::Result<()> {
        let was = self.hot.len();
        let round = (self.hot_off + self.hot_used).saturating_sub(was);
        let mut to = Hot::mapped(Map::open(&self.named(".ring"), cap, false)?);
        if 0 < round {
            to.copy_within(0..round, was);
        }
        self.hot = to; // the old mapping goes: the new one has its bytes, they are the file's
        self.sync_all = true; // laid out anew: the next sync writes what the ring holds whole
        // SAFETY: the header is mapped while `head` is set
        unsafe { set_i32(self.head, Control::H_HOT_CAP, cap as i32) };
        Ok(())
    }

    /// The ring file takes its first size: the ring is empty - its bytes went to the files.
    fn shrink(&mut self) -> io::Result<()> {
        if 0 <= self.sync_ms {
            self.sync()?; // the disk has the journal in the files before the ring it named goes
        }
        self.hot = Hot::mem(0); // the mapping goes before the file is cut
        self.hot_off = 0;
        self.hot = Hot::mapped(Map::open(&self.named(".ring"), self.hot_size, true)?);
        self.store();
        if 0 <= self.sync_ms {
            self.sync()?;
        }
        Ok(())
    }

    /// The files of the session are mapped ( [`Control`] attach ). A fresh journal moves what it holds into them; one of the
    /// process before is taken up from them when the header can be trusted, and starts anew otherwise - a peer that names a
    /// position of it is told it is lost.
    pub(crate) fn attach(&mut self, head: *mut u8, base_file: PathBuf, restoring: bool, trusted: bool) -> io::Result<()> {
        if !self.head.is_null() {
            return Ok(());
        }
        self.base_file = base_file;
        let ring_file = self.named(".ring");
        if restoring && trusted && self.reopen(head, &ring_file)? {
            return Ok(());
        }
        if restoring {
            for start in self.starts() {
                delete_file(&self.seg(start)); // the files of a journal that is not trusted
            }
        }
        if !self.files.is_empty() {
            // what went to files before the session was named goes under its name
            self.close_replay();
            let writing = self.ch.take().is_some();
            for i in 0..self.files.len() {
                let f = self.seg(self.base_ + (i * self.file_size) as i64);
                if std::fs::rename(&self.files[i], &f).is_err() {
                    std::fs::copy(&self.files[i], &f)?; // another volume
                    delete_file(&self.files[i]);
                }
                self.files[i] = f;
            }
            if writing {
                let mut ch = self.last(self.files.last().unwrap())?;
                ch.seek(SeekFrom::Start(self.written as u64))?;
                self.ch = Some(ch);
            }
        }
        let mut to = Hot::mapped(Map::open(&ring_file, self.hot.len(), true)?);
        self.hot_into(&mut to);
        self.hot = to;
        self.head = head;
        self.stopped = false;
        self.store();
        self.sync_open(&ring_file)
    }

    /// Takes the journal up from the header `h` and the files the process before left: up to the end of the last pack it
    /// journalled whole. A file past what the header says is in the files is one that process wrote and did not name in the
    /// header yet: its bytes are still in the ring. The stage is OFFLINE, awaiting the peer's position, as after a loss.
    /// `false` - the header and the files do not agree: the journal is not taken up.
    fn reopen(&mut self, h: *mut u8, ring_file: &Path) -> io::Result<bool> {
        // SAFETY: the header is mapped, HEAD bytes
        let (b, n, hb, off, cap, acked, whole_end, flags) = unsafe {
            (
                get_i64(h, Control::H_BASE),
                get_i64(h, Control::H_NEXT),
                get_i64(h, Control::H_HOT_BASE),
                get_i32(h, Control::H_HOT_OFF) as i64,
                get_i32(h, Control::H_HOT_CAP) as i64,
                get_i64(h, Control::H_ACKED),
                get_i64(h, Control::H_WHOLE_END),
                get_i32(h, Control::H_JFLAGS),
            )
        };
        if !(0 < b
            && b <= n
            && b <= hb
            && (n < hb || n - hb <= cap)
            && self.hot_size as i64 <= cap
            && cap <= self.segment as i64
            && 0 <= off
            && off < cap
            && ring_file.is_file())
        {
            return Ok(false);
        }
        let upto = n.min(hb); // the files hold the journal below hb, up to its end
        let file = self.file_size as i64;
        let segs = self.starts();
        let mut from = 0;
        while from < segs.len() && segs[from] + file <= b {
            delete_file(&self.seg(segs[from])); // given back: its deletion did not come to pass
            from += 1;
        }
        let mut k = from;
        while k < segs.len() && segs[k] < upto {
            k += 1;
        }
        for &s in &segs[k..] {
            delete_file(&self.seg(s)); // past the files or past the journal
        }
        if from < k && segs[k - 1] + file < upto {
            return Ok(false);
        }
        for i in from + 1..k {
            if segs[i] != segs[from] + (i - from) as i64 * file {
                return Ok(false);
            }
        }
        self.discard();
        for &s in &segs[from..k] {
            let f = self.seg(s);
            self.files.push(f);
        }
        self.base_ = if k == from { if n < hb { n } else { hb } } else { segs[from] };
        self.written = if k == from { 0 } else { (upto - segs[k - 1]) as usize };
        if !self.files.is_empty() && self.written < self.file_size {
            let mut ch = self.last(self.files.last().unwrap())?;
            ch.set_len(self.written as u64)?;
            ch.seek(SeekFrom::Start(self.written as u64))?;
            self.ch = Some(ch);
        }
        self.hot = Hot::mapped(Map::open(ring_file, cap as usize, false)?);
        self.hot_off = if n < hb { 0 } else { off as usize };
        self.hot_used = if n < hb { 0 } else { (n - hb) as usize };
        self.next_ = n;
        self.acked_ = self.base_.max(acked.min(n));
        self.sent_ = self.acked_;
        self.whole_end = self.base_.max(whole_end.min(n));
        self.whole = flags & 1 != 0;
        self.datagram = flags & 2 != 0;
        self.ends.clear();
        if self.whole_end < n {
            self.ends.push_back(n); // where the packs of the process before end is not kept: the journal is given back up to its end at once
        }
        self.parked_ = true;
        self.awaiting_ = true;
        self.swallow = false;
        self.replaying = NONE;
        self.kept = None;
        self.head = h;
        self.stopped = false;
        self.store();
        self.sync_open(ring_file)?; // what the process before left only in the memory of the system goes to the disk now
        Ok(true)
    }

    /// The positions the files of this journal start at, in order.
    fn starts(&self) -> Vec<i64> {
        let prefix = format!("{}.", self.base_file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let mut s = Vec::new();
        let Some(dir) = self.base_file.parent() else { return s };
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if let Some(hex) = name.strip_prefix(&prefix).and_then(|r| r.strip_suffix(".seg")) {
                    if let Ok(at) = u64::from_str_radix(hex, 16) {
                        s.push(at as i64);
                    }
                }
            }
        }
        s.sort_unstable();
        s
    }

    /// The last file of the journal, opened to be written on: through to the disk when the journal outlives the power.
    fn last(&self, file: &Path) -> io::Result<File> {
        let mut o = OpenOptions::new();
        o.read(true).write(true);
        if 0 <= self.sync_ms {
            os::through(&mut o);
        }
        o.open(file)
    }

    /// The process ends: what the transmitter held is journalled already ( [`Control::stop`] ), the journal goes to the disk.
    /// Nothing is journalled into the files after it: they are the next process's.
    pub(crate) fn stop(&mut self) -> io::Result<()> {
        if self.head.is_null() {
            return Ok(());
        }
        if let Some(ch) = &self.ch {
            ch.sync_data()?;
        }
        self.hot.force()?;
        self.store();
        self.detach(false);
        Ok(())
    }

    /// The journal leaves the files of its session: it goes on in memory - the session is over ( `over`: its files go ), or
    /// the process ends ( they stay for the next one ).
    pub(crate) fn detach(&mut self, over: bool) {
        if self.head.is_null() {
            return;
        }
        self.head = std::ptr::null_mut();
        let mut to = Hot::mem(self.hot.len().max(self.hot_size));
        self.hot_into(&mut to);
        self.hot = to; // the mapping goes
        self.ring_through = None;
        self.head_through = None;
        if over {
            self.discard_files();
            delete_file(&self.named(".ring"));
        } else {
            self.ch = None;
            self.stopped = true; // the files are the next process's
        }
    }

    // The journal outlives the power: resumable_sync_ms
    //
    // The journal goes to the disk as it goes - the word of the owner, 5.11: the moment of the last sync is kept, and each time a
    // pack is journalled whole the clock is looked at: sync_ms or more since that sync - the disk gets the journal, less - the
    // sync is passed by. No timer: the transmitter that falls silent syncs what the last syncs passed by once its queue is empty.
    // A sync writes the bytes of the ring not on the disk yet through to their places in the ring file - the files of the
    // backlog are written through as they are written -, then the positions of the journal so - the words H_D_* of the header
    // -, through, behind the bytes they name: a reboot leaves the journal as they say, up to the end of the last pack journalled
    // whole. Nothing waits for the disk: a power loss takes the packs of the last sync_ms at most - a peer that has more than
    // the disk is told where the journal goes on ( Lost_ ). The ring does not overwrite what the header on the disk names as its:
    // see overwrite().

    /// The bytes produced so far go to the disk - those of the ring written through at their places in the ring file - and the
    /// positions of the journal to the header on the disk, behind them.
    fn sync(&mut self) -> io::Result<()> {
        let (Some(ring), Some(head)) = (&self.ring_through, &self.head_through) else { return Ok(()) };
        let upto = self.next_;
        let hb = self.hot_base();
        let cap = self.hot.len();
        let mut from = self.synced.max(hb);
        if self.sync_all || cap != self.d_cap {
            from = hb;
        }
        if from < upto {
            let n = (upto - from) as usize;
            let at = self.hot_at((from - hb) as usize);
            let first = n.min(cap - at);
            os::write_at(ring, &self.hot[at..at + first], at as u64)?;
            if first < n {
                os::write_at(ring, &self.hot[..n - first], 0)?;
            }
        }
        let h = self.head;
        // SAFETY: the header is mapped while `head` is set, HEAD bytes
        let words = unsafe {
            set_i64(h, Control::H_D_BASE, self.base_);
            set_i64(h, Control::H_D_ACKED, self.acked_);
            set_i64(h, Control::H_D_NEXT, self.whole_next());
            set_i64(h, Control::H_D_HOT_BASE, hb);
            set_i32(h, Control::H_D_HOT_OFF, self.hot_off as i32);
            set_i32(h, Control::H_D_HOT_CAP, cap as i32);
            set_i64(h, Control::H_D_WHOLE_END, self.whole_end);
            set_i32(h, Control::H_D_JFLAGS, self.whole as i32 | (self.datagram as i32) << 1);
            set_i32(h, Control::H_D_VALID, 1);
            std::slice::from_raw_parts(h.add(Control::H_D_BASE), Control::H_D_END - Control::H_D_BASE)
        };
        os::write_at(head, words, Control::H_D_BASE as u64)?;
        self.sync_all = false;
        self.synced = upto;
        self.synced_at = std::time::Instant::now();
        self.d_hot_base = hb;
        self.d_upto = upto;
        self.d_off = self.hot_off;
        self.d_cap = cap;
        Ok(())
    }

    /// `hot[at .. at + n)` is about to be written on: if the header on the disk names a byte of it as the journal's, the disk
    /// gets the journal as it is first - that place is given back by then.
    fn overwrite(&mut self, at: usize, n: usize) -> io::Result<()> {
        let len = self.d_upto - self.d_hot_base;
        if len <= 0 || n == 0 {
            return Ok(());
        }
        let end = self.d_off + (len as usize).min(self.d_cap);
        let hit = if end <= self.d_cap { at < end && self.d_off < at + n } else { at < end - self.d_cap || self.d_off < at + n };
        if hit {
            self.sync()
        } else {
            Ok(())
        }
    }

    /// The journal outlives the power ( resumable_sync_ms ): the handles that write the ring and the header through, and the
    /// disk gets the journal as it is now.
    fn sync_open(&mut self, ring_file: &Path) -> io::Result<()> {
        if self.sync_ms < 0 {
            return Ok(());
        }
        let opened = (|| {
            let mut o = OpenOptions::new();
            o.write(true);
            os::through(&mut o);
            Ok::<_, io::Error>((o.open(ring_file)?, o.open(self.named(".head"))?))
        })();
        let (ring, head) = match opened {
            Ok(h) => h,
            Err(e) => {
                self.sync_ms = -1; // no handle to write through: only the fall of the process is outlived
                return Err(e);
            }
        };
        self.ring_through = Some(ring);
        self.head_through = Some(head);
        self.sync_all = true;
        self.synced = self.base_;
        self.d_hot_base = self.base_;
        self.d_upto = self.base_;
        self.d_off = 0;
        self.d_cap = self.hot.len();
        self.sync()
    }
}

// The trace of the queue of the transmitter ( Control::TRACE ): the generated `put` of a guaranteed pack stores id + 1 into the
// cell of the place of the queue it took, before it publishes the pack; the generated `on_serializing`, having taken a pack from
// the queue, moves its cell to H_OPEN - the transmitter begins it -, which `pack_ended` clears. The cells are as wide as the id of
// a pack: queued1 / took1 for one byte, 2 for two, 4 for more. A place past the cells is not traced. `head`: the header of the
// session ( the generated transmitter keeps it ), null - the session lives in memory.

#[inline]
pub fn queued1(head: &AtomicPtr<u8>, slot: usize, id: u32) {
    let h = head.load(Ordering::Acquire);
    if !h.is_null() && slot < Control::META - Control::TRACE {
        // SAFETY: the header is mapped while it is published; the cell of the slot is of its producer
        unsafe { *h.add(Control::TRACE + slot) = id as u8 };
    }
}

#[inline]
pub fn queued2(head: &AtomicPtr<u8>, slot: usize, id: u32) {
    let h = head.load(Ordering::Acquire);
    if !h.is_null() && slot < (Control::META - Control::TRACE) >> 1 {
        // SAFETY: as above
        unsafe { (h.add(Control::TRACE + slot * 2) as *mut u16).write((id as u16).to_le()) };
    }
}

#[inline]
pub fn queued4(head: &AtomicPtr<u8>, slot: usize, id: u32) {
    let h = head.load(Ordering::Acquire);
    if !h.is_null() && slot < (Control::META - Control::TRACE) >> 2 {
        // SAFETY: as above
        unsafe { set_i32(h, Control::TRACE + slot * 4, id as i32) };
    }
}

#[inline]
pub fn took1(head: &AtomicPtr<u8>, slot: usize) {
    let h = head.load(Ordering::Acquire);
    if h.is_null() || Control::META - Control::TRACE <= slot {
        return;
    }
    // SAFETY: the header is mapped while it is published; the cell of a slot taken is of the consumer
    unsafe {
        let at = h.add(Control::TRACE + slot);
        set_i32(h, Control::H_OPEN, *at as i32);
        *at = 0;
    }
}

#[inline]
pub fn took2(head: &AtomicPtr<u8>, slot: usize) {
    let h = head.load(Ordering::Acquire);
    if h.is_null() || (Control::META - Control::TRACE) >> 1 <= slot {
        return;
    }
    // SAFETY: as above
    unsafe {
        let at = h.add(Control::TRACE + slot * 2) as *mut u16;
        set_i32(h, Control::H_OPEN, u16::from_le(at.read()) as i32);
        at.write(0);
    }
}

#[inline]
pub fn took4(head: &AtomicPtr<u8>, slot: usize) {
    let h = head.load(Ordering::Acquire);
    if h.is_null() || (Control::META - Control::TRACE) >> 2 <= slot {
        return;
    }
    // SAFETY: as above
    unsafe {
        set_i32(h, Control::H_OPEN, get_i32(h, Control::TRACE + slot * 4));
        set_i32(h, Control::TRACE + slot * 4, 0);
    }
}

/// The ring of the journal: memory of its own, or the ring file of its session mapped. A slice either way - the path of a pack
/// reads `ptr` and `len`, no branch.
struct Hot {
    ptr: *mut u8,
    len: usize,
    mem: Vec<u8>,
    map: Option<Map>,
}

impl Hot {
    fn mem(n: usize) -> Self {
        let mut mem = vec![0u8; n];
        Self { ptr: mem.as_mut_ptr(), len: n, mem, map: None }
    }

    fn mapped(map: Map) -> Self {
        Self { ptr: map.ptr(), len: map.len(), mem: Vec::new(), map: Some(map) }
    }

    /// What is stored into the mapping, on the disk.
    fn force(&self) -> io::Result<()> {
        self.map.as_ref().map_or(Ok(()), |m| m.force())
    }
}

impl std::ops::Deref for Hot {
    type Target = [u8];
    #[inline(always)]
    fn deref(&self) -> &[u8] {
        // SAFETY: `mem` or the mapping, `len` bytes, live as long as `self`
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl std::ops::DerefMut for Hot {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `self` is borrowed mutably
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl std::fmt::Debug for Hot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes{}", self.len, if self.map.is_some() { ", mapped" } else { "" })
    }
}

/// A file mapped for reading and writing: the header of a session ( [`Control`] ), the ring of its journal. What is stored
/// into it is the system's at once - a process that falls takes nothing of it along ( Chronicle Queue, Aeron, Kafka keep their
/// logs so ).
pub(crate) struct Map {
    ptr: *mut u8,
    len: usize,
    file: File,
}

// SAFETY: the mapping is memory of the process, unmapped only when the `Map` goes
unsafe impl Send for Map {}
unsafe impl Sync for Map {}

impl Map {
    /// Maps `file` - `anew`: emptied first -, `size` bytes, for reading and writing.
    pub(crate) fn open(file: &Path, size: usize, anew: bool) -> io::Result<Self> {
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(anew).open(file)?;
        // SAFETY: a file open for reading and writing; the view lives as long as the `Map`
        let ptr = unsafe { os::map(&f, size)? };
        Ok(Self { ptr, len: size, file: f })
    }

    #[inline(always)]
    pub(crate) fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// What is stored into the mapping, on the disk ( FlushViewOfFile + FlushFileBuffers, msync ).
    pub(crate) fn force(&self) -> io::Result<()> {
        // SAFETY: the view of this `Map`
        unsafe { os::flush(self.ptr, self.len)? };
        self.file.sync_data()
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the view of this `Map`, not used after it
        unsafe { os::unmap(self.ptr, self.len) };
    }
}

impl std::fmt::Debug for Map {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes mapped", self.len)
    }
}

#[cfg(windows)]
pub(crate) mod os {
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileMappingW(file: *mut c_void, attributes: *const c_void, protect: u32, high: u32, low: u32, name: *const u16) -> *mut c_void;
        fn MapViewOfFile(mapping: *mut c_void, access: u32, high: u32, low: u32, bytes: usize) -> *mut c_void;
        fn UnmapViewOfFile(at: *const c_void) -> i32;
        fn FlushViewOfFile(at: *const c_void, bytes: usize) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(key: isize, sub_key: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, size: *mut u32) -> i32;
    }

    const PAGE_READWRITE: u32 = 0x04;
    const FILE_MAP_WRITE: u32 = 0x02;
    const FILE_MAP_READ: u32 = 0x04;
    const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as i32 as isize;
    const RRF_RT_REG_DWORD: u32 = 0x10;

    /// The view of `size` bytes of `file`: the file grows to it - the section does it.
    pub(super) unsafe fn map(file: &File, size: usize) -> io::Result<*mut u8> {
        let section = CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), PAGE_READWRITE, (size as u64 >> 32) as u32, size as u32, std::ptr::null());
        if section.is_null() {
            return Err(io::Error::last_os_error());
        }
        let p = MapViewOfFile(section, FILE_MAP_WRITE | FILE_MAP_READ, 0, 0, size);
        let e = io::Error::last_os_error();
        CloseHandle(section); // the view keeps it
        if p.is_null() {
            return Err(e);
        }
        Ok(p as *mut u8)
    }

    pub(super) unsafe fn unmap(at: *mut u8, _len: usize) {
        UnmapViewOfFile(at as *const c_void);
    }

    pub(super) unsafe fn flush(at: *mut u8, len: usize) -> io::Result<()> {
        if FlushViewOfFile(at as *const c_void, len) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Written through to the disk: FILE_FLAG_WRITE_THROUGH.
    pub(crate) fn through(o: &mut OpenOptions) {
        use std::os::windows::fs::OpenOptionsExt;
        o.custom_flags(FILE_FLAG_WRITE_THROUGH);
    }

    pub(crate) fn write_at(f: &File, mut bytes: &[u8], mut at: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !bytes.is_empty() {
            let n = f.seek_write(bytes, at)?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[n..];
            at += n as u64;
        }
        Ok(())
    }

    /// `BootId` of the prefetcher, counted up at every boot.
    pub(super) fn boot() -> i64 {
        let w = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let key = w(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters");
        let value = w("BootId");
        let mut id = 0u32;
        let mut size = 4u32;
        // SAFETY: the strings end with 0, the buffer is 4 bytes as `size` says
        let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut id as *mut u32 as *mut c_void, &mut size) };
        if r == 0 { id as i64 | 2 << 32 } else { 0 }
    }
}

#[cfg(unix)]
pub(crate) mod os {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::io::AsRawFd;

    /// The view of `size` bytes of `file`: the file grows to it first.
    pub(super) unsafe fn map(file: &File, size: usize) -> io::Result<*mut u8> {
        if file.metadata()?.len() < size as u64 {
            file.set_len(size as u64)?;
        }
        let p = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0);
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(p as *mut u8)
    }

    pub(super) unsafe fn unmap(at: *mut u8, len: usize) {
        libc::munmap(at as *mut libc::c_void, len);
    }

    pub(super) unsafe fn flush(at: *mut u8, len: usize) -> io::Result<()> {
        if libc::msync(at as *mut libc::c_void, len, libc::MS_SYNC) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Written through to the disk: O_DSYNC.
    pub(crate) fn through(o: &mut OpenOptions) {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_DSYNC);
    }

    pub(crate) fn write_at(f: &File, bytes: &[u8], at: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        f.write_all_at(bytes, at)
    }

    /// Linux: `/proc/sys/kernel/random/boot_id`, its first 8 hex digits; elsewhere 0 - unknown.
    pub(super) fn boot() -> i64 {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .and_then(|s| s.get(..8).and_then(|h| u32::from_str_radix(h, 16).ok()))
            .map_or(0, |b| b as i64 | 1 << 32)
    }
}

/// The boot of the system this process runs in: what the system had not written to the disk is lost with it. Windows -
/// `BootId` of the prefetcher, counted up at every boot; Linux - `/proc/sys/kernel/random/boot_id`. 0 - unknown: a header left
/// by a process that fell is never trusted then.
pub fn boot() -> i64 {
    static BOOT: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *BOOT.get_or_init(os::boot)
}

/// The lock of [`dir`]: held while the process lives - `None`, another process holds it.
static LOCK: std::sync::OnceLock<Option<File>> = std::sync::OnceLock::new();

/// [`dir`] is this process's: the sessions live in files there. One process holds it at a time - a `.lock` in it -, as Kafka
/// holds each of its `log.dirs`: a second one keeps its sessions in memory and its backlog in files of its own
/// ([`default_dir`]).
pub fn files() -> bool {
    LOCK.get_or_init(|| {
        let d = dir();
        let held = std::fs::create_dir_all(&d)
            .and_then(|_| OpenOptions::new().read(true).write(true).create(true).truncate(false).open(d.join(".lock")))
            .and_then(|f| f.try_lock().map(|_| f).map_err(io::Error::from));
        match held {
            Ok(f) => Some(f),
            Err(e) => {
                crate::lib::adhoc::connection::transmitter::on_error::tx_at(
                    crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error,
                    &d.display(),
                    &"is held by another process: the sessions of this one live in memory only",
                    Some(&e),
                );
                None
            }
        }
    })
    .is_some()
}

/// Deletes the sessions the process before left in [`dir`]: this process starts them anew. Before the first session carries
/// anything.
pub fn wipe() {
    if !files() {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir()) {
        for e in rd.flatten() {
            if e.file_name() != ".lock" {
                delete_file(&e.path());
            }
        }
    }
}

/// `n` bytes at the head of what a saved session is read from ([`Resumable::load`]): taken off it.
pub fn take<'a>(from: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if from.len() < n {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the saved session ends early"));
    }
    let (head, rest) = from.split_at(n);
    *from = rest;
    Ok(head)
}

/// A position or a count at the head of a saved session: little-endian, as [`Resumable::save`]
/// writes it.
pub fn take_i64(from: &mut &[u8]) -> io::Result<i64> {
    Ok(i64::from_le_bytes(take(from, 8)?.try_into().unwrap()))
}

pub fn take_u32(from: &mut &[u8]) -> io::Result<u32> {
    Ok(u32::from_le_bytes(take(from, 4)?.try_into().unwrap()))
}

/// Nobody holds the journal ([`Journal`] `owner`).
pub(crate) const FREE: u32 = 0;
/// The thread that pulls the queue while the session has no socket, or saves / loads / ends the
/// session, holds the journal ([`Journal`] `owner`). A source holds it by its number.
pub(crate) const PULL: u32 = u32::MAX;
/// What the holder of the journal does before it lets it go ([`Journal`] `pending`): the session
/// is over - the journal empties.
const END: u8 = 1;
/// The socket the session was on is gone, its source never held the journal: the journal parks.
const PARK: u8 = 2;
/// The session got its files ([`Control`] attach): the journal moves into them ([`Journal`] `attaching`).
const ATTACH: u8 = 4;

/// The journal of a session among the threads of the session, without a lock ( C# / Java / Go
/// `Transmitter.Resumable` ): one holder at a time - the source of the socket the session is on,
/// from the moment it takes the journal until it is dropped, or, while the session has no socket,
/// the thread that pulls the queue into it, one pack a take. A source keeps the journal across its
/// fills: the path of a pack touches no atomic of the journal but the positions it publishes, and
/// the other threads read only those.
pub struct Journal {
    j: std::cell::UnsafeCell<Resumable>,
    /// Who holds the journal: [`FREE`], [`PULL`], or the number of a source.
    owner: std::sync::atomic::AtomicU32,
    /// The session has no socket: the thread that queues a pack journals it (C# `parked_`).
    offline: std::sync::atomic::AtomicBool,
    /// [`END`] / [`PARK`]: done by the holder as it lets the journal go (C# `closing`).
    pending: std::sync::atomic::AtomicU8,
    /// The positions the holder published as it last let the journal go, or at the end of its
    /// fill (C# `base_pub` …): what the other threads read.
    base: std::sync::atomic::AtomicI64,
    acked: std::sync::atomic::AtomicI64,
    sent: std::sync::atomic::AtomicI64,
    next: std::sync::atomic::AtomicI64,
    /// The files of the session the journal is to move into ([`ATTACH`]): the header mapped, `<dir>/<name>`, taken up from
    /// the process before, trusted.
    attaching: parking_lot::Mutex<Option<(usize, PathBuf, bool, bool)>>,
}

// SAFETY: the journal itself is reached by its holder only (`owner`, taken with a compare-exchange
// that acquires what the holder before released); everything else is atomics.
unsafe impl Sync for Journal {}

impl Journal {
    pub fn new(j: Resumable) -> Self {
        Self {
            base: std::sync::atomic::AtomicI64::new(j.base_),
            acked: std::sync::atomic::AtomicI64::new(j.acked_),
            sent: std::sync::atomic::AtomicI64::new(j.sent_),
            next: std::sync::atomic::AtomicI64::new(j.next_),
            j: std::cell::UnsafeCell::new(j),
            owner: std::sync::atomic::AtomicU32::new(FREE),
            offline: std::sync::atomic::AtomicBool::new(true),
            pending: std::sync::atomic::AtomicU8::new(0),
            attaching: parking_lot::Mutex::new(None),
        }
    }

    /// The session got its files ([`Control`] attach): the journal moves into them - at once when nobody holds it, otherwise by
    /// its holder: as it lets it go, or the source of the socket the session goes on on, at its next fill.
    pub(crate) fn attach(&self, head: *mut u8, base_file: PathBuf, restoring: bool, trusted: bool) {
        *self.attaching.lock() = Some((head as usize, base_file, restoring, trusted));
        self.ask(ATTACH);
    }

    /// The holder `j` moves the journal into the files of the session, if it got them meanwhile ([`attach`](Self::attach)).
    pub(crate) fn attached(&self, j: &mut Resumable) {
        if self.pending.load(Ordering::SeqCst) & ATTACH == 0 {
            return;
        }
        self.pending.fetch_and(!ATTACH, Ordering::SeqCst);
        self.attach_now(j);
    }

    fn attach_now(&self, j: &mut Resumable) {
        let Some((head, base_file, restoring, trusted)) = self.attaching.lock().take() else { return };
        if let Err(e) = j.attach(head as *mut u8, base_file, restoring, trusted) {
            crate::lib::adhoc::connection::transmitter::on_error::tx_at(crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error, &"the journal", &"could not move into the files of its session", Some(&e));
        }
    }

    /// The journal itself.
    ///
    /// # Safety
    /// The caller holds it ([`take`](Self::take)), or no other thread reaches the session yet.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self) -> &mut Resumable {
        // SAFETY: as the caller vouches
        unsafe { &mut *self.j.get() }
    }

    /// `who` takes the journal, if nobody holds it.
    #[inline]
    pub(crate) fn take(&self, who: u32) -> bool {
        self.owner.compare_exchange(FREE, who, Ordering::SeqCst, Ordering::Relaxed).is_ok()
    }

    /// The holder lets the journal go: what was asked of the holder meanwhile is done first, the
    /// positions are published. Asked once it was let go, it is taken back for it - either the
    /// asking side finds the journal free, or the holder finds what was asked (both `SeqCst`).
    pub(crate) fn give(&self) {
        loop {
            let asked = self.pending.swap(0, Ordering::SeqCst);
            // SAFETY: the caller holds the journal
            let j = unsafe { self.get() };
            if asked & ATTACH != 0 {
                self.attach_now(j);
            }
            if asked & END != 0 {
                let _ = j.close(false);
            } else if asked & PARK != 0 {
                if let Err(e) = j.close(true) {
                    crate::lib::adhoc::connection::transmitter::on_error::tx_at(crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error, &"the journal", &"could not be parked", Some(&e));
                }
            }
            self.publish();
            self.owner.store(FREE, Ordering::SeqCst);
            if self.pending.load(Ordering::SeqCst) == 0 || !self.take(PULL) {
                return;
            }
        }
    }

    /// Asks the holder for `what` ([`END`] / [`PARK`]): done at once when nobody holds the journal.
    fn ask(&self, what: u8) {
        self.pending.fetch_or(what, Ordering::SeqCst);
        if self.take(PULL) {
            self.give();
        }
    }

    /// The session is over: the journal empties, its positions go on above what the peer held
    /// (C# `Close`) - at once when nobody holds it, otherwise by its holder as it lets it go.
    pub fn end(&self) {
        self.ask(END);
    }

    /// The socket the session was on is gone without its source ever holding the journal: it parks.
    pub(crate) fn park(&self) {
        self.ask(PARK);
    }

    /// The holder publishes the positions, the ones that changed (C# `publish`).
    #[inline]
    pub(crate) fn publish(&self) {
        // SAFETY: the caller holds the journal
        let j = unsafe { &*self.j.get() };
        for (a, v) in [(&self.base, j.base_), (&self.acked, j.acked_), (&self.sent, j.sent_), (&self.next, j.next_)] {
            if a.load(Ordering::Relaxed) != v {
                a.store(v, Ordering::Release);
            }
        }
    }

    /// The positions as the holder published them: from any thread.
    #[inline]
    pub fn base(&self) -> i64 {
        self.base.load(Ordering::Acquire)
    }
    #[inline]
    pub fn acked(&self) -> i64 {
        self.acked.load(Ordering::Acquire)
    }
    #[inline]
    pub fn sent(&self) -> i64 {
        self.sent.load(Ordering::Acquire)
    }
    #[inline]
    pub fn next(&self) -> i64 {
        self.next.load(Ordering::Acquire)
    }

    /// The journal holds bytes the peer has not confirmed.
    #[inline]
    pub fn unconfirmed(&self) -> bool {
        self.acked() < self.next()
    }

    /// Bytes sent and not confirmed yet: once a step's worth is out the peer owes a confirmation
    /// (`Network::TCP::Redial::stalled`). From any thread.
    #[inline]
    pub fn owed(&self) -> i64 {
        self.sent() - self.acked()
    }

    /// The session has no socket: the thread that queues a pack journals it.
    #[inline]
    pub(crate) fn is_offline(&self) -> bool {
        self.offline.load(Ordering::SeqCst)
    }

    #[inline]
    pub(crate) fn set_offline(&self, offline: bool) {
        self.offline.store(offline, Ordering::SeqCst);
    }

    /// `PULL` takes the journal, waiting for the holder - a pull of one pack, or nobody: for the
    /// threads that save / load a session, never a thread of a socket. `false` - a socket has the
    /// session (`offline` only: while it is offline).
    pub(crate) fn take_offline(&self, offline_only: bool) -> bool {
        loop {
            if offline_only && !self.is_offline() {
                return false;
            }
            if self.take(PULL) {
                if offline_only && !self.is_offline() {
                    self.give();
                    return false;
                }
                return true;
            }
            std::thread::yield_now();
        }
    }

    /// The journal [`save`](Resumable::save) wrote in the process before goes on in this one,
    /// parked: before any socket of the session.
    pub fn load(&self, from: &mut &[u8]) -> io::Result<()> {
        self.take_offline(false);
        // SAFETY: taken
        let r = unsafe { self.get() }.load(from);
        self.set_offline(true); // until the peer comes back, the thread that queues a pack journals it
        self.give();
        r
    }

    /// The session is over before its journal moved into its files: it does not ([`attach`](Self::attach)).
    pub(crate) fn unattach(&self) {
        *self.attaching.lock() = None;
    }
}

impl std::fmt::Display for Journal {
    /// The positions as published.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let base = self.base();
        write!(f, "journal base={} acked=+{} sent=+{} next=+{}", base, self.acked() - base, self.sent() - base, self.next() - base)
    }
}

impl Drop for Resumable {
    /// Attempts to delete all segment files owned by this journal: not those of a session - they are its, a process that falls
    /// leaves them too -, nor those left for the next process ([`stop`](Self::stop)).
    fn drop(&mut self) {
        if self.head.is_null() && !self.stopped {
            self.discard_files();
        }
    }
}

impl std::fmt::Display for Resumable {
    /// Formats journal state and positions relative to `base_` for diagnostics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "replays={}/{}B journal base={} acked=+{} sent=+{} next=+{} buffer={}/{} files={}{}{}{}",
            self.replays,
            self.replayed,
            self.base_,
            self.acked_ - self.base_,
            self.sent_ - self.base_,
            self.next_ - self.base_,
            self.hot_used,
            self.hot.len(),
            self.files.len(),
            if self.parked_ { " OFFLINE" } else { "" },
            if self.awaiting_ { " awaiting" } else { "" },
            if self.swallow { " swallow" } else { "" },
        )
    }
}

// Journal root and seeds

/// Unix time in SECONDS: 64 bits, past 2038. 0 is 1970 and never a real reading: the "not set" of
/// every `resumable` field.
#[inline]
pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64
}

/// `deadline` is 0 or passed already.
#[inline]
pub fn expired(deadline: i64) -> bool {
    deadline == 0 || deadline <= now()
}

/// Where the sessions live, set by [`set_dir`]; none - `<current directory>/journal`.
static DIR: parking_lot::RwLock<Option<PathBuf>> = parking_lot::RwLock::new(None);

/// Sets where the sessions live as they go - their headers, their journals - and where the next process takes them up from
/// ([`Control::open`], `Pool::restore`): set at the start, before the first session carries anything.
pub fn set_dir(root: PathBuf) {
    *DIR.write() = Some(root);
}

/// Where the sessions live: [`set_dir`], or `<current directory>/journal`. One process holds it at a time ([`files`]).
pub fn dir() -> PathBuf {
    DIR.read().clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join("journal"))
}

/// The files of the journals of this process that live in memory - no files of a session: `temp_dir/AdHoc/journal/<pid>`.
pub fn default_dir() -> PathBuf {
    std::env::temp_dir().join("AdHoc").join("journal").join(std::process::id().to_string())
}

fn delete_file(file: &Path) {
    let _ = std::fs::remove_file(file);
}

fn next_file_tag() -> u64 {
    static TAG: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (TAG.fetch_add(1, Ordering::Relaxed) << 32) ^ ((std::process::id() as u64) << 48)
}

/// A random start position in `[1<<48, 1<<61)`: a position of one journal does not fall into
/// another. Seeded from the clock, the pid and a counter — no external RNG in this crate's
/// dependency tree.
pub(crate) fn seed_start() -> i64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut x = nanos
        ^ ((std::process::id() as u64) << 32)
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // splitmix64
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    const LO: u64 = 1 << 48;
    const SPAN: u64 = (1 << 61) - LO;
    (LO + x % SPAN) as i64
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1 << 50;

    /// A pack that is just a blob of bytes.
    fn blob(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + 7) as u8).collect()
    }

    fn test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join("AdHoc")
            .join("journal-test")
            .join(format!("{}-{tag}-{nanos}", std::process::id()))
    }

    fn seg_files(dir: &PathBuf) -> usize {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".seg"))
                    .count()
            })
            .unwrap_or(0)
    }

    struct Sink(Vec<u8>);
    impl Sink {
        fn feeder(&mut self) -> impl FnMut(&[u8]) -> io::Result<()> + '_ {
            |b| {
                self.0.extend_from_slice(b);
                Ok(())
            }
        }
    }

    #[test]
    fn positions_seed_in_range() {
        let jr = Resumable::with_dir(1 << 20, test_dir("seed"));
        let p = jr.base();
        assert!((1 << 48..1 << 61).contains(&p));
        assert_eq!(p, jr.acked());
        assert_eq!(p, jr.sent());
        assert_eq!(p, jr.next());
        assert!(jr.is_holding()); // awaiting the peer's position
        assert!(jr.is_parked()); // no connection yet
    }

    #[test]
    fn capacity_limit_matches_csharp() {
        let dir = test_dir("cap-assert");
        let jr = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Resumable::with_dir((1i64 << 31) + 1, dir)
        }));
        assert!(jr.is_err());
    }

    #[test]
    fn awaiting_journals_without_emitting() {
        let dir = test_dir("awaiting");
        let mut jr = Resumable::with_start(1 << 20, dir, START);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];

        // Before any resume: journalled only.
        assert!(jr.send(&blob(100), window.len(), sink.feeder()).unwrap());
        assert!(sink.0.is_empty());
        assert_eq!(jr.next(), START + 100);

        // `resume_here` skips journal-only bytes and continues from the current end position.
        assert_eq!(jr.resume_here(sink.feeder()).unwrap(), -1);
        assert!(sink.0.is_empty());
        assert!(!jr.is_holding());

        // Now sends emit.
        let b = blob(50);
        assert!(jr.send(&b, window.len(), sink.feeder()).unwrap());
        assert_eq!(sink.0, b);
        assert_eq!(jr.next(), START + 150);
        assert_eq!(jr.sent(), START + 150);
    }

    #[test]
    fn ack_releases_everything() {
        let dir = test_dir("ack");
        let mut jr = Resumable::with_geometry(1 << 20, dir.clone(), START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.send(&blob(100), window.len(), sink.feeder()).unwrap();
        jr.send(&blob(50), window.len(), sink.feeder()).unwrap();
        assert_eq!(jr.backlog(), 150);
        assert!(0 < seg_files(&dir), "150 bytes through a 64-byte hot buffer must spill");

        // A position not of this journal is passed by.
        jr.ack(START - 1);
        jr.ack(START + 151);
        assert_eq!(jr.backlog(), 150);

        jr.ack(START + 150);
        assert_eq!(jr.backlog(), 0);
        assert_eq!(jr.base(), START + 150);
        assert_eq!(seg_files(&dir), 0, "acked files are deleted");
    }

    #[test]
    fn ack_partial_releases_whole_files_only() {
        let dir = test_dir("ack-partial");
        let mut jr = Resumable::with_geometry(1 << 20, dir.clone(), START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.send(&blob(130), window.len(), sink.feeder()).unwrap();
        jr.send(&blob(270), window.len(), sink.feeder()).unwrap();
        // 400 parked bytes through hot=64: files hold 384, hot holds 16.
        assert_eq!(seg_files(&dir), 3);

        jr.ack(START + 130); // the first pack whole: one full file (128) and change
        assert_eq!(jr.base(), START + 128, "only whole files are deleted");
        assert_eq!(seg_files(&dir), 2);
        assert_eq!(jr.backlog(), 400 - 128);

        // Replay must still be byte-exact across the file/hot seam.
        let mut replayed = Sink(Vec::new());
        assert_eq!(jr.resume(START + 130, replayed.feeder()).unwrap(), -1);
        assert_eq!(replayed.0, sink.0[130..]);
    }

    #[test]
    fn ack_in_the_middle_of_a_pack_keeps_the_pack() {
        let dir = test_dir("ack-mid-pack");
        let mut jr = Resumable::with_geometry(1 << 20, dir.clone(), START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.send(&blob(400), window.len(), sink.feeder()).unwrap();

        // the peer confirms in the middle of the pack: the journal still starts at its start - a
        // peer with none of the journal is told a position a pack starts at
        jr.ack(START + 300);
        assert_eq!(jr.acked(), START + 300);
        assert_eq!(jr.base(), START, "a pack goes back only once the confirmation passed its end");
        assert_eq!(seg_files(&dir), 3);

        // a peer that has the pack up to its middle goes on from there, byte-exact
        let mut replayed = Sink(Vec::new());
        assert_eq!(jr.resume(START + 300, replayed.feeder()).unwrap(), -1);
        assert_eq!(replayed.0, sink.0[300..]);

        jr.ack(START + 400);
        assert_eq!(jr.backlog(), 0);
        assert_eq!(seg_files(&dir), 0, "the pack is confirmed whole: it goes");
    }

    #[test]
    fn resume_replays_from_files_and_hot() {
        let dir = test_dir("replay");
        let mut jr = Resumable::with_geometry(1 << 20, dir.clone(), START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.send(&blob(150), window.len(), sink.feeder()).unwrap();

        let mut replayed = Sink(Vec::new());
        assert_eq!(jr.resume(START + 20, replayed.feeder()).unwrap(), -1);
        assert_eq!(replayed.0, sink.0[20..]);
        // resume_here counts as the first replay.
        assert_eq!(jr.replays(), (2, 130));
    }

    /// The buffer in memory is a ring: packs that go round its end, bytes given back from its middle, the ring grown and its
    /// head spilled to the files - a replay from any position held is the stream as it went, byte for byte.
    #[test]
    fn the_ring_replays_what_went_round_its_end() {
        let dir = test_dir("ring");
        let mut jr = Resumable::with_geometry(1 << 20, dir, START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        jr.resume_here(sink.feeder()).unwrap();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        let mut replays = 0;
        for round in 0..6000usize {
            let pack: Vec<u8> = (0..1 + rnd(90) as usize).map(|i| (round * 7 + i) as u8).collect();
            if !jr.send(&pack, 1 + rnd(64) as usize, sink.feeder()).unwrap() {
                jr.ack(jr.sent()); // the bytes in flight at their limit: the peer reports
                assert!(jr.send(&pack, 64, sink.feeder()).unwrap());
            }
            if rnd(3) == 0 {
                jr.ack(jr.acked() + rnd((jr.sent() - jr.acked()) as u64 + 1) as i64);
            }
            if rnd(9) == 0 {
                let from = jr.base() + rnd((jr.next() - jr.base()) as u64 + 1) as i64;
                let mut re = Sink(Vec::new());
                if jr.resume(from, re.feeder()).unwrap() == -1 {
                    assert_eq!(re.0, sink.0[(from - START) as usize..], "round {round}, from +{}", from - START);
                    replays += 1;
                }
            }
        }
        assert!(300 < replays, "{replays} replays");
    }

    // Datagram transport: reported / turn / replay_from_sent

    /// Seed a journal with two sent blobs; returns (jr, wire bytes).
    fn datagram_rig(tag: &str, a: usize, b: usize) -> (Resumable, Vec<u8>) {
        let dir = test_dir(tag);
        let mut jr = Resumable::with_geometry(1 << 20, dir, START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.send(&blob(a), window.len(), sink.feeder()).unwrap();
        jr.send(&blob(a + b), window.len(), sink.feeder())
            .unwrap();
        (jr, sink.0)
    }

    #[test]
    fn reported_releases_and_bumps_sent() {
        let (mut jr, _) = datagram_rig("reported", 100, 50); // 250 bytes in the journal
        assert!(jr.is_whole(), "nothing given back mid-pack yet");
        // The peer received all but the last 50 bytes (the low 32 bits).
        let pos = (jr.next() - 50) as i32;
        jr.reported(pos);
        assert_eq!(jr.acked(), START + 200);
        assert_eq!(jr.sent(), START + 250, "sent never rewinds past the report");
        assert!(!jr.is_whole(), "base may now stand mid-pack");
        assert_eq!(jr.backlog(), 50, "only the reported prefix is given back");
    }

    #[test]
    fn reported_wraps_to_nearest_below_next() {
        let (mut jr, _) = datagram_rig("reported-wrap", 60, 0);
        // A low-32 position slightly ahead clamps to next_: a full ack.
        jr.reported((jr.next() + 10) as i32);
        assert_eq!(jr.acked(), jr.next(), "clamped to next_");
        // Reset, then a position so far ahead the 32-bit difference wraps negative: resolved far
        // below acked_ → passed by.
        let mut jr2 = datagram_rig("reported-wrap2", 60, 0).0;
        let pos = (jr2.next() + 3_000_000_000i64) as i32; // wraps to a negative i32
        jr2.reported(pos);
        assert_eq!(jr2.acked(), START, "a wrapped report is not of this journal");
    }

    #[test]
    fn turn_held_replays_from_position() {
        let (mut jr, wire) = datagram_rig("turn-held", 100, 50);
        assert_eq!(jr.turn(START + 40), -1);
        let mut replayed = Sink(Vec::new());
        jr.replay_from_sent(replayed.feeder()).unwrap();
        assert_eq!(replayed.0, wire[40..]);
        assert_eq!(jr.sent(), jr.next(), "the backlog is fully served");
    }

    #[test]
    fn turn_below_base_with_mid_pack_head_discards() {
        let (mut jr, _) = datagram_rig("turn-discard", 100, 50);
        // Give back the middle of the journal: base stands mid-pack.
        jr.reported((START + 130) as i32);
        assert!(!jr.is_whole());
        // The peer asks for a position the journal no longer holds.
        let at = jr.turn(START + 10);
        assert_eq!(at, START + 250, "goes on from the next pack boundary");
        assert_eq!(jr.base(), START + 250);
        assert_eq!(jr.acked(), START + 250);
        assert_eq!(jr.backlog(), 0, "the headless journal is discarded");
        assert!(jr.is_whole());
    }

    #[test]
    fn turn_not_held_but_whole_keeps_journal() {
        let (mut jr, wire) = datagram_rig("turn-whole", 60, 0); // 120 bytes
        assert!(jr.is_whole(), "no reported() happened");
        // Not held, but base stands on a pack start: nothing to discard.
        let at = jr.turn(START - 1);
        assert_eq!(at, START, "goes on from the first byte held");
        assert_eq!(jr.backlog(), 120);
        let mut replayed = Sink(Vec::new());
        jr.replay_from_sent(replayed.feeder()).unwrap();
        assert_eq!(replayed.0, wire);
    }

    #[test]
    fn datagram_loss_and_repeated_resend() {
        let (mut jr, wire) = datagram_rig("datagram-loss", 80, 40);
        // A datagram covering [20, 40) is lost: the peer reports 20, the stream turns back there.
        jr.reported((START + 20) as i32);
        assert_eq!(jr.turn(START + 20), -1);
        let mut again = Sink(Vec::new());
        jr.replay_from_sent(again.feeder()).unwrap();
        assert_eq!(again.0, wire[20..], "the lost range is sent again");
        // The peer now has everything; reported releases it all.
        jr.reported(jr.next() as i32);
        assert_eq!(jr.backlog(), 0);
    }

    #[test]
    fn turn_resets_awaiting_and_swallow() {
        let (mut jr, _) = datagram_rig("turn-flags", 10, 10);
        jr.ack(START + 20); // in the middle of the second pack: the journal starts at its start, the end of the first
        // Force swallow by naming a gone position via resume().
        let mut dummy = Sink(Vec::new());
        assert_eq!(jr.resume(START - 5, dummy.feeder()).unwrap(), START + 10);
        assert!(jr.is_holding());
        // turn clears both flags, stream-side.
        assert_eq!(jr.turn(START + 20), -1);
        assert!(!jr.is_holding());
    }

    #[test]
    fn resume_below_base_swallows_until_told() {
        let dir = test_dir("swallow");
        let mut jr = Resumable::with_geometry(1 << 20, dir.clone(), START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.send(&blob(80), window.len(), sink.feeder()).unwrap();
        jr.ack(START + 80); // journal empty, base = START+80

        // The peer asks for a position that is gone.
        let mut dummy = Sink(Vec::new());
        assert_eq!(jr.resume(START + 10, dummy.feeder()).unwrap(), START + 80);
        assert!(jr.is_holding());

        // What is sent meanwhile is journalled only.
        let b = blob(40);
        let sent_so_far = sink.0.len();
        jr.send(&b, window.len(), sink.feeder()).unwrap();
        assert_eq!(sink.0.len(), sent_so_far);
        assert_eq!(jr.next(), START + 120);

        // The peer is told LOST(START+80) and answers CONTINUE(START+80): the journalled pack goes
        // out.
        let mut replayed = Sink(Vec::new());
        assert_eq!(jr.resume(START + 80, replayed.feeder()).unwrap(), -1);
        assert_eq!(replayed.0, b);
        assert!(!jr.is_holding());

        // And new sends emit again.
        jr.send(&blob(10), window.len(), sink.feeder()).unwrap();
        assert_eq!(sink.0.len(), sent_so_far + 10);
    }

    #[test]
    fn resume_above_next_returns_boundary() {
        let dir = test_dir("above");
        let mut jr = Resumable::with_start(1 << 20, dir, START);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.send(&blob(30), window.len(), sink.feeder()).unwrap();
        // NOWHERE-like position of another journal: above next_.
        let mut dummy = Sink(Vec::new());
        assert_eq!(jr.resume(START + 999, dummy.feeder()).unwrap(), START + 30);
    }

    #[test]
    fn park_moves_journal_to_disk() {
        let dir = test_dir("park");
        let mut jr = Resumable::with_geometry(1 << 20, dir.clone(), START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.online(); // the hot buffer may grow up to the segment

        jr.send(&blob(200), window.len(), sink.feeder()).unwrap();
        assert_eq!(seg_files(&dir), 0, "200 bytes fit the grown buffer (256)");

        jr.close(true).unwrap(); // keep the session: park
        assert!(jr.is_parked());
        assert!(jr.is_holding());
        assert_eq!(jr.sent(), jr.acked());
        assert_eq!(seg_files(&dir), 2, "the grown buffer spills: 200 = 128 + 72");

        // Reconnect: replay from the files is byte-exact.
        let mut replayed = Sink(Vec::new());
        assert_eq!(jr.resume(START, replayed.feeder()).unwrap(), -1);
        assert_eq!(replayed.0, sink.0);
    }

    /// The header of a session in `dir`, mapped: what `Control` maps.
    fn header(dir: &PathBuf) -> (Map, PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        let base = dir.join("s.1");
        (Map::open(&dir.join("s.1.head"), Control::HEAD, false).unwrap(), base)
    }

    /// The process falls with a session in its files - packs in the files of the backlog, in the ring, one in the middle -:
    /// the next process takes the journal up to the end of the last pack journalled whole, and replays it byte for byte.
    #[test]
    fn a_fall_leaves_the_journal_to_the_next_process() {
        for sync_ms in [-1, 0] {
            let dir = test_dir(&format!("fall{sync_ms}"));
            let (map, base) = header(&dir);
            let mut jr = Resumable::with_geometry(1 << 20, dir.clone(), START, 64, 128, 256).sync_ms(sync_ms);
            let mut sink = Sink(Vec::new());
            jr.resume_here(sink.feeder()).unwrap();
            jr.send(&blob(70), 64, sink.feeder()).unwrap(); // before the session has files: in files of this process
            jr.attach(map.ptr(), base.clone(), false, false).unwrap();
            jr.online();
            for i in 0..3 {
                assert!(jr.send(&blob(40 + i * 7), 1 + i * 7, sink.feeder()).unwrap()); // the ring grows in its file
            }
            jr.ack(START + 70);
            jr.close(true).unwrap(); // the socket is lost: what the ring holds goes to the files, the ring file takes its first size
            for i in 0..5 {
                let b = blob(33 + i * 5);
                assert!(jr.send(&b, 64, |_: &[u8]| Ok(())).unwrap()); // parked: journalled only
                sink.0.extend_from_slice(&b);
            }
            let whole = sink.0.len();
            jr.journal(&blob(25)).unwrap(); // a pack begun: not of the journal the next process takes up
            assert!(0 < seg_files(&dir));
            drop(jr); // the fall: the files stay

            let mut next = Resumable::with_geometry(1 << 20, dir.clone(), START + 999, 64, 128, 256).sync_ms(sync_ms);
            next.attach(map.ptr(), base.clone(), true, true).unwrap();
            assert!(next.is_parked() && next.is_awaiting());
            assert_eq!(next.next(), START + whole as i64, "sync_ms {sync_ms}");
            let from = next.base();
            let mut replayed = Sink(Vec::new());
            assert_eq!(next.resume(from, replayed.feeder()).unwrap(), -1);
            assert_eq!(replayed.0, sink.0[(from - START) as usize..whole], "sync_ms {sync_ms}");

            // it goes on in the files: more packs, a clean stop, the process after takes it up again
            next.online();
            next.ack(START + whole as i64); // the peer has it all: the journal is given back, its files go
            assert!(next.send(&blob(500), 64, sink.feeder()).unwrap());
            assert_eq!(next.next(), START + whole as i64 + 500);
            next.stop().unwrap();
            // SAFETY: the header mapped
            assert_eq!(unsafe { get_i64(map.ptr(), Control::H_NEXT) }, START + whole as i64 + 500, "sync_ms {sync_ms}");
            let files = seg_files(&dir);
            drop(next);
            assert_eq!(seg_files(&dir), files, "the files are the next process's: the one that stopped does not delete them");
            let mut third = Resumable::with_geometry(1 << 20, dir.clone(), START + 5, 64, 128, 256);
            third.attach(map.ptr(), base, true, true).unwrap();
            let mut all = Sink(Vec::new());
            assert_eq!(third.base(), START + whole as i64);
            assert_eq!(third.resume(START + whole as i64, all.feeder()).unwrap(), -1);
            assert_eq!(all.0, sink.0[whole..], "sync_ms {sync_ms}");
            third.detach(true);
            assert_eq!(seg_files(&dir), 0, "the session is over: its files go");
        }
    }

    /// The trace of the queue: a guaranteed pack queued goes to its cell, taken - to OPEN, whole - nowhere.
    #[test]
    fn the_trace_follows_the_queue() {
        let dir = test_dir("trace");
        let (map, _) = header(&dir);
        let trace = AtomicPtr::new(map.ptr());
        queued2(&trace, 5, 42);
        // SAFETY: the header mapped
        unsafe {
            assert_eq!(*map.ptr().add(Control::TRACE + 10), 42);
            took2(&trace, 5);
            assert_eq!(*map.ptr().add(Control::TRACE + 10), 0);
            assert_eq!(get_i32(map.ptr(), Control::H_OPEN), 42);
        }
        queued1(&trace, Control::META, 1); // past the cells: not traced
    }

    /// The clock of the sessions counts seconds since 1970; 0 is never a reading - it has run out
    /// already.
    #[test]
    fn the_clock_counts_seconds() {
        let n = super::now();
        assert!(1_700_000_000 < n && n < 4_000_000_000, "{n}");
        assert!(super::expired(0) && super::expired(n) && !super::expired(n + 2));
        assert!(super::default_dir().ends_with(std::process::id().to_string()));
    }

    #[test]
    fn close_drops_session_and_reseeds() {
        let dir = test_dir("close");
        let mut jr = Resumable::with_geometry(1 << 20, dir.clone(), START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();
        jr.send(&blob(200), window.len(), sink.feeder()).unwrap();
        assert!(0 < seg_files(&dir));

        jr.close(false).unwrap();
        assert_eq!(seg_files(&dir), 0);
        assert_eq!(jr.backlog(), 0);
        assert_eq!(jr.base(), START + 201, "offsets go on above everything the peer may hold");
        assert!(jr.is_holding(), "the peer's position is awaited again");
    }

    #[test]
    fn send_refused_at_capacity_and_at_segment_limit() {
        let dir = test_dir("backpressure");
        let mut jr = Resumable::with_geometry(300, dir.clone(), START, 64, 128, 256);
        let mut sink = Sink(Vec::new());
        let mut window = [0u8; 64];
        jr.resume_here(sink.feeder()).unwrap();

        jr.send(&blob(100), window.len(), sink.feeder()).unwrap();
        jr.send(&blob(100), window.len(), sink.feeder()).unwrap();
        jr.send(&blob(50), window.len(), sink.feeder()).unwrap();
        // 250 unacked ≥ segment - segment/8 = 224: wait for a report.
        assert!(!jr.send(&blob(10), window.len(), sink.feeder()).unwrap());
        assert_eq!(jr.next(), START + 250, "a refused send journals nothing");

        jr.ack(START + 250);
        assert!(jr.send(&blob(10), window.len(), sink.feeder()).unwrap());

        // Capacity is checked before a packet starts. Finishing that packet may take the backlog
        // past the configured cap; the following packet must wait for acknowledgement.
        jr.send(&blob(280), window.len(), sink.feeder()).unwrap();
        assert_eq!(jr.backlog(), 290);
        assert!(!jr.send(&blob(1), window.len(), sink.feeder()).unwrap());
        let next = jr.next();
        jr.ack(next);
        assert!(jr.send(&blob(1), window.len(), sink.feeder()).unwrap());
    }

    #[test]
    fn sink_failure_keeps_journal() {
        let dir = test_dir("sink-fail");
        let mut jr = Resumable::with_start(1 << 20, dir, START);
        let mut window = [0u8; 64];
        jr.resume_here(|_: &[u8]| -> io::Result<()> { Ok(()) }).unwrap();
        // A transport that dies on the very first window: the window's bytes are journalled but not
        // marked sent.
        let r = jr.send(&blob(100), window.len(), |_: &[u8]| {
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "cut"))
        });
        assert!(r.is_err());
        assert_eq!(jr.next(), START + 64, "the first window's bytes are journalled");
        assert_eq!(jr.sent(), START, "but not marked sent");
        // A reconnect replays what the journal holds of the cut pack.
        let mut replayed = Sink(Vec::new());
        assert_eq!(jr.resume(START, replayed.feeder()).unwrap(), -1);
        assert_eq!(replayed.0, blob(64));
    }
}

mod source;
pub use source::{excluding_pulls, journal_queue, save, unconfirmed, ResumableSource, Session, Unconfirmed};
pub(crate) use source::pull_offline;
