// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Control packets for guaranteed delivery and session resumption.
//!
//! The generated connection dispatches these records alongside application packets:
//!
//! * `Confirm_` acknowledges progress in the peer journal.
//! * `Fresh_` starts negotiation without a retained receive position.
//! * `Continue_` requests output from a retained position.
//! * `Ok_` confirms that the requested position is available.
//! * `Lost_` supplies a replacement when the requested position is unavailable.
//!
//! Positions are truncated to `position_bytes` on the wire and expanded relative to the local
//! position. Journal capacity must keep positions within half the encoded range. A journal that has
//! never answered in the session responds with `Lost_`, preventing unrelated session positions from
//! being accepted.
//!
//! The receiver acknowledges step boundaries once per receive call, including bytes of a guaranteed
//! packet still in progress. After reconnecting, both sides exchange positions before guaranteed
//! output resumes. The dialing side sends its login first; the accepting side calls
//! [`Control::greet`] after identifying the session. `Ok_` continues a suspended packet and `Lost_`
//! abandons it.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU32, Ordering};

use crate::lib::adhoc::connection::transmitter::resumable::{self, Map, Resumable, Session};
use crate::lib::adhoc::connection::transmitter::TxHost;

/// The kinds of the packs, in the order the generated code passes them.
pub const CONFIRM: u32 = 0;
pub const FRESH: u32 = 1;
pub const CONTINUE: u32 = 2;
/// The journal holds the position asked for: the bytes behind this pack go on from it.
pub const OK: u32 = 3;
pub const LOST: u32 = 4;

/// What a pack of the peer means to the receiving side ([`Control::heard`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Heard {
    /// Nothing for the receiver: a confirmation, the peer's position.
    Nothing,
    /// The peer's journal does not hold the position named: its stream goes on from this one, a
    /// pack in progress is dropped.
    Lost(i64),
    /// The peer's journal goes on from the position named: a guaranteed pack in progress goes on.
    Ok(i64),
}

/// Nothing posted (a `Fresh_` posts -1, so this is below it).
const NONE: i64 = -2;

/// Posted by [`Control::resume_here`]: the journal goes on from its next position.
const HERE: i64 = -3;

/// What a connection says besides its packs, and what the peer said to it: shared by the sending
/// and the receiving side of a session. Atomics only: the receiving thread posts, the sending
/// thread applies at a pack boundary.
pub struct Control {
    to_say: AtomicU32,
    /// The `Confirm_` packs to say.
    confirms: AtomicU32,
    from: AtomicI64,
    lost: AtomicI64,
    /// The border the peer confirmed, the largest: for the journal (-1 — none).
    acked: AtomicI64,
    /// The border of a step the peer confirmed last, in the journal of this side: counted from the
    /// position answered.
    confirmed: AtomicI64,
    /// The peer's `Continue_` as it travelled, -1 — its `Fresh_`: for the journal ([`NONE`] —
    /// none).
    continue_: AtomicI64,
    /// What of a position travels.
    mask: i64,
    /// A step is `1 << shift` bytes.
    shift: u32,
    /// Offline queue-drain requests. Transition 0 to 1 claims the drain; later requests require
    /// another pass through [`journal_queue`](super::transmitter::resumable::journal_queue).
    pub(crate) draining: AtomicU32,
    /// What the offline pull serializes into: the bytes of a guaranteed pack go on into the
    /// journal. Only by who holds the journal ( `PULL` ): no lock of its own.
    pub(crate) scratch: crate::lib::adhoc::utils::OwnCell<Vec<u8>>,
    /// The number of the newest source of the session (`ResumableSource`): one a newer replaced
    /// says nothing of the journal.
    pub(crate) sources: AtomicU32,
    /// The answer `Ok_`: the position the journal goes on from, as the peer asked.
    ok: AtomicI64,
    /// This side named its position on this socket.
    asked: AtomicBool,
    /// And the peer has not answered it yet: what is received meanwhile is not confirmed.
    asking: AtomicBool,
    /// The position named.
    asked_at: AtomicI64,
    /// The peer named its position on this socket: the side that made the connection names its own
    /// only then.
    heard_: AtomicBool,
    /// The position of this side's decoder the two agreed on, on this socket ([`NONE`] — none).
    agreed: AtomicI64,
    /// Until the position the peer names is answered, the journal sends nothing but these packs
    /// ([`Control::agreeing`]).
    agreeing: AtomicBool,
    /// The journal of this side has answered in this session.
    answered: AtomicBool,
    /// `Fresh_` in a session the journal of this side has answered in: a new run of the peer took
    /// the session by its account ([`Control::anew`]).
    anew: AtomicBool,
    /// The session is identified on this socket: the parked journal goes on at the next fill.
    online_: AtomicBool,

    /// The header of the files of the session, mapped: null - the session lives in memory only.
    head: AtomicPtr<u8>,
    /// The number of the state of the connection's own actor + 1, 0 - none ([`Control::actor0`]).
    actor0_: AtomicI32,
    /// The name of the files of the session: empty - none.
    name: parking_lot::Mutex<String>,
    /// The mapping of the header, and the one of the session before - a thread that read its address as that session ended
    /// writes into memory still mapped. The lock of attaching, stopping, unbinding.
    maps: parking_lot::Mutex<[Option<Map>; 2]>,
    /// What the generated `save` writes ([`Control::meta`]); its lock.
    meta_out: parking_lot::Mutex<Vec<u8>>,

    /// `resumable_sync_ms` of the journal of the peer: the position of the receiver goes to the disk as that journal does; -1 -
    /// it does not ([`Control::durable`]).
    rx_sync_ms: i32,
    rx_sync_ns: i64,
    /// The bytes of the peer's journal the receiver may hold off the disk: half of that journal.
    rx_hold: i64,
    /// The position of the receiver the header on the disk has; `i64::MIN` - none of this count. On the thread that receives.
    rx_synced: AtomicI64,
    /// The clock ( `engine::now_nanos` ) of the last sync.
    rx_synced_at: AtomicI64,
    /// The header, written through: opened at the first sync.
    rx_through: parking_lot::Mutex<Option<std::fs::File>>,
}

/// The word of the header at `at`, little-endian.
///
/// # Safety
/// `h` is a header mapped ([`Control::HEAD`] bytes), `at` within it, aligned.
#[inline(always)]
pub(crate) unsafe fn set_i64(h: *mut u8, at: usize, v: i64) {
    (h.add(at) as *mut i64).write(v.to_le())
}

/// # Safety
/// As [`set_i64`].
#[inline(always)]
pub(crate) unsafe fn set_i32(h: *mut u8, at: usize, v: i32) {
    (h.add(at) as *mut i32).write(v.to_le())
}

/// # Safety
/// As [`set_i64`].
#[inline(always)]
pub(crate) unsafe fn get_i64(h: *const u8, at: usize) -> i64 {
    i64::from_le((h.add(at) as *const i64).read())
}

/// # Safety
/// As [`set_i64`].
#[inline(always)]
pub(crate) unsafe fn get_i32(h: *const u8, at: usize) -> i32 {
    i32::from_le((h.add(at) as *const i32).read())
}

#[inline]
fn le_i32(h: &[u8], at: usize) -> i32 {
    i32::from_le_bytes(h[at..at + 4].try_into().unwrap())
}

#[inline]
fn le_i64(h: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(h[at..at + 8].try_into().unwrap())
}

impl Control {
    /// `position_bytes` — how many bytes of a position travel; `step` — a `Confirm_` for every so
    /// many bytes received: a power of two; `rx_sync_ms` — `resumable_sync_ms` of the journal of the
    /// peer: the position of the receiver goes to the disk as that journal does, -1 — it does not;
    /// `rx_hold` — the bytes of the peer's journal the receiver may hold off the disk: half of that
    /// journal.
    pub fn new(position_bytes: usize, step: i64, rx_sync_ms: i32, rx_hold: i64) -> Self {
        Self {
            to_say: AtomicU32::new(0),
            confirms: AtomicU32::new(0),
            from: AtomicI64::new(0),
            lost: AtomicI64::new(0),
            acked: AtomicI64::new(-1),
            confirmed: AtomicI64::new(0),
            continue_: AtomicI64::new(NONE),
            mask: if position_bytes < 8 { (1i64 << (position_bytes * 8)) - 1 } else { -1 },
            shift: step.trailing_zeros(),
            draining: AtomicU32::new(0),
            scratch: crate::lib::adhoc::utils::OwnCell::new(Vec::new()),
            sources: AtomicU32::new(0),
            ok: AtomicI64::new(0),
            asked: AtomicBool::new(false),
            asking: AtomicBool::new(false),
            asked_at: AtomicI64::new(-1),
            heard_: AtomicBool::new(false),
            agreed: AtomicI64::new(NONE),
            agreeing: AtomicBool::new(false),
            answered: AtomicBool::new(false),
            anew: AtomicBool::new(false),
            online_: AtomicBool::new(false),
            head: AtomicPtr::new(std::ptr::null_mut()),
            actor0_: AtomicI32::new(0),
            name: parking_lot::Mutex::new(String::new()),
            maps: parking_lot::Mutex::new([None, None]),
            meta_out: parking_lot::Mutex::new(Vec::new()),
            rx_sync_ms,
            rx_sync_ns: rx_sync_ms as i64 * 1_000_000,
            rx_hold,
            rx_synced: AtomicI64::new(i64::MIN),
            rx_synced_at: AtomicI64::new(0),
            rx_through: parking_lot::Mutex::new(None),
        }
    }

    /// What the peer has confirmed of the journal of this side: a position, which moves as each
    /// confirmation is heard - what a client watches the speed of its link by
    /// (`Network::TCP::Redial`).
    #[inline]
    pub fn confirmed(&self) -> i64 {
        self.confirmed.load(Ordering::Relaxed)
    }

    /// A step is `1 << shift()` bytes.
    #[inline]
    pub fn shift(&self) -> u32 {
        self.shift
    }

    /// The peer confirms what it received once per this many bytes (`Confirm_`): a journal holds up
    /// to a step it has sent and the peer has received, but not confirmed yet.
    #[inline]
    pub fn step(&self) -> i64 {
        1i64 << self.shift
    }

    #[inline]
    fn say(&self, kind: u32) {
        self.to_say.fetch_or(1 << kind, Ordering::AcqRel);
    }

    /// Something is to be said: the sender of the connection is to be woken.
    #[inline]
    pub fn to_say(&self) -> bool {
        self.to_say.load(Ordering::Acquire) != 0
    }

    /// Something is to be said now: a confirmation waits while a replay is due (`no_confirm`).
    #[inline]
    pub fn pending(&self, no_confirm: bool) -> bool {
        self.to_say.load(Ordering::Acquire) & if no_confirm { !(1 << CONFIRM) } else { u32::MAX } != 0
    }

    /// The receiver crossed `borders` borders of a step since it confirmed last (on the thread that
    /// receives).
    pub fn confirm(&self, borders: u32) {
        if self.asking.load(Ordering::Acquire) {
            return; // the answer to the position named sets the count anew
        }
        self.confirms.fetch_add(borders, Ordering::AcqRel);
        self.say(CONFIRM);
    }

    /// The session goes on on this connection: the peer is asked to continue its stream from
    /// `received` (`-1` — none), the position of this side's decoder with the bytes of the
    /// guaranteed pack in progress. From the generated `resume`; `dialing` - this side made the
    /// connection: it names its position once the peer has named its own, until then it says
    /// nothing here. Once the two agreed on the socket, the position is named anew when it is not
    /// the one agreed on - the application moved it.
    pub fn continue_from(&self, received: i64, dialing: bool) {
        if !dialing {
            self.greet(received);
        } else if self.heard_.load(Ordering::Acquire) && received != self.agreed.load(Ordering::Acquire) {
            self.ask(received);
        }
    }

    /// This side names its position in the peer's journal on this socket, once: `-1` — none
    /// (`Fresh_`). Until the position the peer names is answered, the journal of this side sends
    /// nothing but these packs. The side that took the connection: when the login identified the
    /// session. The side that made it: when the peer's position is heard.
    pub fn greet(&self, received: i64) {
        if self.asked.swap(true, Ordering::AcqRel) {
            return;
        }
        self.agreeing.store(true, Ordering::Release);
        self.ask(received);
    }

    #[inline]
    fn ask(&self, at: i64) {
        self.asked_at.store(at, Ordering::Release);
        self.asking.store(true, Ordering::Release);
        if at < 0 {
            self.say(FRESH);
        } else {
            self.from.store(at, Ordering::Release);
            self.say(CONTINUE);
        }
    }

    /// Nothing but these packs goes out: the position the peer names is not answered yet (a journal
    /// only).
    #[inline]
    pub fn agreeing(&self) -> bool {
        self.agreeing.load(Ordering::Acquire)
    }

    /// The connection is gone: what was to be said on it is not said on the next one, and the next
    /// one is agreed on anew.
    pub fn silent(&self) {
        self.to_say.store(0, Ordering::Release);
        self.confirms.store(0, Ordering::Release);
        self.asked.store(false, Ordering::Release);
        self.asking.store(false, Ordering::Release);
        self.heard_.store(false, Ordering::Release);
        self.agreed.store(NONE, Ordering::Release);
        self.agreeing.store(false, Ordering::Release);
        self.online_.store(false, Ordering::Release);
    }

    /// The session is identified on this socket (the generated `resume`): the source of the socket
    /// takes the parked journal on at its next fill.
    #[inline]
    pub fn online(&self) {
        self.online_.store(true, Ordering::Release);
    }

    /// The session was identified on this socket since the journal parked: taken once - a datagram
    /// transport keeps its source across a park, so a word of the socket before is not one of the
    /// next.
    #[inline]
    pub(crate) fn take_online(&self) -> bool {
        self.online_.swap(false, Ordering::AcqRel)
    }

    /// The session is over: a journal of the next one has not answered yet, and its files go.
    pub fn ended<T: Session>(&self, conn: &T) {
        self.answered.store(false, Ordering::Release);
        self.anew.store(false, Ordering::Release);
        self.unbind(conn);
    }

    /// Appends what the next process needs of this side's word in the session: whether the journal
    /// of this side has answered in it - a position the peer names on the next socket is then one
    /// of this journal -, and the name of the files of the session.
    pub fn save(&self, out: &mut Vec<u8>) {
        out.push(self.answered.load(Ordering::Acquire) as u8);
        let name = self.name.lock();
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }

    /// Takes up what [`Control::save`] wrote in the process before, and the files of the session with it: the journal and
    /// the position of the receiver as they were the instant that process ended - see [`Control::attach`].
    pub fn load<T: Session>(&self, conn: &T, from: &mut &[u8]) -> std::io::Result<()> {
        let answered = resumable::take(from, 1)?[0] != 0;
        self.answered.store(answered, Ordering::Release);
        let n = resumable::take_u32(from)? as usize;
        let name = String::from_utf8_lossy(resumable::take(from, n)?).into_owned();
        if !name.is_empty() && self.head.load(Ordering::Acquire).is_null() {
            self.attach(conn, &name, false, true)?;
        }
        Ok(())
    }

    // The file of the session
    //
    // A session lives in files of its own as it goes, under `resumable::dir()`: <name>.head - the header, the positions and
    // the state, each word stored where it changes, a plain store into a mapped page -, <name>.ring - the ring of the journal,
    // mapped -, <name>.<position>.seg - the backlog beyond it. What is stored into a mapped page is the system's at once: a
    // process that falls takes nothing of it along ( Chronicle Queue, Aeron, Kafka keep their logs so ). What the system had
    // not written to the disk when the power went is not: the journal of a session the header tells was left by another boot
    // of the system, unless it was closed cleanly, is not taken up. The layout is the one of the Java lib.

    /// "AdHocJ1".
    pub const MAGIC: i64 = 0x0031_4A63_6F48_6441;
    // the words of the header: the session; the journal - the transmit side stores them; the receiver - the receive side
    pub const H_MAGIC: usize = 0;
    pub const H_BOOT: usize = 8;
    pub const H_FLAGS: usize = 16;
    pub const H_KIND: usize = 24;
    pub const H_ID: usize = 28;
    pub const H_PARKED: usize = 32;
    pub const H_META: usize = 40;
    pub const H_ACTOR0: usize = 48;
    pub const H_OPEN: usize = 52;
    pub const H_BASE: usize = 64;
    pub const H_ACKED: usize = 72;
    pub const H_NEXT: usize = 80;
    pub const H_HOT_BASE: usize = 88;
    pub const H_HOT_OFF: usize = 96;
    pub const H_HOT_CAP: usize = 100;
    pub const H_WHOLE_END: usize = 104;
    pub const H_JFLAGS: usize = 112;
    pub const H_GUARDED: usize = 128;
    pub const H_POSITIONED: usize = 136;
    // the journal as it is on the disk ( resumable_sync_ms ): what a reboot leaves of it - the words above are what a process
    // that fell leaves. Written through, behind the bytes they name
    pub const H_D_BASE: usize = 160;
    pub const H_D_ACKED: usize = 168;
    pub const H_D_NEXT: usize = 176;
    pub const H_D_HOT_BASE: usize = 184;
    pub const H_D_HOT_OFF: usize = 192;
    pub const H_D_HOT_CAP: usize = 196;
    pub const H_D_WHOLE_END: usize = 200;
    pub const H_D_JFLAGS: usize = 208;
    pub const H_D_VALID: usize = 212;
    pub const H_D_END: usize = 216;
    // the position of the receiver as it is on the disk ( resumable_sync_ms of the peer's journal ): written through
    pub const H_D_GUARDED: usize = 224;
    pub const H_D_GVALID: usize = 232;
    pub const H_D_GEND: usize = 236;
    /// The trace of the queue of the transmitter: a cell a place of the queue, as wide as the id of a pack, id + 1 of the
    /// guaranteed pack queued there, 0 - none; [`H_OPEN`](Self::H_OPEN) - id + 1 of the guaranteed pack the transmitter has begun.
    pub const TRACE: usize = 256;
    /// Two places for what the generated `save` writes: the next one is written while the last is the valid one.
    pub const META: usize = 4096;
    pub const SLOT: usize = 30720;
    pub const HEAD: usize = Self::META + 2 * Self::SLOT;
    /// [`H_FLAGS`](Self::H_FLAGS): closed cleanly.
    pub const CLEAN: i32 = 1;
    /// [`H_FLAGS`](Self::H_FLAGS): taken up by the host at the start ( `Pool::restore` ), otherwise by the application.
    pub const HOST: i32 = 2;

    /// The session goes on on a socket: it is in files from here on - the first time a socket of an identified peer carries
    /// it -, and it is not parked. The generated `resume`, before the session is put online.
    pub fn live<T: Session>(&self, conn: &T) {
        let h = self.head.load(Ordering::Acquire);
        if !h.is_null() {
            // SAFETY: the header is mapped while it is published
            unsafe { set_i64(h, Self::H_PARKED, 0) };
            return;
        }
        let Some(of) = conn.files() else { return }; // a datagram transport, a virtual connection: in memory
        if conn.ext_connection().map_or(0, |x| x.id()) == 0 || !resumable::files() {
            return;
        }
        let name = format!("{of}.{:x}", resumable::seed_start() as u64);
        if let Err(e) = self.attach(conn, &name, true, false) {
            crate::lib::adhoc::connection::transmitter::on_error::tx_at(crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error, &"the files of the session", &name, Some(&e));
        }
    }

    /// The application keeps the session in the files `name` names, and takes it up from them at its start: a session that is
    /// not of a host - one of no socket yet, say. Before the session carries anything. `true` - the files held a session of the
    /// process before, and it goes on here: what the generated `save` wrote of it is read back (`load` of the connection), the
    /// journal and the position of the receiver are as that process left them. `false` - a fresh one.
    pub fn open<T: Session>(&self, conn: &T, name: &str) -> bool {
        if !self.head.load(Ordering::Acquire).is_null() || !resumable::files() {
            return false;
        }
        let h = Self::peek(&resumable::dir().join(format!("{name}.head")));
        if let Some((h, (at, len))) = h.as_deref().and_then(|b| Self::meta_in(b).map(|m| (b, m))) {
            if conn.load(&mut &h[at + 8..at + 8 + len]).is_ok() {
                if self.head.load(Ordering::Acquire).is_null() {
                    let _ = self.attach(conn, name, false, true); // the generated save did not name the files
                }
                return true;
            }
        }
        Self::delete(name);
        if let Err(e) = self.attach(conn, name, false, false) {
            crate::lib::adhoc::connection::transmitter::on_error::tx_at(crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error, &"the files of the session", &name, Some(&e));
        }
        false
    }

    /// Maps the header of the files `name` names. A fresh session stores what it is; a session of the process before hands what
    /// the header says on to the journal and the receiver, the journal only when the header can be trusted: closed cleanly, or
    /// left by this boot of the system - a process that fell. Either way the header is not clean from here on, and is on the
    /// disk so before anything of this process changes the files.
    fn attach<T: Session>(&self, conn: &T, name: &str, host: bool, restoring: bool) -> std::io::Result<()> {
        let base = resumable::dir().join(name);
        let mut maps = self.maps.lock();
        if !self.head.load(Ordering::Acquire).is_null() {
            return Ok(());
        }
        let mut head_file = base.clone().into_os_string();
        head_file.push(".head");
        let map = Map::open(std::path::Path::new(&head_file), Self::HEAD, false)?;
        let h = map.ptr();
        let boot = resumable::boot();
        let mut host = host;
        let mut trusted = false;
        let mut rebooted = false;
        // SAFETY: the header just mapped, HEAD bytes
        unsafe {
            if restoring {
                let flags = get_i32(h, Self::H_FLAGS);
                host = flags & Self::HOST != 0;
                trusted = flags & Self::CLEAN != 0 || boot != 0 && get_i64(h, Self::H_BOOT) == boot;
                rebooted = !trusted;
                if !trusted && get_i32(h, Self::H_D_VALID) != 0 {
                    // a reboot, and the journal was written through ( resumable_sync_ms ): it goes on as the disk has it
                    for (live, disk) in [(Self::H_BASE, Self::H_D_BASE), (Self::H_ACKED, Self::H_D_ACKED), (Self::H_NEXT, Self::H_D_NEXT), (Self::H_HOT_BASE, Self::H_D_HOT_BASE), (Self::H_WHOLE_END, Self::H_D_WHOLE_END)] {
                        set_i64(h, live, get_i64(h, disk));
                    }
                    for (live, disk) in [(Self::H_HOT_OFF, Self::H_D_HOT_OFF), (Self::H_HOT_CAP, Self::H_D_HOT_CAP), (Self::H_JFLAGS, Self::H_D_JFLAGS)] {
                        set_i32(h, live, get_i32(h, disk));
                    }
                    trusted = true;
                }
                if flags & Self::CLEAN == 0 {
                    self.report_lost(h, name, trusted, conn.id_mask());
                }
                let a = get_i32(h, Self::H_ACTOR0);
                if a != 0 {
                    self.actor0_.store(a, Ordering::Release);
                }
            } else {
                set_i64(h, Self::H_MAGIC, Self::MAGIC);
                set_i32(h, Self::H_KIND, conn.kind() as i32);
                set_i32(h, Self::H_ID, conn.ext_connection().map_or(0, |x| x.id()) as i32);
                set_i64(h, Self::H_PARKED, 0);
                set_i32(h, Self::H_ACTOR0, self.actor0_.load(Ordering::Acquire));
            }
            set_i64(h, Self::H_BOOT, boot);
            set_i32(h, Self::H_FLAGS, if host { Self::HOST } else { 0 });
        }
        map.force()?;
        *self.name.lock() = name.to_owned();
        maps[1] = maps[0].take();
        maps[0] = Some(map);
        self.head.store(h, Ordering::Release);
        if let Some(t) = conn.trace() {
            t.store(h, Ordering::Release);
        }
        if let Some(j) = conn.journal() {
            j.attach(h, base, restoring, trusted);
        }
        if let Some(p) = conn.position() {
            // SAFETY: the session gets its files on the thread that runs it ( `live`, from its `resume` ), or before it carries
            // anything ( `open`, `Pool::restore` ): nothing receives into the position meanwhile
            unsafe { p.lend(|p| p.attach(h, restoring, rebooted, self)) };
            // SAFETY: the header just mapped
            if restoring && unsafe { get_i32(h, Self::H_D_GVALID) } != 0 {
                self.rx_synced.store(unsafe { get_i64(h, Self::H_D_GUARDED) }, Ordering::Relaxed); // what Confirm_ may name: the disk, not what the mapping had
            }
        }
        drop(maps);
        if !restoring {
            self.meta(conn);
        }
        Ok(())
    }

    /// Writes what the generated `save` writes of the session - the region of the application, the roles, the states of the
    /// actors - to the place of the header the last write did not take; the header names it once it is whole. `save`
    /// returning `false`: the session is not taken up by the next process.
    fn meta<T: Session>(&self, conn: &T) {
        let mut out = self.meta_out.lock();
        let h = self.head.load(Ordering::Acquire);
        if h.is_null() {
            return;
        }
        out.clear();
        let mut len = if conn.save(&mut out) { out.len() as i64 } else { -1 };
        if ((Self::SLOT - 8) as i64) < len {
            crate::lib::adhoc::connection::transmitter::on_error::tx_at(crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error, &format_args!("What the session saves of itself, {len} bytes"), &format_args!("does not fit the {} bytes of the header: the next process will not take it up", Self::SLOT - 8), None);
            len = -1;
        }
        // SAFETY: the header is mapped while it is published; the places of META are written under `meta_out` only
        unsafe {
            let seq = get_i32(h, Self::H_META).wrapping_add(1);
            let at = h.add(Self::META + (seq & 1) as usize * Self::SLOT);
            let mut crc = 0u32;
            if 0 < len {
                std::ptr::copy_nonoverlapping(out.as_ptr(), at.add(8), len as usize);
                crc = crate::lib::adhoc::helpers::crc32c(0, &out);
            }
            set_i32(at, 0, len as i32);
            set_i32(at, 4, crc as i32);
            set_i32(h, Self::H_META, seq); // the place is the session's from here on
        }
    }

    /// What the region of the application in the generated `save` writes has changed: it is written to the header of the
    /// session now, and a process that falls after it leaves it so. Without this call it is written when the session gets its
    /// files, when it loses a socket, and when the process ends cleanly. On the thread of the connection, or while no thread
    /// runs it.
    pub fn saved<T: Session>(&self, conn: &T) {
        self.meta(conn);
    }

    /// The connection's own actor took the state of number `index` among its states. The header has it at once; what the
    /// generated `save` wrote of it is older.
    pub fn actor0(&self, index: i32) {
        self.actor0_.store(index + 1, Ordering::Release);
        let h = self.head.load(Ordering::Acquire);
        if !h.is_null() {
            // SAFETY: the header is mapped while it is published
            unsafe { set_i32(h, Self::H_ACTOR0, index + 1) };
        }
    }

    /// The number of the state of the connection's own actor the header has, `saved` - what the generated `save` wrote - when
    /// it has none. The generated `load`.
    pub fn actor0_or(&self, saved: i32) -> i32 {
        let s = self.actor0_.load(Ordering::Acquire);
        if s == 0 { saved } else { s - 1 }
    }

    //#region the position of the receiver outlives the power: resumable_sync_ms of the peer's journal
    // The position of the receiver goes to the disk as the journal of the peer does - the word of the owner, 6.11: the moment of
    // the last sync is kept, and at the end of each receive call the clock is looked at - rx_sync_ms or more since that sync, or
    // half of the peer's journal off the disk: the position goes to the header on the disk, written through ( H_D_GUARDED );
    // otherwise the sync is passed by. Confirm_ names no more than the disk has: the peer keeps what a reboot here asks for
    // again, and is told no Lost_ of packs this side handled. No timer: a position the last sync passed by goes to the disk on
    // the next receive call - the next pack of the peer, or its Pulse_.

    /// The receiver goes on from `position`: one the disk does not have is of a count the disk knows nothing of yet - a `Lost_`
    /// moved it, say. On the thread that receives.
    #[inline]
    pub(crate) fn rx_from(&self, position: i64) {
        if position != self.rx_synced.load(Ordering::Relaxed) {
            self.rx_synced.store(i64::MIN, Ordering::Relaxed);
        }
    }

    /// The end of a receive call: the receiver has handled the packs up to `guarded`, `at` - with what it holds of a pack in
    /// progress. The position goes to the disk if it is time; what `Confirm_` may name is returned - no pack end past the disk.
    /// On the thread that receives.
    pub fn durable(&self, guarded: i64, at: i64) -> i64 {
        if self.rx_sync_ms < 0 || self.head.load(Ordering::Relaxed).is_null() {
            return at;
        }
        let mut synced = self.rx_synced.load(Ordering::Relaxed);
        if synced < guarded {
            let now = crate::lib::network::engine::now_nanos();
            if synced == i64::MIN || self.rx_sync_ns <= now - self.rx_synced_at.load(Ordering::Relaxed) || self.rx_hold <= guarded - synced {
                let mut through = self.rx_through.lock();
                let h = self.head.load(Ordering::Acquire); // under the lock: stop(), unbind() close the handle behind the header
                if !h.is_null() {
                    let r = (|| -> std::io::Result<()> {
                        if through.is_none() {
                            let mut o = std::fs::OpenOptions::new();
                            o.write(true);
                            resumable::os::through(&mut o);
                            *through = Some(o.open(resumable::dir().join(format!("{}.head", self.name.lock())))?);
                        }
                        // SAFETY: the header is mapped while it is published; the mapping before is kept ( maps )
                        let words = unsafe {
                            set_i64(h, Self::H_D_GUARDED, guarded);
                            set_i32(h, Self::H_D_GVALID, 1);
                            std::slice::from_raw_parts(h.add(Self::H_D_GUARDED), Self::H_D_GEND - Self::H_D_GUARDED)
                        };
                        resumable::os::write_at(through.as_ref().unwrap(), words, Self::H_D_GUARDED as u64)
                    })();
                    match r {
                        Ok(()) => {
                            synced = guarded;
                            self.rx_synced.store(guarded, Ordering::Relaxed);
                            self.rx_synced_at.store(now, Ordering::Relaxed);
                        }
                        Err(e) => crate::lib::adhoc::connection::transmitter::on_error::tx_at(crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error, &"the position of the receiver", &"could not be written to the disk", Some(&e)),
                    }
                }
            }
        }
        if synced < guarded { synced } else { at }
    }
    //#endregion

    /// The socket of the session is lost and the session parks: since when - its wait counts from there in the next process -,
    /// and what it is, to the header.
    pub(crate) fn parked<T: Session>(&self, conn: &T) {
        let h = self.head.load(Ordering::Acquire);
        if h.is_null() {
            return;
        }
        // SAFETY: the header is mapped while it is published
        unsafe { set_i64(h, Self::H_PARKED, resumable::now()) };
        self.meta(conn);
    }

    /// The process before ended without a stop: the guaranteed packs the trace of the queue of the transmitter names were
    /// queued and not journalled - lost, and so is a pack the transmitter had begun. Their ids go to `lost.<name>.txt`, a line a
    /// fall - it outlives the session -, and the trace is cleared for this process.
    ///
    /// # Safety
    /// `h` is the header mapped.
    unsafe fn report_lost(&self, h: *mut u8, name: &str, trusted: bool, id_mask: u32) {
        let w = ((32 - id_mask.leading_zeros() as usize + 7) >> 3).max(1);
        let cell = if w == 3 { 4 } else { w };
        let cells = (Self::META - Self::TRACE) / cell;
        let mut ids = String::new();
        let mut n = 0;
        for i in 0..cells {
            let at = h.add(Self::TRACE + i * cell);
            let id = match cell {
                1 => *at as u32,
                2 => u16::from_le((at as *const u16).read()) as u32,
                _ => get_i32(at, 0) as u32,
            };
            if id == 0 {
                continue;
            }
            if n != 0 {
                ids.push_str(", ");
            }
            ids.push_str(&(id - 1).to_string());
            n += 1;
        }
        let open = get_i32(h, Self::H_OPEN);
        if n == 0 && open == 0 {
            return;
        }
        std::ptr::write_bytes(h.add(Self::TRACE), 0, Self::META - Self::TRACE);
        set_i32(h, Self::H_OPEN, 0);
        let mut line = format!("{} the process that held this session ended without a stop{}.", utc(resumable::now()), if trusted { "" } else { ", the system rebooted - possibly" });
        if n != 0 {
            line.push_str(&format!(" Guaranteed packs queued, not journalled, lost: {n}, ids {ids}."));
        }
        if open != 0 {
            line.push_str(&format!(" A guaranteed pack begun, lost: id {}.", open - 1));
        }
        line.push('\n');
        let file = resumable::dir().join(format!("lost.{name}.txt"));
        if let Err(e) = std::fs::OpenOptions::new().create(true).append(true).open(&file).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes())) {
            crate::lib::adhoc::connection::transmitter::on_error::tx_at(crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error, &file.display(), &"could not be written", Some(&e));
        }
    }

    /// The process ends: what the transmitter holds goes into the journal, everything of the session to the disk, and the
    /// header says it was closed cleanly - the next process takes it up whatever happens to the system meanwhile. The session
    /// is parked ( `Pool::stop` ).
    pub fn stop<T>(&self, conn: &T)
    where
        for<'a> &'a T: TxHost,
        T: Session,
    {
        if self.head.load(Ordering::Acquire).is_null() {
            return;
        }
        self.meta(conn);
        let mut maps = self.maps.lock();
        let h = self.head.load(Ordering::Acquire);
        if h.is_null() {
            return;
        }
        let r = (|| -> std::io::Result<()> {
            if let Some(m) = conn.journal() {
                resumable::excluding_pulls(conn, || -> std::io::Result<()> {
                    while resumable::pull_offline(conn, m)? {} // the pack in the middle of serialization, the packs queued after it
                    m.take_offline(false);
                    // SAFETY: taken
                    let r = unsafe { m.get() }.stop();
                    m.give();
                    r
                })?;
            }
            // SAFETY: the header is mapped while it is published
            unsafe { set_i32(h, Self::H_FLAGS, get_i32(h, Self::H_FLAGS) | Self::CLEAN) };
            maps[0].as_ref().map_or(Ok(()), |m| m.force())
        })();
        if let Err(e) = r {
            crate::lib::adhoc::connection::transmitter::on_error::tx_at(crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error, &"the session", &"could not be closed cleanly", Some(&e));
        }
        self.detached(conn);
        maps[1] = maps[0].take();
    }

    /// The session is over: its files go.
    pub(crate) fn unbind<T: Session>(&self, conn: &T) {
        let mut maps = self.maps.lock();
        let h = self.head.load(Ordering::Acquire);
        if h.is_null() {
            return;
        }
        // SAFETY: the header is mapped while it is published: a header left behind - its deletion did not come to pass - is not
        // one of a session any more
        unsafe { set_i64(h, Self::H_MAGIC, 0) };
        self.detached(conn);
        if let Some(j) = conn.journal() {
            j.unattach(); // the holder leaves the files of the journal as it ends the session ( Journal::end, before this )
        }
        maps[1] = maps[0].take();
        let name = std::mem::take(&mut *self.name.lock());
        Self::delete(&name);
    }

    /// The header is not of this session any more: nobody stores into it from here on.
    fn detached<T: Session>(&self, conn: &T) {
        self.head.store(std::ptr::null_mut(), Ordering::Release);
        if let Some(t) = conn.trace() {
            t.store(std::ptr::null_mut(), Ordering::Release);
        }
        if let Some(p) = conn.position() {
            // SAFETY: the session ends on the thread that runs it ( `ended`, its close event ), or parked - no receive runs it ( `stop` )
            unsafe { p.lend(|p| p.attach(std::ptr::null_mut(), true, false, self)) };
        }
        // stop(), unbind(): the handle that wrote the position of the receiver through goes, and what the disk had of it is not
        // of the next files - under the lock durable() syncs under
        let mut through = self.rx_through.lock();
        self.rx_synced.store(i64::MIN, Ordering::Relaxed);
        *through = None;
    }

    /// The header of the files of a session, read: `None` - not one.
    pub fn peek(file: &std::path::Path) -> Option<Vec<u8>> {
        let mut h = vec![0u8; Self::HEAD];
        let mut f = std::fs::File::open(file).ok()?;
        std::io::Read::read_exact(&mut f, &mut h).ok()?;
        (le_i64(&h, Self::H_MAGIC) == Self::MAGIC).then_some(h)
    }

    /// Where in the header `h` the valid place of what the generated `save` wrote is - its bytes start 8 past it -, and its
    /// length; `None` - none: the session is not to be taken up.
    pub fn meta_in(h: &[u8]) -> Option<(usize, usize)> {
        let seq = le_i32(h, Self::H_META);
        for i in 0..2 {
            let at = Self::META + (seq.wrapping_sub(i) & 1) as usize * Self::SLOT;
            let len = le_i32(h, at);
            if len < 1 || ((Self::SLOT - 8) as i32) < len {
                if i == 0 && len == -1 {
                    return None; // the last word of the session: it is not to be taken up
                }
                continue;
            }
            let len = len as usize;
            if crate::lib::adhoc::helpers::crc32c(0, &h[at + 8..at + 8 + len]) == le_i32(h, at + 4) as u32 {
                return Some((at, len));
            }
        }
        None
    }

    /// The header of a session: what it is of - the connection ( [`H_KIND`](Self::H_KIND) ), the peer ( [`H_ID`](Self::H_ID) ),
    /// since when it is parked ( [`H_PARKED`](Self::H_PARKED) ), taken up by the host ( [`HOST`](Self::HOST) ).
    pub fn header(h: &[u8]) -> (u32, u32, i64, bool) {
        (le_i32(h, Self::H_KIND) as u32, le_i32(h, Self::H_ID) as u32, le_i64(h, Self::H_PARKED), le_i32(h, Self::H_FLAGS) & Self::HOST != 0)
    }

    /// Deletes the files of the session `name` names.
    pub fn delete(name: &str) {
        if name.is_empty() {
            return;
        }
        let prefix = format!("{name}.");
        if let Ok(rd) = std::fs::read_dir(resumable::dir()) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    /// The peer is a new run that took the session by its account: it said `Fresh_` in a session
    /// the journal of this side has answered in - the run that had the session is gone, and with it
    /// everything of the session on that side. Once: the generated code tells the session (`lost`).
    #[inline]
    pub fn anew(&self) -> bool {
        self.anew.load(Ordering::Relaxed) && self.anew.swap(false, Ordering::AcqRel)
    }

    /// The position `low`, as it travels, means: the nearest one to `at`.
    #[inline]
    pub fn resolve(&self, low: i64, at: i64) -> i64 {
        if self.mask == -1 {
            return low;
        }
        let mut d = (low - at) & self.mask;
        if self.mask >> 1 < d {
            d -= self.mask + 1;
        }
        at + d
    }

    /// The peer's pack of the kind `kind`, the position it carries as it travelled `low` (on the
    /// thread that receives): a confirmation and the peer's position are posted to the sending side
    /// - the side that made the connection names its own position on the peer's one, `named` gives
    /// it; an answer to the position of this side goes to the receiver.
    pub fn heard(&self, kind: u32, low: u64, named: impl Fn() -> i64, dialing: bool) -> Heard {
        match kind {
            CONFIRM => {
                let shift = self.shift;
                let border = match self.confirmed.try_update(Ordering::AcqRel, Ordering::Acquire, |c| Some(((c >> shift) + 1) << shift)) {
                    Ok(c) | Err(c) => ((c >> shift) + 1) << shift,
                };
                self.acked.fetch_max(border, Ordering::AcqRel);
                Heard::Nothing
            }
            FRESH | CONTINUE => {
                self.heard_.store(true, Ordering::Release);
                if kind == FRESH && self.answered.load(Ordering::Acquire) {
                    self.anew.store(true, Ordering::Release);
                }
                if dialing {
                    self.greet(named()); // the peer is identified and named its position: this one names its own first
                }
                self.continue_.store(if kind == FRESH { -1 } else { low as i64 }, Ordering::Release);
                Heard::Nothing
            }
            OK => {
                self.asking.store(false, Ordering::Release);
                let at = self.asked_at.load(Ordering::Acquire);
                self.agreed.store(at, Ordering::Release);
                Heard::Ok(at)
            }
            _ => {
                self.asking.store(false, Ordering::Release);
                let at = named();
                let position = if at < 0 { low as i64 } else { self.resolve(low as i64, at) };
                self.agreed.store(position, Ordering::Release);
                Heard::Lost(position)
            }
        }
    }

    /// The sending side, at any read: what the peer confirmed received is given back
    /// - a pack in the middle held at the cap goes on.
    #[inline]
    pub fn acks(&self, journal: &mut Resumable) {
        // a load first: most reads find no confirmation - the line the receiving thread writes is
        // not taken for nothing (C6)
        if self.acked.load(Ordering::Relaxed) < 0 {
            return;
        }
        let a = self.acked.swap(-1, Ordering::AcqRel);
        if 0 <= a {
            journal.ack(a);
        }
    }

    /// The sending side, at a pack boundary: what the peer said goes to the journal. A position the
    /// journal does not hold is answered with `Lost_` at once; one it holds waits for the replay in
    /// progress to end - the stream switches between two packs, never inside one - and one the
    /// replay in progress started from is served already.
    pub fn apply(&self, journal: &mut Resumable) {
        self.acks(journal);
        let c = self.continue_.load(Ordering::Acquire);
        if c == NONE {
            return;
        }
        let position = if c == HERE {
            journal.next()
        } else if c < 0 || !self.answered.load(Ordering::Acquire) {
            -1 // none named, or a journal that has not answered in its session: the position is not of it
        } else {
            self.resolve(c, journal.next())
        };
        let held = 0 <= position && journal.holds(position);
        if held && position != journal.replaying() && journal.sent() != journal.next() {
            return; // a replay in progress: the resume waits for its end
        }
        if self.continue_.compare_exchange(c, NONE, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return; // a newer one came: the next boundary takes it
        }
        if c != HERE {
            self.answered.store(true, Ordering::Release);
        }
        if held && position == journal.replaying() {
            self.agreeing.store(false, Ordering::Release);
            return; // the replay in progress serves it
        }
        let at = journal.resume_at(position);
        if at != -1 {
            self.lost.store(at, Ordering::Release);
            self.confirmed.store(at, Ordering::Release);
            self.say(LOST);
        } else if c != HERE {
            self.ok.store(position, Ordering::Release);
            self.confirmed.store(position, Ordering::Release);
            self.say(OK); // the answer: the replay of the journal follows it
        }
        self.agreeing.store(false, Ordering::Release);
    }

    /// Nobody to wait for: the journal goes on from where it is, as if the peer had named its
    /// position. Posted, as a `Continue_` of the peer is: the transmit applies it at a pack
    /// boundary - it is to be woken.
    pub fn resume_here(&self) {
        self.continue_.store(HERE, Ordering::Release);
    }

    /// The socket is gone: a `Continue_` its peer said and the transmit did not apply is of that
    /// socket only - the next socket waits for its own.
    pub(crate) fn forget(&self) {
        self.continue_.store(NONE, Ordering::Release);
    }

    /// The pack that is due: its kind and the position it carries as it travels; `None` — none
    /// (`no_confirm` — a confirmation waits). The position of this side first: the answer is right
    /// before the journal.
    pub fn next(&self, no_confirm: bool) -> Option<(u32, u64)> {
        let s = self.to_say.load(Ordering::Acquire) & if no_confirm { !(1 << CONFIRM) } else { u32::MAX };
        if s == 0 {
            return None;
        }
        let kind = if s & 1 << CONTINUE != 0 {
            CONTINUE
        } else if s & 1 << FRESH != 0 {
            FRESH
        } else if s & 1 << LOST != 0 {
            LOST
        } else if s & 1 << OK != 0 {
            OK
        } else {
            CONFIRM
        };
        self.to_say.fetch_and(!(1 << kind), Ordering::AcqRel); // taken first, read second: a position written after this is said again
        let value = match kind {
            CONFIRM => {
                if 1 < self.confirms.try_update(Ordering::AcqRel, Ordering::Acquire, |c| Some(c.saturating_sub(1))).unwrap_or(0) {
                    self.say(CONFIRM); // more to confirm
                }
                0
            }
            CONTINUE => self.from.load(Ordering::Acquire) & self.mask,
            LOST => self.lost.load(Ordering::Acquire) & self.mask,
            _ => 0,
        };
        Some((kind, value as u64))
    }

    /// The pack [`next`](Self::next) gave is out: after `Lost_` the journal goes on from the
    /// position told.
    pub fn said(&self, kind: u32, journal: Option<&mut Resumable>) {
        if kind == LOST {
            if let Some(j) = journal {
                j.resume_at(self.lost.load(Ordering::Acquire));
            }
        }
    }
}

/// `seconds` of Unix time as `yyyy-MM-ddTHH:mm:ss+00:00`: the civil date of the day count ( H. Hinnant ).
fn utc(seconds: i64) -> String {
    let (days, s) = (seconds.div_euclid(86400), seconds.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00", s / 3600, s / 60 % 60, s % 60)
}
