// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Per-host registry of reusable connection slots.
//!
//! The registry retains every slot, with loop-local free lists and a synchronized parked session
//! list. A slot stays assigned to its loop across sessions. Free-list operations run on that loop
//! alone; the slot's deadline-list placement is reused when a new session starts. An
//! [`ExternalConnection`] remains allocated while its host retains it.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use super::external_connection::{ConnId, ExternalConnection};
use crate::lib::adhoc::spin_lock::FastReentrantSpinLock;
use crate::lib::network::engine::Loop;

/// The lifecycle of a slot.
pub const FREE: u8 = 0;
pub const ACTIVE: u8 = 1;
pub const CLEANING: u8 = 2;
pub const PARKED: u8 = 3;

/// The slots of the first segment of the registry: each next one holds twice as many.
const FIRST: usize = 64;
/// Segments enough for every index a `u32` names.
const SEGMENTS: usize = 27;

/// The segment and the place in it of the slot `index`.
#[inline(always)]
fn at(index: usize) -> (usize, usize) {
    let j = index / FIRST + 1;
    let k = (usize::BITS - 1 - j.leading_zeros()) as usize;
    (k, index - FIRST * ((1 << k) - 1))
}

pub struct Pool {
    /// The name of the host: in what its connections print of themselves.
    pub name: String,
    /// The registry of all allocated connections: it only grows - a segment once made stays where
    /// it is, so a slot is found in O(1) and never moves.
    connections: [AtomicPtr<OnceLock<Arc<ExternalConnection>>>; SEGMENTS],
    /// The number of connections made: published after the slot is in place.
    connections_size: AtomicU32,
    /// Making a new slot: the only place two loops meet.
    grow: parking_lot::Mutex<()>,
    /// Per loop: the free slots of this host that live on it, through [`ExternalConnection::next`]
    /// - the index + 1 of the first. That loop's thread alone touches its list.
    free: Box<[UnsafeCell<u32>]>,
    /// The parked sessions, ascending by their deadline, through [`ExternalConnection::next`]: the
    /// index + 1 of the head. Changed under [`parked_lock`](Self::parked_lock) - a park, a claim
    /// and the reap run on different loops -, read without it by the tick that finds nothing
    /// parked.
    parked_head: AtomicU32,
    /// The last of them: a park that waits longest - the common case, most sessions of a host wait
    /// the same - goes behind it with no walk. Under the lock.
    parked_tail: AtomicU32,
    parked_lock: FastReentrantSpinLock,
    /// How many connections of this host hold a socket right now.
    pub(crate) connections_live: AtomicUsize,
    /// How long a read after our own FIN waits for the peer's, in milliseconds: a policy of the
    /// host, every session takes it when it starts.
    pub(crate) linger_timeout_ms: std::sync::atomic::AtomicI64,
    me: Weak<Pool>,
    /// The loops of the engine the connections of the host run on: a slot belongs to one of them.
    loops: Option<Arc<[Arc<Loop>]>>,
    /// Where the failures of the connections go.
    on_failure: Option<crate::lib::network::failure::OnFailure>,
}

unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}

impl Pool {
    pub(crate) fn new(name: impl Into<String>, loops: Option<Arc<[Arc<Loop>]>>, on_failure: Option<crate::lib::network::failure::OnFailure>) -> Arc<Pool> {
        let n = loops.as_ref().map_or(1, |l| l.len().max(1));
        let name = name.into();
        Arc::new_cyclic(|me| Pool {
            name,
            connections: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            connections_size: AtomicU32::new(0),
            grow: parking_lot::Mutex::new(()),
            free: (0..n).map(|_| UnsafeCell::new(0)).collect(),
            parked_head: AtomicU32::new(0),
            parked_tail: AtomicU32::new(0),
            parked_lock: FastReentrantSpinLock::new(),
            connections_live: AtomicUsize::new(0),
            linger_timeout_ms: std::sync::atomic::AtomicI64::new(5_000),
            me: me.clone(),
            loops,
            on_failure,
        })
    }

    /// How long a read after our own FIN waits for the peer's, in milliseconds.
    pub fn linger_timeout(&self) -> i64 {
        self.linger_timeout_ms.load(Ordering::Relaxed)
    }

    /// Sets it for the sessions that start from now on.
    pub fn set_linger_timeout(&self, ms: i64) {
        self.linger_timeout_ms.store(ms, Ordering::Relaxed);
    }

    /// The slot `index`, whatever its state; `None` - not made.
    #[inline]
    pub(crate) fn slot(&self, index: u32) -> Option<&Arc<ExternalConnection>> {
        if self.connections_size.load(Ordering::Acquire) <= index {
            return None;
        }
        let (k, i) = at(index as usize);
        // SAFETY: a segment is published before the size that counts a slot in it, and lives as
        // long as the pool
        unsafe { (*self.connections[k].load(Ordering::Acquire).add(i)).get() }
    }

    /// The connection `id` while a session runs on it, in O(1).
    #[inline]
    pub fn get_connection(&self, id: u32) -> Option<&Arc<ExternalConnection>> {
        self.slot(id).filter(|c| c.is_active())
    }

    /// The loop `number` of this host's engine (1 and up); `None` - a pool of no engine (a test).
    #[inline]
    fn home(&self, number: u8) -> Option<&Arc<Loop>> {
        self.loops.as_ref()?.get((number as usize).wrapping_sub(1))
    }

    /// The free list of the loop `number`.
    #[inline]
    fn free_of(&self, number: u8) -> *mut u32 {
        self.free[(number as usize).saturating_sub(1).min(self.free.len() - 1)].get()
    }

    /// A slot for a session on the loop `number`: the first of this host's free slots of that loop,
    /// else a new one - the registry grows. On that loop's thread only (0 - a pool of no engine).
    pub(crate) fn allocate(&self, number: u8) -> Arc<ExternalConnection> {
        let free = self.free_of(number);
        // SAFETY: this loop's list, on its thread
        unsafe {
            if *free != 0 {
                let c = self.slot(*free - 1).expect("a free slot is in the registry").clone();
                *free = c.next.swap(0, Ordering::Relaxed);
                assert!(c.activate(), "A slot on the free list was not FREE.");
                return c;
            }
        }
        self.grown(number, true)
    }

    /// A new slot on the loop `number`: the registry grows; `active` - a session takes it now, else
    /// it stays FREE for one that is restored onto it ([`restore`](Self::restore)).
    fn grown(&self, number: u8, active: bool) -> Arc<ExternalConnection> {
        let _g = self.grow.lock();
        let index = self.connections_size.load(Ordering::Relaxed);
        let (k, i) = at(index as usize);
        let mut seg = self.connections[k].load(Ordering::Relaxed);
        if seg.is_null() {
            let cells: Box<[OnceLock<Arc<ExternalConnection>>]> = (0..FIRST << k).map(|_| OnceLock::new()).collect();
            seg = Box::into_raw(cells) as *mut OnceLock<Arc<ExternalConnection>>;
            self.connections[k].store(seg, Ordering::Release);
        }
        let c = ExternalConnection::pooled(ConnId(index), self.me.clone(), self.loops.clone(), number);
        if let Some(f) = &self.on_failure {
            c.set_on_failure(f.clone());
        }
        assert!(!active || c.activate(), "a new slot is free");
        // SAFETY: the cell is this one's alone until the size counts it
        let _ = unsafe { (*seg.add(i)).set(c.clone()) };
        self.connections_size.store(index + 1, Ordering::Release); // the publication
        c
    }

    /// The slot goes back to the free list of its loop: at once on that loop, through its door from
    /// any other thread. The session on it is over - torn down, reaped, or moved to another slot -
    /// and nothing of the transport is in it any more; what the session left on it goes.
    pub(crate) fn return_to_free_pool(&self, c: &ExternalConnection) {
        match self.home(c.home) {
            Some(lp) if !lp.in_loop() => {
                let me = self.me.clone();
                let index = c.index.0;
                lp.submit_fn(Box::new(move |_| {
                    if let Some(pool) = me.upgrade() {
                        if let Some(c) = pool.slot(index) {
                            pool.free(c);
                        }
                    }
                }));
            }
            _ => self.free(c),
        }
    }

    /// Onto the free list of its loop, on that loop.
    fn free(&self, c: &ExternalConnection) {
        c.reset();
        c.conn_state.store(FREE, Ordering::Release);
        let free = self.free_of(c.home);
        // SAFETY: this loop's list, on its thread
        unsafe {
            c.next.store(*free, Ordering::Relaxed);
            *free = c.index.0 + 1;
        }
    }

    /// Claims the parked session `session_id`: taken off the parked list and returned FREE, holding
    /// neither socket nor buffers - what the session keeps (its journal, its keys) is still on it.
    /// Exactly one caller wins it against the reaper.
    pub fn parked(&self, session_id: u64) -> Option<Arc<ExternalConnection>> {
        let c = self.slot((session_id >> 32) as u32)?;
        let _g = self.parked_lock.lock();
        if c.session() != session_id || c.conn_state.compare_exchange(PARKED, FREE, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }
        self.unlink(c.index.0 + 1);
        Some(c.clone())
    }

    /// Takes the slot `me` (index + 1) off the parked list: previous and current in hand. The lock
    /// is held.
    fn unlink(&self, me: u32) {
        let next = self.slot(me - 1).map_or(0, |c| c.next.swap(0, Ordering::Relaxed));
        let head = self.parked_head.load(Ordering::Relaxed);
        if head == me {
            self.parked_head.store(next, Ordering::Relaxed);
            if next == 0 {
                self.parked_tail.store(0, Ordering::Relaxed);
            }
            return;
        }
        let mut p = head;
        while p != 0 {
            let Some(c) = self.slot(p - 1) else { return };
            let n = c.next.load(Ordering::Relaxed);
            if n == me {
                c.next.store(next, Ordering::Relaxed);
                if next == 0 {
                    self.parked_tail.store(p, Ordering::Relaxed);
                }
                return;
            }
            p = n;
        }
    }

    /// Puts a lost session on the parked list, sorted by its deadline. Most sessions of a host wait
    /// the same, so the common case lands at the tail. The slot's loop is told the deadline: it
    /// wakes for it and ends the session when the wait runs out - to the second, not at its next
    /// tick.
    pub(crate) fn park_connection(&self, c: &ExternalConnection) {
        if self.listed(c) {
            if let Some(lp) = self.home(c.home) {
                lp.reap_by(c.resumable.load(Ordering::Relaxed));
            }
        }
    }

    /// The session of `c` on the parked list, in the order of the deadlines: `false` - it is there
    /// already.
    fn listed(&self, c: &ExternalConnection) -> bool {
        let _g = self.parked_lock.lock();
        if c.conn_state.load(Ordering::Acquire) == PARKED {
            return false; // parked already: once on the list
        }
        c.conn_state.store(PARKED, Ordering::Release);
        let me = c.index.0 + 1;
        let r = c.resumable.load(Ordering::Relaxed);
        let deadline = |i: u32| self.slot(i - 1).map_or(i64::MAX, |s| s.resumable.load(Ordering::Relaxed));
        let tail = self.parked_tail.load(Ordering::Relaxed);
        if tail == 0 || deadline(tail) <= r {
            // the common case: it waits longest - behind the tail, no walk
            c.next.store(0, Ordering::Relaxed);
            if tail == 0 {
                self.parked_head.store(me, Ordering::Release);
            } else {
                self.slot(tail - 1).expect("a parked slot").next.store(me, Ordering::Relaxed);
            }
            self.parked_tail.store(me, Ordering::Relaxed);
            return true;
        }
        let head = self.parked_head.load(Ordering::Relaxed);
        if r < deadline(head) {
            c.next.store(head, Ordering::Relaxed);
            self.parked_head.store(me, Ordering::Release);
            return true;
        }
        let mut p = head; // never past the tail: the tail waits longer
        loop {
            let s = self.slot(p - 1).expect("a parked slot");
            let n = s.next.load(Ordering::Relaxed);
            if n == 0 || r < deadline(n) {
                c.next.store(n, Ordering::Relaxed);
                s.next.store(me, Ordering::Relaxed);
                return true;
            }
            p = n;
        }
    }

    /// Ends the parked sessions of the slots of the loop `on` whose wait has run out by `now`:
    /// every loop's tick asks for its own, so the close the loss withheld and the return to the
    /// free list happen on the thread every other event of the connection came from. The list is
    /// sorted by deadline: the walk ends at the first session still waiting, the others' expired
    /// are stepped over. Unlinked under the lock, ended outside it. Returns the deadline of the
    /// nearest session of the loop `on` still waiting - when it wakes for them again -, `i64::MAX`
    /// - none.
    pub(crate) fn reap_parked(&self, now: i64, on: u8) -> i64 {
        if self.parked_head.load(Ordering::Acquire) == 0 {
            return i64::MAX; // the usual tick: nothing is parked, the lock is not touched
        }
        let mut expired = 0u32;
        let mut nearest = i64::MAX;
        {
            let _g = self.parked_lock.lock();
            let mut prev = 0u32;
            let mut p = self.parked_head.load(Ordering::Relaxed);
            while p != 0 {
                let Some(c) = self.slot(p - 1) else { break };
                if now < c.resumable.load(Ordering::Relaxed) {
                    // ascending: the rest wait longer - the first of them on this loop is when it
                    // wakes again
                    let mut q = p;
                    while let Some(s) = (q != 0).then(|| self.slot(q - 1)).flatten() {
                        if s.home == on {
                            nearest = s.resumable.load(Ordering::Relaxed);
                            break;
                        }
                        q = s.next.load(Ordering::Relaxed);
                    }
                    break;
                }
                let next = c.next.load(Ordering::Relaxed);
                if c.home == on && c.conn_state.compare_exchange(PARKED, CLEANING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    if prev == 0 {
                        self.parked_head.store(next, Ordering::Relaxed);
                    } else {
                        self.slot(prev - 1).unwrap().next.store(next, Ordering::Relaxed);
                    }
                    if next == 0 {
                        self.parked_tail.store(prev, Ordering::Relaxed);
                    }
                    c.next.store(expired, Ordering::Relaxed);
                    expired = p;
                } else {
                    prev = p;
                }
                p = next;
            }
        }
        while expired != 0 {
            let c = self.slot(expired - 1).unwrap();
            expired = c.next.swap(0, Ordering::Relaxed);
            c.reap();
            self.free(c);
        }
        nearest
    }

    // The restart of the process

    /// The process ends, and the sessions of this host go on in the next one ([`restore`](Self::restore)).
    /// They live in their files as they go ( `Control` ): a process that falls leaves them there too.
    /// A clean end adds what a fall cannot: every session whose peer is identified is made to wait
    /// for it, as after a loss, what its transmitter holds goes into its journal, and its files go
    /// to the disk, closed cleanly ([`ExternalConnection::set_stop_hook`]) - the next process takes
    /// them up whatever happens to the system meanwhile. For the peers the sockets are lost: they
    /// dial again, into the next process. Waits `wait_ms` at most for the sessions to park. Not on a
    /// loop of this host; nothing new is to be taken in by then (the server's `deafen`). Returns how
    /// many sessions were closed cleanly.
    pub fn stop(&self, wait_ms: u64) -> usize {
        self.each(|c| {
            if c.conn_state.load(Ordering::Acquire) == ACTIVE && c.session() as u32 != 0 {
                c.lost(); // as a loss: the session parks, its journal goes to the disk
            }
        });
        let until = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        while std::time::Instant::now() < until && self.parking() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut waiting = Vec::new();
        {
            let _g = self.parked_lock.lock();
            let mut p = self.parked_head.load(Ordering::Relaxed);
            while let Some(c) = (p != 0).then(|| self.slot(p - 1)).flatten() {
                waiting.push(c.clone());
                p = c.next.load(Ordering::Relaxed);
            }
        }
        let mut closed = 0;
        for c in waiting {
            if c.stop_session() {
                closed += 1;
            }
        }
        closed
    }

    /// A session of this host is still on its way to park: a socket of an identified peer, or a
    /// teardown under way.
    fn parking(&self) -> bool {
        let mut on_way = false;
        self.each(|c| {
            let s = c.conn_state.load(Ordering::Acquire);
            on_way |= s == CLEANING || s == ACTIVE && c.session() as u32 != 0;
        });
        on_way
    }

    /// The sessions of hosts the process before left in `resumable::dir()` - it ended or it fell -
    /// go on in this one: each takes a slot of its own, parked for the rest of its wait ( counted
    /// from when it was parked, or from now for one that had its socket when the process fell ),
    /// with the account it had and the session `internals` makes of the connection it is of and of
    /// what it wrote of itself ( the generated `restore` ), held to that slot - `None`, none to go
    /// on with: its journal and the position of its receiver as they were. A peer that dials again
    /// reclaims it as after any loss - by the token of the slot ([`ExternalConnection::session`]),
    /// which the application learns from the slots returned. Before the host takes callers. A
    /// session whose wait ran out meanwhile, or that `internals` did not take along, is deleted.
    pub fn restore(&self, internals: &dyn Fn(u32, &mut &[u8], &Arc<ExternalConnection>) -> Option<Arc<dyn std::any::Any + Send + Sync>>) -> std::io::Result<Vec<Arc<ExternalConnection>>> {
        use crate::lib::adhoc::connection::control::Control;
        use crate::lib::adhoc::connection::transmitter::resumable::{self, expired, now};
        let mut restored = Vec::new();
        if !resumable::files() {
            return Ok(restored);
        }
        let loops = self.loops.as_ref().map_or(0, |l| l.len());
        for entry in std::fs::read_dir(resumable::dir())?.flatten() {
            let file = entry.path();
            if file.extension().is_none_or(|e| e != "head") {
                continue;
            }
            let Some(name) = file.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
            let h = Control::peek(&file);
            if h.as_deref().is_some_and(|h| !Control::header(h).3) {
                continue; // the application takes it up ( Control::open )
            }
            let Some((h, (at, len))) = h.as_deref().and_then(|b| Control::meta_in(b).map(|m| (b, m))) else {
                Control::delete(&name);
                continue;
            };
            let (kind, id, parked, _) = Control::header(h);
            let home = if loops == 0 { 0 } else { (self.connections_size() % loops) as u8 + 1 }; // the loops in turn
            let c = self.grown(home, false);
            let Some(session) = internals(kind, &mut &h[at + 8..at + 8 + len], &c) else {
                self.return_to_free_pool(&c); // the slot waits for a session of its loop
                Control::delete(&name);
                continue;
            };
            let deadline = if parked == 0 { now() } else { parked } + c.resumable_seconds(super::external_connection::Event::REMOTE_CLOSE_GRACEFUL);
            if expired(deadline) {
                drop(session); // its files are let go
                self.return_to_free_pool(&c);
                Control::delete(&name);
                continue;
            }
            c.identify(id);
            c.resumable.store(deadline, Ordering::Release);
            c.save_session(session); // the session the slot waits with
            self.park_connection(&c);
            restored.push(c);
        }
        Ok(restored)
    }

    /// How many connections of this host hold a socket right now: one read.
    #[inline]
    pub fn connections_live(&self) -> usize {
        self.connections_live.load(Ordering::Acquire)
    }

    /// How many slots this host has ever made: the high-water mark of them all - what the host
    /// costs in memory.
    #[inline]
    pub fn connections_size(&self) -> usize {
        self.connections_size.load(Ordering::Acquire) as usize
    }

    /// The slots by the state they are in, indexed by it: [`FREE`], [`ACTIVE`], [`CLEANING`],
    /// [`PARKED`]. Read off the slots when asked - a walk of the registry, nothing counted on the
    /// way of a message.
    pub fn census(&self) -> [usize; 4] {
        let mut n = [0usize; 4];
        self.each(|c| n[c.conn_state.load(Ordering::Relaxed) as usize & 3] += 1);
        n
    }

    /// The census as one line, for a log: `"free 12, active 300, cleaning 0, parked 4, slots 316"`.
    pub fn census_text(&self) -> String {
        let n = self.census();
        format!(
            "free {}, active {}, cleaning {}, parked {}, slots {}",
            n[FREE as usize],
            n[ACTIVE as usize],
            n[CLEANING as usize],
            n[PARKED as usize],
            self.connections_size()
        )
    }

    /// Every slot made.
    pub fn each(&self, mut f: impl FnMut(&Arc<ExternalConnection>)) {
        for i in 0..self.connections_size.load(Ordering::Acquire) {
            if let Some(c) = self.slot(i) {
                f(c);
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let size = *self.connections_size.get_mut();
        for i in 0..size {
            let Some(c) = self.slot(i) else { continue };
            if let Some(lp) = self.home(c.home) {
                // its place on its loop goes with the host: on that loop
                let c = c.clone();
                lp.submit_fn(Box::new(move |lp| unsafe { lp.forget(&c) }));
            }
        }
        for (k, seg) in self.connections.iter().enumerate() {
            let p = seg.load(Ordering::Acquire);
            if !p.is_null() {
                // SAFETY: made by `allocate` as a boxed slice of `FIRST << k` cells
                drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, FIRST << k)) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::network::TCP::external_connection::Event;

    #[test]
    fn the_segments_hold_every_index_once() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..100_000usize {
            let (k, j) = at(i);
            assert!(j < FIRST << k, "{i}");
            assert!(seen.insert((k, j)), "{i}");
        }
        assert_eq!(at(63), (0, 63));
        assert_eq!(at(64), (1, 0));
        assert_eq!(at(191), (1, 127));
        assert_eq!(at(192), (2, 0));
        assert!(at(u32::MAX as usize).0 < SEGMENTS);
    }

    #[test]
    fn a_slot_is_made_once_and_taken_again() {
        let pool = Pool::new("test", None, None);
        let a = pool.allocate(0);
        let b = pool.allocate(0);
        let c = pool.allocate(0);
        assert_eq!((a.index, b.index, c.index), (ConnId(0), ConnId(1), ConnId(2)));
        assert_eq!(pool.connections_live(), 3);
        assert!(Arc::ptr_eq(pool.get_connection(1).unwrap(), &b));
        for s in [&a, &b] {
            s.claim_close(Event::THIS_CLOSE_ABRUPTLY);
            pool.return_to_free_pool(s);
        }
        assert_eq!(pool.connections_live(), 1);
        assert!(pool.get_connection(1).is_none(), "a free slot is no connection");
        let d = pool.allocate(0);
        let e = pool.allocate(0);
        assert!(Arc::ptr_eq(&d, &b) && Arc::ptr_eq(&e, &a), "last returned, first taken");
        assert_eq!(pool.connections_size(), 3);
        assert_eq!(pool.census(), [0, 3, 0, 0]);
    }

    /// A close of a slot no transport took, whose close handler and session hook panic: the panics go to the handler of
    /// the connection, the slot goes back to the free list all the same.
    #[test]
    fn a_panic_in_the_close_of_a_slot_of_no_transport_still_gives_it_back() {
        let pool = Pool::new("test", None, None);
        let c = pool.allocate(0);
        let failures = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let f = failures.clone();
        c.set_on_failure(Arc::new(move |_: &dyn std::fmt::Display, e: &(dyn std::error::Error + 'static)| f.lock().unwrap().push(e.to_string())));
        c.set_event_sink(Arc::new(|_, _| panic!("the close handler")));
        c.set_session_hook(Arc::new(|open| assert!(open, "the session hook")));
        c.session_opened();
        c.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        assert_eq!(pool.census(), [1, 0, 0, 0], "the slot is free");
        let f = failures.lock().unwrap();
        assert!(f.iter().any(|s| s.contains("the close handler")), "{f:?}");
        assert!(f.iter().any(|s| s.contains("the session hook")), "{f:?}");
    }

    /// Parked sessions wait in the order of their deadlines; a returning peer claims one, the
    /// reaper ends the ones whose wait ran out, each heard once, and their slots are taken again.
    #[test]
    fn parked_sessions_are_claimed_or_reaped() {
        let pool = Pool::new("test", None, None);
        let heard = Arc::new(parking_lot::Mutex::new(Vec::<u32>::new()));
        let park = |deadline: i64| {
            let c = pool.allocate(0);
            let h = heard.clone();
            let id = c.index.0;
            c.set_event_sink(Arc::new(move |_, _| h.lock().push(id)));
            c.identify(7);
            c.claim_close(Event::REMOTE_CLOSE_ABRUPTLY);
            c.resumable.store(deadline, Ordering::Relaxed);
            pool.park_connection(&c);
            c
        };
        let a = park(30);
        let b = park(10);
        let c = park(20);
        assert_eq!(pool.census()[PARKED as usize], 3);
        assert!(pool.parked(b.session() ^ 1).is_none(), "another identity");
        let claimed = pool.parked(c.session()).expect("parked");
        assert!(Arc::ptr_eq(&claimed, &c));
        assert!(pool.parked(c.session()).is_none(), "claimed once");
        pool.reap_parked(25, 0);
        assert_eq!(*heard.lock(), vec![b.index.0], "due and not claimed");
        pool.reap_parked(25, 0);
        assert_eq!(heard.lock().len(), 1, "heard once");
        pool.reap_parked(99, 1);
        assert_eq!(heard.lock().len(), 1, "another loop's are not its own");
        pool.reap_parked(99, 0);
        assert_eq!(*heard.lock(), vec![b.index.0, a.index.0]);
        assert_eq!(pool.census(), [3, 0, 0, 0], "{}", pool.census_text()); // the claimed one is free: its taker holds it
        assert!(Arc::ptr_eq(&pool.allocate(0), &a));
    }

    /// The parked list keeps its order and its tail through parks behind the tail, in front of the
    /// head and between, claims of the last, of the head and of one between, and a reap that ends
    /// the last.
    #[test]
    fn the_parked_list_keeps_its_tail() {
        let pool = Pool::new("test", None, None);
        let park = |deadline: i64| {
            let c = pool.allocate(0);
            c.identify(7);
            c.claim_close(Event::REMOTE_CLOSE_ABRUPTLY);
            c.resumable.store(deadline, Ordering::Relaxed);
            pool.park_connection(&c);
            c
        };
        let order = |pool: &Pool| {
            let mut v = vec![];
            let mut p = pool.parked_head.load(Ordering::Relaxed);
            let mut last = 0;
            while p != 0 {
                let s = pool.slot(p - 1).unwrap();
                v.push(s.resumable.load(Ordering::Relaxed));
                last = p;
                p = s.next.load(Ordering::Relaxed);
            }
            assert_eq!(pool.parked_tail.load(Ordering::Relaxed), last, "the tail is the last of the list");
            v
        };
        let a = park(10);
        let _b = park(10); // behind its equal: the tail
        let _c = park(5); // in front of the head
        let d = park(20);
        let _e = park(15); // between
        assert_eq!(order(&pool), vec![5, 10, 10, 15, 20]);
        pool.parked(d.session()).unwrap(); // the last
        assert_eq!(order(&pool), vec![5, 10, 10, 15]);
        let _f = park(30);
        assert_eq!(order(&pool), vec![5, 10, 10, 15, 30]);
        pool.parked(a.session()).unwrap(); // one between
        assert_eq!(order(&pool), vec![5, 10, 15, 30]);
        pool.reap_parked(99, 0); // every one, the tail too
        assert_eq!(order(&pool), Vec::<i64>::new());
        let _g = park(1);
        assert_eq!(order(&pool), vec![1]);
    }
}
