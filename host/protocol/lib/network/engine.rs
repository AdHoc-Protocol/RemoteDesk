// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! TCP I/O loops, connection tasks, buffer reuse, and deadlines.
//!
//! Each loop owns one thread and poller. A connection remains assigned to one loop across sessions,
//! so its I/O and protocol state are accessed serially. Other threads submit request bits through
//! [`ExternalConnection`]; the first outstanding request enqueues the connection and wakes a
//! sleeping poller.
//!
//! Input is decoded from loop-owned buffers, and output is serialized into them. Empty buffers
//! return to the loop's cache. Readiness is edge-triggered: sockets are drained until they would
//! block. If processing stops earlier, the loop schedules another attempt without waiting for a new
//! readiness edge.

#![allow(dangerous_implicit_autorefs)] // the slots and the state of a loop are reached through raw pointers on purpose: a handler up the
// stack may hold them

use std::cell::{Cell, UnsafeCell};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};

use super::buf::{Buf, RxBuf};
use super::TCP::external_connection::Event;
use crate::lib::adhoc::{BytesDst, BytesSrc};
use super::TCP::external_connection::{ExternalConnection, TimeoutAnswer, TimeoutKind};
use super::TCP::web_socket::Codec;
use crate::lib::chain::{Link, HEADERS, RECORD_BUFFER};

/// The smallest buffer of the transport: what a frame header, a control frame and a byte of payload
/// need.
pub const MIN_BUFFER: usize = 128;

/// How many times in a row one connection reads, or writes, before it steps aside for the others.
const BURST: usize = 16;
/// Linux epoll, edge-triggered (mio): a read shorter than asked emptied the socket, a write shorter
/// than given filled it (the kernel marks it to signal the room) - the next edge says when there is
/// more, no call runs into "would block" (Netty). A FIN that came with the readiness is read to the
/// end. Elsewhere the call that would block says it.
const EDGE_SHORT: bool = cfg!(any(target_os = "linux", target_os = "android"));
/// The buffers a loop keeps at most on its stack: beyond, a buffer given back is let go.
const BUFFERS_MAX: usize = 1 << 14;
/// How deep the loop goes in "a write finished, which produced another write" before the rest waits
/// for the next pass.
const INLINE_DEPTH: u32 = 16;
/// The deadlines are filed by beats of 2^24 ns (16.8 ms): a busy connection renews its wait without
/// touching the list.
const BEAT: i64 = 1 << 24;
/// Connections accepted on one readiness before the loop serves the others.
const ACCEPT_BURST: usize = 64;

const WAKER: Token = Token(usize::MAX);
/// The tokens of listeners: from here up.
const ACCEPTORS: usize = 1 << (usize::BITS - 8); // far above the slots, on 32 bits too
const NONE: u32 = u32::MAX;
/// The header of a record of TLS, before the window of a transmit.
const TLS_ROOM: usize = 5;
/// All a record of TLS adds: the header, the type byte and the tag.
const TLS_TAIL: usize = 22;

// the kinds of the tasks of a connection: the low byte of a task
pub(crate) const WAKE: u64 = 1;
pub(crate) const DRAIN: u64 = 2;
pub(crate) const CLOSE: u64 = 3;
pub(crate) const GRACE: u64 = 4;
pub(crate) const DEADLINE: u64 = 5;
const READ: u64 = 6;
const RUN: u64 = 7;
const REAP: u64 = 8;
/// A connect or a reconnect carried by the connection itself (its `dial`): no closure for it.
pub(crate) const CONNECT: u64 = 9;

const R: usize = 0; // the wait for data
const T: usize = 1; // the wait for the socket to take a parked write
const A: usize = 2; // the nearest deadline of the actor instances

/// The deadline of a parked session, in unix seconds, as a time of the loops' clock ([`clock`]).
#[inline]
fn reap_at(deadline: i64) -> i64 {
    (deadline.saturating_mul(1000) - super::TCP::external_connection::clock_base().1).max(0).saturating_mul(1_000_000)
}

/// This thread runs a loop of some engine: it may not wait for the loops.
pub(crate) fn on_a_loop() -> bool {
    CURRENT.with(|c| !c.get().is_null())
}

/// Monotonic nanoseconds relative to the same base used by
/// [`now_ms`](super::TCP::external_connection::now_ms), the sleep of the machine taken in
/// ([`slept`]).
#[inline]
pub(crate) fn clock() -> i64 {
    let s = SLEPT.load(Ordering::Relaxed);
    let t = super::TCP::external_connection::clock_base().0.elapsed().as_nanos() as i64 + s;
    if t < LOOK_AT.load(Ordering::Relaxed) { t } else { slept(t, s) }
}

/// The sleep of the machine the clock has taken in, nanoseconds ([`slept`]).
static SLEPT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// When, by the [`clock`], it compares itself with the wall clock next.
static LOOK_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// How often the clock looks whether the machine slept, nanoseconds.
const LOOK_NS: i64 = 1_000_000_000;
/// The wall clock ran ahead of the clock by this much at least: the machine slept, nanoseconds.
const SLEEP_NS: i64 = 2_000_000_000;

/// The monotonic clock stands while the machine sleeps (Linux, macOS): woken, the deadlines would
/// see no time passed - a line dead for an hour is found only once its receive timeout runs out
/// again, the session the peer kept for it long gone. So once a second the clock compares itself
/// with the wall clock and takes in how far the wall clock ran ahead of it - the sleep: what ran
/// out in it is due at once, a line silent longer than its timeout ends and its client dials. A
/// step of the wall clock forward counts as a sleep; one back counts nothing - the clock never
/// goes back. `t` - the clock with `s`, the sleep taken in so far.
#[cold]
fn slept(t: i64, s: i64) -> i64 {
    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64);
    let ahead = wall - (super::TCP::external_connection::clock_base().1 * 1_000_000 + t);
    if SLEEP_NS <= ahead {
        let _ = SLEPT.compare_exchange(s, s + ahead, Ordering::Relaxed, Ordering::Relaxed); // threads that saw one sleep take it in once
    }
    let t = t - s + SLEPT.load(Ordering::Relaxed);
    LOOK_AT.store(t + LOOK_NS, Ordering::Relaxed);
    t
}

/// The machine slept `ns`, as [`slept`] takes it in: the clock of the whole process moves.
#[cfg(test)]
pub(crate) fn sleep_for_test(ns: i64) {
    SLEPT.fetch_add(ns, Ordering::Relaxed);
}

/// The loops' clock now, nanoseconds: on a loop thread the reading of this pass ( one a pass ), elsewhere a
/// fresh one - for the measures that do not need it finer than a pass, read at every read of a connection.
#[inline]
pub(crate) fn now_nanos() -> i64 {
    CURRENT.with(|c| {
        let l = c.get();
        // SAFETY: a loop is current only on its own thread, for as long as it runs
        if l.is_null() { clock() } else { unsafe { (*l).nanos() } }
    })
}

thread_local! {
    /// The loop this thread runs, null elsewhere.
    static CURRENT: Cell<*const Loop> = const { Cell::new(std::ptr::null()) };
}

// Engine

/// The engine of the process ([`Engine::shared`]).
static SHARED: OnceLock<Arc<Engine>> = OnceLock::new();

/// The threads the connections of hosts run on: loops, each one thread with one poller. New
/// connections are handed out round-robin. One engine serves any number of hosts; the size of the
/// buffers is the engine's.
pub struct Engine {
    /// The loops, numbered 1 and up in this order ([`Loop::number`]): a connection names its loop
    /// by the number.
    pub(crate) loops: Arc<[Arc<Loop>]>,
    next: AtomicUsize,
    /// The size of the buffers the loops read into and write from.
    pub buffer_size: usize,
    /// The parked sessions of the hosts on this engine, reaped by its first loop every tick.
    ticks: Arc<Ticks>,
    /// The connections of the clients of this engine: their slots, the sessions that lost their
    /// connection and wait for a reconnect.
    pub(crate) clients: Arc<super::TCP::Pool>,
    /// Where the failures of its loops and of its clients go.
    on_failure: Arc<std::sync::OnceLock<super::failure::OnFailure>>,
}

/// What each loop of an engine does every tick: the parked sessions of its connections whose wait
/// ran out get their withheld close event there.
pub(crate) struct Ticks {
    /// How often, in milliseconds.
    tick_ms: std::sync::atomic::AtomicI64,
    /// How many times a loop with nothing to do polls before it blocks.
    spin: std::sync::atomic::AtomicU32,
    lists: parking_lot::Mutex<Vec<std::sync::Weak<super::TCP::Pool>>>,
}

impl Ticks {
    /// Reaps expired parked sessions assigned to loop `home`, invoking cleanup outside the lock.
    /// `now_ms` is converted to the seconds used by parked deadlines. Removes departed hosts and
    /// returns the nearest remaining deadline, or `i64::MAX` when none remain.
    fn reap(&self, now_ms: i64, home: u8) -> i64 {
        let pools: Vec<Arc<super::TCP::Pool>> = {
            let mut l = self.lists.lock();
            l.retain(|w| w.strong_count() != 0);
            l.iter().filter_map(|w| w.upgrade()).collect()
        };
        pools.iter().map(|pool| pool.reap_parked(now_ms / 1000, home)).min().unwrap_or(i64::MAX)
    }
}

impl Engine {
    /// `threads` loops - at least 1, and never more than the processors: a loop beyond them adds no
    /// work done, only one more thread to be switched out, and a loop that is switched out stalls
    /// every connection on it. `buffer_size` - at least [`MIN_BUFFER`].
    pub fn new(name: &str, threads: usize, buffer_size: usize) -> Arc<Self> {
        let buffer_size = buffer_size.max(MIN_BUFFER);
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let n = threads.clamp(1, cores.min(255)); // a connection names its loop in a byte
        let ticks = Arc::new(Ticks { tick_ms: std::sync::atomic::AtomicI64::new(5000), spin: std::sync::atomic::AtomicU32::new(0), lists: parking_lot::Mutex::new(Vec::new()) });
        let on_failure = Arc::new(std::sync::OnceLock::new());
        let loops: Arc<[Arc<Loop>]> = (0..n).map(|i| Loop::start(format!("{name}-io-{i}"), i as u8 + 1, buffer_size, ticks.clone(), on_failure.clone())).collect();
        for l in loops.iter() {
            let _ = l.all.set(Arc::downgrade(&loops));
        }
        let clients = super::TCP::Pool::new(format!("{name} clients"), Some(loops.clone()), None);
        ticks.lists.lock().push(Arc::downgrade(&clients));
        Arc::new(Self { loops, next: AtomicUsize::new(0), buffer_size, ticks, clients, on_failure })
    }

    /// Where the failures of the loops of this engine and of its clients go;
    /// [`super::failure::print_console`] until it is set. Once, before the first connection: a
    /// connection takes the handler it is made with.
    pub fn set_on_failure(&self, f: super::failure::OnFailure) {
        let _ = self.on_failure.set(f);
    }

    /// The handler of this engine.
    pub(crate) fn on_failure(&self) -> super::failure::OnFailure {
        self.on_failure.get().cloned().unwrap_or_else(super::failure::print_console)
    }

    /// A slot for a new client connection on the loop `lp`, of the clients of this engine, with the
    /// handler of the engine. On that loop's thread.
    pub(crate) fn client_state(&self, lp: &Loop) -> Arc<super::TCP::external_connection::ExternalConnection> {
        let state = self.clients.allocate(lp.number);
        state.set_on_failure(self.on_failure()); // once: the slot keeps the one it was first given
        state
    }

    /// How often the first loop reaps the parked sessions of the hosts, in milliseconds: from the
    /// next tick on.
    pub fn set_tick(&self, ms: i64) {
        self.ticks.tick_ms.store(ms.max(1), Ordering::Relaxed);
    }

    /// How many times a loop with nothing to do polls its sockets before it blocks: 0, the default,
    /// blocks at once. A loop that polls is never asleep, so nothing has to wake it - a core kept
    /// busy, traded for the latency of a wakeup, for a link where that latency is what matters.
    pub fn set_spin(&self, polls: u32) {
        self.ticks.spin.store(polls, Ordering::Relaxed);
    }

    /// Opens a socket to `addr` and connects it on a loop of this engine, in a slot of its clients,
    /// with the protocol layer `seat` gives seated first. `on_connected` hears on that loop's
    /// thread: the connection once its session started, or what went wrong - `TimedOut` when
    /// `timeout` ran out first. As [`super::TCP::client::connect_with`].
    pub fn connect(
        self: &Arc<Self>,
        addr: SocketAddr,
        timeout: Option<std::time::Duration>,
        seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
        on_connected: impl FnOnce(io::Result<Arc<ExternalConnection>>) + Send + 'static,
    ) {
        super::TCP::client::connect_with(addr, timeout, self, seat, on_connected)
    }

    /// Runs `f` on one of the threads of this engine.
    pub fn execute(&self, f: impl FnOnce() + Send + 'static) {
        self.pick().submit_fn(Box::new(move |_| f()));
    }

    /// The parked sessions of a host on this engine: reaped every tick, for as long as the host is
    /// there.
    pub(crate) fn watch_parked(&self, pool: &Arc<super::TCP::Pool>) {
        self.ticks.lists.lock().push(Arc::downgrade(pool));
    }

    /// The engine of the process: a loop per core, buffers of [`super::buf::BUFFER_SIZE`] - unless
    /// [`configure_shared`](Self::configure_shared) set it otherwise first.
    pub fn shared() -> Arc<Engine> {
        SHARED.get_or_init(|| Engine::new("adhoc", usize::MAX, super::buf::BUFFER_SIZE)).clone()
    }

    /// Makes the engine of the process ([`shared`](Self::shared)) with `threads` loops and buffers
    /// of `buffer_size` bytes, as [`new`](Self::new) takes them: before the first connection that
    /// uses it. `false` - it is made already (by a connection, or by an earlier call): nothing
    /// changes.
    pub fn configure_shared(threads: usize, buffer_size: usize) -> bool {
        let mut made = false;
        SHARED.get_or_init(|| {
            made = true;
            Engine::new("adhoc", threads, buffer_size)
        });
        made
    }

    pub fn threads(&self) -> usize {
        self.loops.len()
    }

    /// Round-robin. Two threads that race here at worst hand two connections to one loop: nothing
    /// is worth a `lock` prefix.
    pub(crate) fn pick(&self) -> &Arc<Loop> {
        let i = self.next.load(Ordering::Relaxed);
        self.next.store(if i + 1 >= self.loops.len() { 0 } else { i + 1 }, Ordering::Relaxed);
        &self.loops[i.min(self.loops.len() - 1)]
    }

    /// A leak check for tests and monitoring: how many transmit buffers are out of the loops'
    /// stacks - in a connection's hand. With no traffic it is zero; if it is not, something kept a
    /// buffer. The receive buffers are the connections' own ([`RxBuf`]), not counted. Summed over
    /// the loops - relays carry buffers from one loop to another - as they last wrote it: from any
    /// thread, a stopped loop too, waiting for none; exact only at rest.
    pub fn buffers_held(&self) -> usize {
        // a buffer taken on one loop and given back on another leaves one counter up and the other
        // down: the sum is right
        self.loops.iter().fold(0usize, |n, l| n.wrapping_add(l.held.load(Ordering::Relaxed)))
    }

    /// How often the first loop reaps the parked sessions of the hosts, in milliseconds.
    pub fn tick(&self) -> i64 {
        self.ticks.tick_ms.load(Ordering::Relaxed)
    }

    /// How many times a loop with nothing to do polls before it blocks.
    pub fn spin(&self) -> u32 {
        self.ticks.spin.load(Ordering::Relaxed)
    }

    /// The engine of the process ([`shared`](Self::shared)): no host ends it.
    pub fn is_shared(self: &Arc<Self>) -> bool {
        SHARED.get().is_some_and(|s| Arc::ptr_eq(s, self))
    }

    /// Stops the threads; what is left open on them closes.
    pub fn close(&self) {
        for l in self.loops.iter() {
            l.shutdown();
        }
    }

    /// Starts listening on `addr` for `host`: every accepted socket goes to [`Accept::accepted`] on
    /// the loop it will live on.
    pub fn listen(self: &Arc<Self>, addr: SocketAddr, backlog: i32, recv_buffer: Option<usize>, host: Arc<dyn Accept>) -> io::Result<Listening> {
        let socket = socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
        #[cfg(not(windows))]
        socket.set_reuse_address(true)?;
        if let Some(size) = recv_buffer {
            let _ = socket.set_recv_buffer_size(size); // before bind: the window scaling of the handshake, inherited by every accepted socket
        }
        socket.bind(&addr.into())?;
        socket.listen(if backlog < 1 { 50 } else { backlog })?; // Use a backlog of 50 when the caller supplies a value below one.
        socket.set_nonblocking(true)?;
        let std: std::net::TcpListener = socket.into();
        let local = std.local_addr()?;
        let listener = TcpListener::from_std(std);
        let lp = self.pick().clone();
        let token = Arc::new(AtomicUsize::new(usize::MAX));
        let closed = Arc::new(AtomicBool::new(false));
        let (t, c) = (token.clone(), closed.clone());
        let engine = self.clone();
        lp.submit_fn(Box::new(move |lp: &Loop| unsafe { lp.add_acceptor(listener, host, engine, &t, c) }));
        Ok(Listening { lp, token, closed, local })
    }

}

/// A host that listens: what it does with a socket accepted for it.
pub trait Accept: Send + Sync {
    /// The admission of a peer (limits, rates): `None` - the socket closes at once; `Some(counted)`
    /// - taken, and whether its address was counted (given back when the connection goes).
    fn admit(&self, peer: SocketAddr) -> Option<bool>;
    /// A socket accepted for this host, on the loop it will live on.
    fn accepted(self: Arc<Self>, lp: &Loop, stream: TcpStream, peer: SocketAddr, counted: bool);
    /// A failure of the host itself, to its handler.
    fn failure(&self, e: &(dyn std::error::Error + 'static));
    /// How long the listener stays deaf after accept failed.
    fn accept_retry(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }
}

/// A listening socket: closing it stops the accepts, what was accepted lives on.
pub struct Listening {
    lp: Arc<Loop>,
    token: Arc<AtomicUsize>,
    /// Raised by [`close`](Self::close): the listener's loop takes nothing in after that - what it
    /// accepts closes at once.
    closed: Arc<AtomicBool>,
    local: SocketAddr,
}

impl Listening {
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// From any thread, without waiting for the loop: once it returns no socket is taken in - one
    /// the loop accepts meanwhile closes at once; the listening socket itself closes on the loop's
    /// next pass.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let t = self.token.clone();
        self.lp.submit_fn(Box::new(move |lp: &Loop| unsafe { lp.drop_acceptor(t.load(Ordering::Acquire)) }));
    }
}

// Loop

/// One poller, one thread, and the connections that belong to it. Each pass: poll, serve what is
/// ready, run the tasks that piled up, fail whatever timed out. Everything a loop owns sits in
/// [`St`], touched by its thread alone; the one door in from other threads is the inbox.
pub struct Loop {
    waker: Waker,
    /// The inbox: a Treiber stack of the connections other threads asked something of, threaded
    /// through their own `next_task`. A connection is on it once at most - whoever raises its
    /// requests from zero pushes it
    /// - so it has no size, allocates nothing and cannot fill. It holds a strong count of each
    ///   connection on it.
    inbox: std::sync::atomic::AtomicPtr<super::TCP::external_connection::ExternalConnection>,
    /// The handoffs of the listeners with sockets for this loop ([`Handoff`]): a Treiber stack
    /// through their own `next`, each on it once for any number of sockets, with a strong count.
    handoffs: std::sync::atomic::AtomicPtr<Handoff>,
    /// Rare work from other threads - a socket to adopt, a listener to start: boxed, under a lock,
    /// never per message.
    fns: parking_lot::Mutex<Vec<Box<dyn FnOnce(&Loop) + Send>>>,
    has_fns: AtomicBool,
    /// Raised right before the loop blocks in the poller, and only then: a producer wakes the
    /// poller only when it sees it.
    asleep: AtomicBool,
    running: AtomicBool,
    /// Transmit buffers out of the loop's stack - in a connection's hand: written by the loop
    /// alone, plain writes, read by [`Engine::buffers_held`] from anywhere.
    held: crossbeam_utils::CachePadded<AtomicUsize>, // a line of its own: the loop writes it on every buffer, the producers write `inbox` / `asleep`
    pub buffer_size: usize,
    /// Its number among the loops of its engine, 1 and up: a connection names its loop by it.
    pub(crate) number: u8,
    /// The loops of its engine.
    all: OnceLock<std::sync::Weak<[Arc<Loop>]>>,
    /// What it does every tick: reap the parked sessions of its connections.
    ticks: Option<Arc<Ticks>>,
    /// The handler of its engine: the failures of the loop itself.
    on_failure: Arc<std::sync::OnceLock<super::failure::OnFailure>>,
    /// Lines of its own: rustc orders the fields as it likes, and the producers write `inbox` /
    /// `fns` / `has_fns` and read `asleep` on every request.
    st: crossbeam_utils::CachePadded<UnsafeCell<St>>,
}

unsafe impl Send for Loop {}
unsafe impl Sync for Loop {}

/// What a loop owns: its thread alone touches it.
struct St {
    poll: Poll,
    events: Option<Events>,
    local: Vec<Local>,
    spare: Vec<Local>,
    slots: Vec<*mut Slot>,
    /// The loop's buffers: a stack, so the one given back last - still hot - is the one taken next.
    /// A connection takes one for a write and gives it back once it is out: one at rest holds none.
    bufs: Vec<Buf>,
    /// Whole records of a chain: only while one is on its way and does not fit the buffers above,
    /// and for the handshake of TLS. No connection keeps one.
    records: Vec<Buf>,
    /// What a connection with a chain needs, between connections. A connection with none takes
    /// nothing.
    links: Vec<Box<Link>>,
    gens: Vec<u32>,
    free: Vec<u32>,
    acceptors: Vec<Option<Box<Acceptor>>>,
    heads: [u32; 3],
    tails: [u32; 3],
    fingers: [u32; 3],
    nanos: i64,
    millis: i64,
    stale: bool,
    depth: u32,
    /// The ready sockets are being served: what is queued for sending meanwhile goes out after
    /// them, once a connection.
    serving: bool,
    /// When the next tick is due, in the loop's nanoseconds (the first loop of an engine).
    next_tick: i64,
    /// When the nearest parked session of this loop runs out, in the loop's nanoseconds: the loop
    /// wakes for it. `i64::MAX` - none.
    next_reap: i64,
    /// The parked sessions of the clients of this loop that dial again, and when, in the loop's
    /// nanoseconds ( `TCP::Redial` ): the loop wakes for them and the client dials on it.
    redials: Vec<(i64, std::sync::Weak<super::TCP::client::Core>)>,
}

enum Local {
    Task(u64),
    Fn(Box<dyn FnOnce(&Loop)>),
    /// The listener of this token accepted a full burst: it goes on with the backlog on the next
    /// pass (no closure for it).
    Accept(usize),
}

impl Loop {
    fn start(name: String, number: u8, buffer_size: usize, ticks: Arc<Ticks>, on_failure: Arc<std::sync::OnceLock<super::failure::OnFailure>>) -> Arc<Loop> {
        let poll = Poll::new().expect("the poller of a loop");
        let waker = Waker::new(poll.registry(), WAKER).expect("the waker of a loop");
        let lp = Arc::new(Loop {
            waker,
            inbox: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            handoffs: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            fns: parking_lot::Mutex::new(Vec::new()),
            has_fns: AtomicBool::new(false),
            asleep: AtomicBool::new(false),
            running: AtomicBool::new(true),
            held: crossbeam_utils::CachePadded::new(AtomicUsize::new(0)),
            buffer_size,
            number,
            all: OnceLock::new(),
            ticks: Some(ticks),
            on_failure,
            st: crossbeam_utils::CachePadded::new(UnsafeCell::new(St {
                poll,
                events: Some(Events::with_capacity(1024)),
                local: Vec::with_capacity(64),
                spare: Vec::with_capacity(64),
                slots: Vec::new(),
                bufs: Vec::new(),
                records: Vec::new(),
                links: Vec::new(),
                gens: Vec::new(),
                free: Vec::new(),
                acceptors: Vec::new(),
                heads: [NONE; 3],
                tails: [NONE; 3],
                fingers: [NONE; 3],
                nanos: 0,
                millis: 0,
                stale: true,
                depth: 0,
                serving: false,
                next_tick: 0,
                next_reap: i64::MAX,
                redials: Vec::new(),
            })),
        });
        let run = lp.clone();
        std::thread::Builder::new().name(name).spawn(move || run.run()).expect("the thread of a loop");
        lp
    }

    #[inline]
    fn st(&self) -> *mut St {
        self.st.get()
    }

    /// This thread runs this loop.
    #[inline]
    /// A parked session of this loop waits until `deadline` (unix seconds): the loop wakes for it.
    /// From any thread.
    pub(crate) fn reap_by(&self, deadline: i64) {
        let at = reap_at(deadline);
        if self.in_loop() {
            unsafe {
                let st = self.st();
                if at < (*st).next_reap {
                    (*st).next_reap = at;
                }
            }
        } else {
            self.submit_fn(Box::new(move |lp: &Loop| lp.reap_by(deadline)));
        }
    }

    /// The parked session of the client `core` dials again `ms` from now ( `TCP::Redial` ): the loop
    /// wakes for it, and the client dials on it. From any thread.
    pub(crate) fn redial_in(&self, ms: i64, core: std::sync::Weak<super::TCP::client::Core>) {
        if self.in_loop() {
            unsafe {
                let at = self.nanos() + ms * 1_000_000;
                (*self.st()).redials.push((at, core));
            }
        } else {
            self.submit_fn(Box::new(move |lp: &Loop| lp.redial_in(ms, core)));
        }
    }

    pub fn in_loop(&self) -> bool {
        CURRENT.with(|c| c.get() == self as *const Loop)
    }

    #[inline]
    fn nanos(&self) -> i64 {
        unsafe {
            let st = self.st();
            if (*st).stale {
                self.read_clock();
            }
            (*st).nanos
        }
    }

    /// The deadline `after` from now, in the loops' nanoseconds - for a wait armed later on a loop.
    pub(crate) fn deadline_of(after: std::time::Duration) -> i64 {
        clock() + after.as_nanos().min(i64::MAX as u128 / 2) as i64
    }

    #[inline]
    fn millis(&self) -> i64 {
        unsafe {
            let st = self.st();
            if (*st).stale {
                self.read_clock();
            }
            (*st).millis
        }
    }

    unsafe fn read_clock(&self) {
        let st = self.st();
        (*st).nanos = clock();
        (*st).millis = super::TCP::external_connection::clock_base().1 + (*st).nanos / 1_000_000; // one read of the clock a pass
        (*st).stale = false;
    }

    // tasks

    /// A connection onto the inbox, from any thread: one CAS - and a wakeup only when the loop is
    /// asleep. `e` carries a strong count the loop takes over, and is on no inbox.
    pub(crate) fn push(&self, e: *const super::TCP::external_connection::ExternalConnection) {
        let e = e as *mut super::TCP::external_connection::ExternalConnection;
        let mut h = self.inbox.load(Ordering::Relaxed);
        loop {
            // SAFETY: `e` is alive - the count it carries - and on no inbox: its link is this
            // pusher's alone
            unsafe { (*e).asks.next_task.store(h, Ordering::Relaxed) };
            match self.inbox.compare_exchange_weak(h, e, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) => h = now,
            }
        }
        if self.asleep.load(Ordering::SeqCst) && self.asleep.swap(false, Ordering::AcqRel) {
            let _ = self.waker.wake(); // the one that flips the word wakes: the others find it awake
        }
    }

    /// A handoff with sockets onto the stack of handoffs, from the listener's loop: one CAS. `h`
    /// carries a strong count the loop takes over, and is on no stack.
    fn push_handoff(&self, h: *const Handoff) {
        let h = h as *mut Handoff;
        let mut top = self.handoffs.load(Ordering::Relaxed);
        loop {
            // SAFETY: `h` is alive - the count it carries - and on no stack: its link is this
            // pusher's alone
            unsafe { (*h).next.store(top, Ordering::Relaxed) };
            match self.handoffs.compare_exchange_weak(top, h, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) => top = now,
            }
        }
        if self.asleep.load(Ordering::SeqCst) && self.asleep.swap(false, Ordering::AcqRel) {
            let _ = self.waker.wake();
        }
    }

    /// Rare work for this loop, from any thread.
    pub(crate) fn submit_fn(&self, f: Box<dyn FnOnce(&Loop) + Send>) {
        if self.in_loop() {
            unsafe { (*self.st()).local.push(Local::Fn(f)) };
            return;
        }
        self.fns.lock().push(f);
        self.has_fns.store(true, Ordering::SeqCst);
        if !self.running.load(Ordering::SeqCst) {
            // stopped - maybe after its last look at them: what nobody will run goes now, with
            // whatever waits on it
            let gone = std::mem::take(&mut *self.fns.lock()); // out of the lock first: a drop may submit again
            drop(gone);
            return;
        }
        if self.asleep.load(Ordering::SeqCst) && self.asleep.swap(false, Ordering::AcqRel) {
            let _ = self.waker.wake();
        }
    }

    /// This thread only: the task runs on the next pass.
    #[inline]
    pub(crate) unsafe fn later(&self, task: u64) {
        (*self.st()).local.push(Local::Task(task));
    }

    fn run(self: Arc<Self>) {
        CURRENT.with(|c| c.set(Arc::as_ptr(&self)));
        let st = self.st();
        while self.running.load(Ordering::Relaxed) {
            unsafe {
                let mut idle = (*st).local.is_empty() && self.inbox.load(Ordering::Acquire).is_null() && self.handoffs.load(Ordering::Acquire).is_null() && !self.has_fns.load(Ordering::Acquire);
                let mut events = (*st).events.take().unwrap();
                if idle && self.spins(&mut events) {
                    idle = false; // the polls found something: no sleep
                }
                let r = if !idle && !events.is_empty() {
                    Ok(())
                } else {
                    if idle {
                        self.asleep.store(true, Ordering::SeqCst); // written first...
                        idle = self.inbox.load(Ordering::SeqCst).is_null() && self.handoffs.load(Ordering::SeqCst).is_null() && !self.has_fns.load(Ordering::SeqCst); // ...read second
                    }
                    let wait = if idle { Some(self.next_deadline()) } else { Some(std::time::Duration::ZERO) };
                    (*st).stale = true;
                    (*st).poll.poll(&mut events, wait)
                };
                if self.asleep.load(Ordering::Relaxed) {
                    self.asleep.store(false, Ordering::Relaxed); // only when it slept: a pass that did not leaves the line the producers read alone
                }
                if let Err(e) = r {
                    if e.kind() != io::ErrorKind::Interrupted {
                        self.failure(&e); // Report failure to rebuild the poller.
                    }
                }
                (*st).serving = true;
                for ev in events.iter() {
                    self.ready(ev);
                }
                (*st).serving = false;
                (*st).events = Some(events);
                self.run_tasks();
                self.expire();
            }
        }
        // the loop ends: its sockets and its listeners close now, not with the process - a port is
        // free again, the peers see the close; its slots go too - a slot holds its connection and
        // the connection its loops: kept, the two keep each other and all they hold for good. Nobody
        // reaches a slot but its loop, through the inboxes no one serves any more.
        unsafe {
            for i in 0..(*st).slots.len() {
                let p = (*st).slots[i];
                if p.is_null() {
                    continue;
                }
                if let Some(s) = (*p).stream.take() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
                if !(*p).link.is_null() {
                    (*(*p).link).close(p);
                    drop(Box::from_raw(std::mem::replace(&mut (*p).link, std::ptr::null_mut())));
                }
                if !(*p).rb.is_null() {
                    drop(Box::from_raw(std::mem::replace(&mut (*p).rb, std::ptr::null_mut())));
                }
                for k in 0..3 {
                    if (*p).dl[k].linked {
                        self.unfile(p, k);
                    }
                }
                self.free_slot(p);
            }
            for a in (*st).acceptors.iter_mut() {
                drop(a.take());
            }
            let mut e = self.inbox.swap(std::ptr::null_mut(), Ordering::Acquire);
            while !e.is_null() {
                let next = (*e).asks.next_task.load(Ordering::Relaxed);
                drop(Arc::from_raw(e)); // the count the push gave: nobody serves it any more
                e = next;
            }
            let mut h = self.handoffs.swap(std::ptr::null_mut(), Ordering::Acquire);
            while !h.is_null() {
                let next = (*h).next.load(Ordering::Relaxed);
                drop(Arc::from_raw(h)); // its sockets close with it
                h = next;
            }
            // the work nobody will run: whoever waits on it hears it is gone (a sender dropped)
            // instead of waiting forever
            (*st).local.clear();
            let gone = std::mem::take(&mut *self.fns.lock()); // out of the lock first: a drop may submit again
            drop(gone);
        }
    }

    /// Polls instead of blocking, [`Engine::set_spin`] times at most: true - a socket is ready or
    /// work came, the loop does not sleep; `events` holds what the last poll found.
    unsafe fn spins(&self, events: &mut Events) -> bool {
        let n = self.ticks.as_ref().map_or(0, |t| t.spin.load(Ordering::Relaxed));
        let st = self.st();
        for _ in 0..n {
            (*st).stale = true;
            if (*st).poll.poll(events, Some(std::time::Duration::ZERO)).is_ok() && !events.is_empty() {
                return true;
            }
            if !self.inbox.load(Ordering::Acquire).is_null() || !self.handoffs.load(Ordering::Acquire).is_null() || self.has_fns.load(Ordering::Acquire) {
                return true;
            }
            std::hint::spin_loop();
        }
        false
    }

    /// Runs what the inboxes hold right now. A task that adds another leaves it for the next pass.
    unsafe fn run_tasks(&self) {
        if !self.inbox.load(Ordering::Relaxed).is_null() {
            // the swap only when there is something to take
            let mut e = self.inbox.swap(std::ptr::null_mut(), Ordering::Acquire);
            while !e.is_null() {
                let next = (*e).asks.next_task.load(Ordering::Relaxed); // first: served, it may be pushed again
                // SAFETY: the push gave the inbox a strong count of it: taken over here, dropped
                // once served
                let ext = Arc::from_raw(e);
                let what = ext.asks.requests.swap(0, Ordering::AcqRel); // from here a request pushes it again
                if what & 1 << CONNECT != 0 {
                    // before the slot: the connection has none yet (a connect), or the one it had
                    // is going (a reconnect)
                    let pending = ext.dial.lock().take();
                    if let Some(p) = pending {
                        self.quietly(|| super::TCP::pending(self, &ext, p));
                    }
                }
                self.serve(&ext, what);
                e = next;
            }
        }
        if !self.handoffs.load(Ordering::Relaxed).is_null() {
            let mut h = self.handoffs.swap(std::ptr::null_mut(), Ordering::Acquire);
            while !h.is_null() {
                let next = (*h).next.load(Ordering::Relaxed); // first: taken, it may be pushed again
                // SAFETY: the push gave the stack a strong count of it: taken over here, dropped
                // once its sockets are taken
                let handoff = Arc::from_raw(h);
                handoff.take(self);
                h = next;
            }
        }
        if self.has_fns.load(Ordering::Acquire) {
            self.has_fns.store(false, Ordering::SeqCst);
            let fs = std::mem::take(&mut *self.fns.lock());
            for f in fs {
                self.quietly(|| f(self));
            }
        }
        let st = self.st();
        if !(*st).local.is_empty() {
            let mut batch = std::mem::take(&mut (*st).spare);
            std::mem::swap(&mut batch, &mut (*st).local);
            for l in batch.drain(..) {
                match l {
                    Local::Task(t) => self.task(t),
                    Local::Fn(f) => self.quietly(|| f(self)),
                    Local::Accept(token) => {
                        if let Some(Some(a)) = (*self.st()).acceptors.get_mut(token - ACCEPTORS) {
                            let a: *mut Acceptor = &mut **a;
                            self.accept(a);
                        }
                    }
                }
            }
            (*st).spare = batch;
        }
    }

    /// What other threads asked of the connection `ext` (its `requests`, bits `1 << kind`): on the
    /// slot of its socket, as the local task of each kind runs. No socket any more - nothing to do.
    unsafe fn serve(&self, ext: &Arc<super::TCP::external_connection::ExternalConnection>, what: u32) {
        let Some((_, key)) = ext.link() else { return };
        let p = self.slot(key);
        if p.is_null() {
            return;
        }
        guarded(p, || {
            for kind in [CLOSE, GRACE, DEADLINE, DRAIN, WAKE] {
                if what & 1 << kind != 0 && !(*p).dead {
                    self.task_of(p, key | kind);
                }
            }
        });
    }

    /// The slot a task names, when it still holds the session the task was for.
    #[inline]
    unsafe fn slot(&self, task: u64) -> *mut Slot {
        let idx = (task >> 32) as usize;
        let st = self.st();
        match (*st).slots.get(idx) {
            Some(&p) if !p.is_null() && (*p).gen == ((task >> 8) & 0xFF_FFFF) as u32 => p,
            _ => std::ptr::null_mut(),
        }
    }

    unsafe fn task(&self, task: u64) {
        let p = self.slot(task);
        if p.is_null() {
            return;
        }
        if task & 0xFF == REAP {
            return self.task_of(p, task); // the slot goes: nothing of the connection runs any more
        }
        guarded(p, || self.task_of(p, task));
    }

    unsafe fn task_of(&self, p: *mut Slot, task: u64) {
        if (*p).connecting {
            // the socket is not connected yet: nothing goes in or out of it - what was asked for
            // runs when the session starts
            match task & 0xFF {
                DEADLINE => arm_deadline_from_state(p),
                CLOSE | GRACE => close_on_loop(p),
                REAP => reap(p),
                _ => {}
            }
            return;
        }
        match task & 0xFF {
            WAKE => wake_on_loop(p),
            DRAIN => drain_on_loop(p),
            CLOSE => close_on_loop(p),
            GRACE => grace(p),
            DEADLINE => arm_deadline_from_state(p),
            READ => {
                (*p).read_queued = false;
                if active(p) && (*p).reading {
                    do_read(p);
                }
            }
            RUN => {
                (*p).aside_queued = false;
                if active(p) {
                    transmit(p);
                }
            }
            REAP => reap(p),
            _ => {}
        }
    }

    // readiness

    unsafe fn ready(&self, ev: &mio::event::Event) {
        let t = ev.token().0;
        if t == WAKER.0 {
            return;
        }
        if ACCEPTORS <= t {
            let st = self.st();
            if let Some(Some(a)) = (*st).acceptors.get_mut(t - ACCEPTORS) {
                let a: *mut Acceptor = &mut **a;
                self.accept(a);
            }
            return;
        }
        let st = self.st();
        let p = match (*st).slots.get(t) {
            Some(&p) if !p.is_null() => p,
            _ => return,
        };
        if (*p).dead {
            return;
        }
        if (*p).connecting {
            if ev.is_writable() || ev.is_error() || ev.is_write_closed() {
                guarded(p, || finish_connect(p));
            }
            return;
        }
        guarded(p, || self.ready_slot(p, ev));
    }

    unsafe fn ready_slot(&self, p: *mut Slot, ev: &mio::event::Event) {
        // the write first: what is parked goes out before a read that may turn out to be the peer's
        // FIN
        if ev.is_writable() && (*p).writing {
            do_write(p);
            if !(*p).writing && active(p) {
                transmit(p);
            }
        }
        if ev.is_readable() || ev.is_read_closed() || ev.is_error() {
            (*p).read_blocked = false;
            (*p).read_hup |= ev.is_read_closed() || ev.is_error();
            if (*p).reading && active(p) {
                do_read(p);
            } else if cfg!(windows) && (*p).writing && active(p) {
                // the receive is parked: no read runs into "would block", and on Windows an event
                // takes every interest of the socket with it (mio#1612) - the parked write would
                // never hear that the socket takes it
                interest(p, true);
            }
        }
    }

    // slots

    /// A slot for a connection on this loop: registered with the poller, linked to its state.
    /// Nothing is armed yet.
    unsafe fn attach(&self, mut stream: TcpStream, ext: Arc<ExternalConnection>, sink: Box<dyn BytesDst>, source: Box<dyn BytesSrc>, ws: Option<Box<Codec>>, connecting: bool) -> io::Result<*mut Slot> {
        let st = self.st();
        // a slot of a host keeps its place on its loop from one session to the next: its waits on
        // the deadline lists go on, the old ones come up and are passed by
        let kept = ext.slot.load(Ordering::Relaxed);
        let mine = (kept as usize) < (*st).slots.len() && {
            let q = (*st).slots[kept as usize];
            !q.is_null() && (*q).dead && (*q).stream.is_none() && Arc::ptr_eq(&(*q).ext, &ext)
        };
        let idx = if mine {
            kept
        } else {
            match (*st).free.pop() {
                Some(i) => i,
                None => {
                    (*st).slots.push(std::ptr::null_mut());
                    (*st).gens.push(0);
                    ((*st).slots.len() - 1) as u32
                }
            }
        };
        let gen = ((*st).gens[idx as usize] + 1) & 0xFF_FFFF;
        (*st).gens[idx as usize] = gen;
        // writability only while a write is parked ([`interest`]): an event of no use is not
        // delivered, and on Windows it would take the interest in reading with it on Linux both at
        // once and for good: edge-triggered, a writability edge comes only after a write found the
        // socket full
        let interest = if connecting || EDGE_SHORT { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE }; // the writability says the connect is over
        if let Err(e) = (*st).poll.registry().register(&mut stream, Token(idx as usize), interest) {
            if !mine {
                (*st).free.push(idx);
            }
            return Err(e);
        }
        let key = ((idx as u64) << 32) | ((gen as u64) << 8);
        let link = match self.link(&ext, !connecting) {
            Ok(l) => l,
            Err(e) => {
                let _ = (*st).poll.registry().deregister(&mut stream);
                if !mine {
                    (*st).free.push(idx);
                }
                return Err(e);
            }
        };
        // The window of a transmit: as chosen; through a chain - the room of what its layers add, the
        // header of a record of TLS before it.
        let (t_lo, t_hi) = if link.is_null() {
            (0, self.buffer_size)
        } else if (*link).has_tls() {
            (TLS_ROOM, (self.buffer_size + HEADERS - TLS_TAIL).min(TLS_ROOM + 16384))
        } else {
            (0, self.buffer_size + HEADERS)
        };
        ext.buffer_size.store(self.buffer_size, Ordering::Relaxed);
        ext.upgrading.store(ws.is_some(), Ordering::Release); // a WebSocket session opens with its upgrade
        let dl = if mine { (*(*st).slots[idx as usize]).dl } else { [Dl::default(); 3] };
        let slot = Slot {
            idx,
            gen,
            lp: self as *const Loop,
            ext: ext.clone(),
            stream: Some(stream),
            sink: Some(sink),
            source: Some(source),
            rb: std::ptr::null_mut(),
            r_at: 0,
            r_end: 0,
            tbuf: None,
            t_at: 0,
            t_end: 0,
            t_lo,
            t_hi,
            link,
            reading: false,
            writing: false,
            streaming: false,
            running: false,
            wake_pending: false,
            held: false,
            aside_queued: false,
            read_queued: false,
            read_blocked: false,
            read_hup: false,
            in_read: false,
            pending_tail: false,
            receiving: false,
            receive_woken: false,
            filling: false,
            closing: false,
            fin_sent: false,
            connecting: false,
            report: None,
            dial_until: None,
            handshake_due: 0,
            on_gone: None,
            ws,
            dl,
            dead: false,
        };
        let p = if mine {
            let p = (*st).slots[idx as usize];
            *p = slot; // what the session before left is gone with its reap
            p
        } else {
            let p = Box::into_raw(Box::new(slot));
            (*st).slots[idx as usize] = p;
            ext.slot.store(idx, Ordering::Relaxed);
            p
        };
        if let Some(all) = self.all.get().and_then(std::sync::Weak::upgrade) {
            ext.link_to(&all, self.number, key);
        }
        Ok(p)
    }

    /// The tick: every connection of this loop gets its servicing when it asked for it; a line silent
    /// for its pulse says `Pulse_`; a client's link that is almost still while its session needs it
    /// is taken as lost ( `TCP::Redial::stalled` ).
    unsafe fn maintain(&self, now: i64) {
        let st = self.st();
        let tick = self.ticks.as_ref().map_or(0, |t| t.tick_ms.load(Ordering::Relaxed));
        for i in 0..(*st).slots.len() {
            let p = (*st).slots[i];
            if p.is_null() || (*p).dead || (*p).connecting || !(*p).ext.is_active() {
                continue;
            }
            guarded(p, || {
                let ext = &(*p).ext;
                if super::TCP::client::Core::tick(ext) {
                    return; // the session parks, the client dials again
                }
                let pulse = ext.pulse_ms.load(Ordering::Relaxed);
                if 0 < pulse && (!ext.dialing.load(Ordering::Relaxed) || ext.pulse_alone.load(Ordering::Relaxed)) {
                    // a side that hears no Pulse_ says it by itself; the next tick may come late by a
                    // tick: what it would miss goes now
                    ext.say_pulse(pulse - tick.min(pulse / 2));
                }
                ext.maintain(now)
            });
        }
    }

    /// A buffer of the loop's size: the last given back, or a new one.
    #[inline]
    pub(crate) unsafe fn take_buf(&self) -> Buf {
        let st = self.st();
        self.held.store(self.held.load(Ordering::Relaxed) + 1, Ordering::Relaxed); // the loop's own word: a plain write
        match (*st).bufs.pop() {
            Some(b) => b,
            // Past the size asked for, the room of the headers of a chain: the window stays what was
            // chosen, with nothing of a connection's own.
            None => vec![0u8; self.buffer_size + HEADERS].into_boxed_slice(),
        }
    }

    /// A whole-record buffer of a chain.
    pub(crate) unsafe fn take_record(&self) -> Buf {
        match (*self.st()).records.pop() {
            Some(b) => b,
            None => vec![0u8; RECORD_BUFFER].into_boxed_slice(),
        }
    }

    pub(crate) unsafe fn give_record(&self, b: Buf) {
        let st = self.st();
        if (*st).records.len() < 4 {
            (*st).records.push(b);
        } // else enough: let it go
    }

    /// The chain of a new socket: what its connection chose, the Noise stream it is seated with.
    /// Null - the socket as it is, and the library is not touched.
    unsafe fn link(&self, ext: &Arc<ExternalConnection>, server: bool) -> io::Result<*mut Link> {
        let chain = ext.chain();
        let noise = ext.noise().map(|h| h.0);
        if noise.is_none() && !chain.as_ref().is_some_and(|c| c.has_tls_for(server)) {
            return Ok(std::ptr::null_mut());
        }
        crate::lib::crypto::try_api().map_err(io::Error::other)?;
        let mut link = (*self.st()).links.pop().unwrap_or_else(Link::new);
        link.attach(self, server, chain.as_deref(), noise)?;
        Ok(Box::into_raw(link))
    }

    unsafe fn give_link(&self, link: *mut Link) {
        let st = self.st();
        if (*st).links.len() < 64 {
            (*st).links.push(Box::from_raw(link));
        } else {
            drop(Box::from_raw(link));
        }
    }

    /// A buffer back on the loop's stack: beyond [`BUFFERS_MAX`] it is let go.
    #[inline]
    pub(crate) unsafe fn give_buf(&self, b: Buf) {
        let st = self.st();
        self.held.store(self.held.load(Ordering::Relaxed) - 1, Ordering::Relaxed);
        if (*st).bufs.len() < BUFFERS_MAX {
            (*st).bufs.push(b);
        } // else enough: let it go
    }

    /// The host of `ext` is gone: its place on this loop goes, once its session is over.
    pub(crate) unsafe fn forget(&self, ext: &Arc<ExternalConnection>) {
        let st = self.st();
        let idx = ext.slot.load(Ordering::Relaxed) as usize;
        if idx < (*st).slots.len() {
            let p = (*st).slots[idx];
            if !p.is_null() && (*p).dead && Arc::ptr_eq(&(*p).ext, ext) && (*p).stream.is_none() {
                for k in 0..3 {
                    if (*p).dl[k].linked {
                        self.unfile(p, k);
                    }
                }
                self.free_slot(p);
            }
        }
    }

    /// The slot leaves the loop for good.
    unsafe fn free_slot(&self, p: *mut Slot) {
        let st = self.st();
        let idx = (*p).idx;
        (*st).slots[idx as usize] = std::ptr::null_mut();
        (*st).free.push(idx);
        drop(Box::from_raw(p));
    }

    // listeners

    unsafe fn add_acceptor(&self, mut listener: TcpListener, host: Arc<dyn Accept>, engine: Arc<Engine>, token: &AtomicUsize, closed: Arc<AtomicBool>) {
        if closed.load(Ordering::Acquire) {
            return; // closed before it listened: the socket closes here
        }
        let st = self.st();
        let i = (*st).acceptors.iter().position(|a| a.is_none()).unwrap_or_else(|| {
            (*st).acceptors.push(None);
            (*st).acceptors.len() - 1
        });
        if let Err(e) = (*st).poll.registry().register(&mut listener, Token(ACCEPTORS + i), Interest::READABLE) {
            host.failure(&e); // Report the accept failure to the host.
            return;
        }
        let handoffs = engine.loops.iter().map(|_| Handoff::new(host.clone())).collect();
        (*st).acceptors[i] = Some(Box::new(Acceptor { listener, host, engine, closed, deaf_until: 0, handoffs, next: 0, own: self.number as usize - 1 }));
        token.store(ACCEPTORS + i, Ordering::Release);
        let a: *mut Acceptor = &mut **(*st).acceptors[i].as_mut().unwrap();
        self.accept(a); // what is already in the backlog: an edge that came before the registration is not seen again
    }

    unsafe fn drop_acceptor(&self, token: usize) {
        if token < ACCEPTORS {
            return;
        }
        let st = self.st();
        if let Some(slot) = (*st).acceptors.get_mut(token - ACCEPTORS) {
            if let Some(mut a) = slot.take() {
                let _ = (*st).poll.registry().deregister(&mut a.listener);
            }
        }
    }

    /// Accepts in a batch, because a burst of connects arrives as one readiness; a bounded batch,
    /// because the loop's other sockets are waiting too - the rest is accepted on the next pass.
    unsafe fn accept(&self, a: *mut Acceptor) {
        if 0 < (*a).deaf_until {
            return;
        }
        for _ in 0..ACCEPT_BURST {
            match (*a).listener.accept() {
                Ok((stream, peer)) => {
                    if (*a).closed.load(Ordering::Acquire) {
                        return; // closed meanwhile: `stream` closes, the listener goes with the close's task
                    }
                    let Some(counted) = (*a).host.admit(peer) else {
                        drop(stream); // at a limit: the peer is told at once, not left hanging in the backlog
                        continue;
                    };
                    // the next loop round with room: this one always has it - the socket is served
                    // here at once
                    let a_ = &mut *a;
                    let n = a_.handoffs.len();
                    let to = loop {
                        let k = a_.next;
                        a_.next = if k + 1 == n { 0 } else { k + 1 };
                        if k == a_.own || a_.handoffs[k].room() {
                            break k;
                        }
                    };
                    if to == a_.own {
                        let host = a_.host.clone();
                        if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.accepted(self, stream, peer, counted))) {
                            a_.host.failure(&super::failure::panicked(&*why));
                        }
                    } else {
                        a_.handoffs[to].put(&a_.engine.loops[to], (stream, peer, counted)); // no allocation per socket
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // A peer may disconnect while queued in the accept backlog. Continue accepting
                // after ECONNABORTED or WSAECONNRESET.
                Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset) => continue,
                Err(e) => {
                    // out of descriptors, mostly: the listener goes deaf for a second - the backlog
                    // holds the peers meanwhile
                    (*a).host.failure(&e); // Report the listener failure before retrying.
                    (*a).deaf_until = self.nanos() + ((*a).host.accept_retry().as_nanos() as i64).max(1_000_000);
                    return;
                }
            }
        }
        let token = self.acceptor_token(a);
        (*self.st()).local.push(Local::Accept(token));
    }

    unsafe fn acceptor_token(&self, a: *mut Acceptor) -> usize {
        let st = self.st();
        for (i, x) in (*st).acceptors.iter_mut().enumerate() {
            if let Some(b) = x {
                if &mut **b as *mut Acceptor == a {
                    return ACCEPTORS + i;
                }
            }
        }
        usize::MAX
    }

    // deadlines: three lists threaded through the slots, each sorted as it is filled. A connection
    // is filed under its key - the deadline it had when it was filed - and the key does not change
    // while it is in the list: a busy connection renews its wait with two field writes and a
    // compare. When the key comes up the loop looks: cancelled - forgotten; run out - the timeout;
    // a later one by now - filed again.

    unsafe fn arm(&self, p: *mut Slot, k: usize, due: i64) {
        let d = &mut (*p).dl[k];
        d.due = due;
        d.armed = true;
        if d.linked {
            if d.at <= (due | (BEAT - 1)) + 1 {
                return; // comes up no later than the beat of this deadline
            }
            self.unfile(p, k);
        }
        self.file(p, k, due);
    }

    unsafe fn file(&self, p: *mut Slot, k: usize, due: i64) {
        let st = self.st();
        let at = (due | (BEAT - 1)) + 1;
        let me = (*p).idx;
        (*p).dl[k].at = at;
        (*p).dl[k].linked = true;
        let tail = (*st).tails[k];
        if tail == NONE {
            (*p).dl[k].next = NONE;
            (*st).heads[k] = me;
            (*st).tails[k] = me;
            return;
        }
        let sl = |i: u32| (*st).slots[i as usize];
        if (*sl(tail)).dl[k].at <= at {
            // the common case: the newest deadline is the latest
            (*p).dl[k].next = NONE;
            (*sl(tail)).dl[k].next = me;
            (*st).tails[k] = me;
            return;
        }
        let head = (*st).heads[k];
        if at < (*sl(head)).dl[k].at {
            (*p).dl[k].next = head;
            (*st).heads[k] = me;
            return;
        }
        let f = (*st).fingers[k];
        let mut q = if f != NONE && (*sl(f)).dl[k].linked && (*sl(f)).dl[k].at <= at { f } else { head };
        while (*sl((*sl(q)).dl[k].next)).dl[k].at <= at {
            q = (*sl(q)).dl[k].next;
        }
        (*p).dl[k].next = (*sl(q)).dl[k].next;
        (*sl(q)).dl[k].next = me;
        (*st).fingers[k] = me;
    }

    unsafe fn unfile(&self, p: *mut Slot, k: usize) {
        let st = self.st();
        let me = (*p).idx;
        let mut prev = NONE;
        let mut c = (*st).heads[k];
        while c != NONE {
            let cp = (*st).slots[c as usize];
            if c == me {
                let next = (*cp).dl[k].next;
                if prev == NONE {
                    (*st).heads[k] = next;
                } else {
                    (*(*st).slots[prev as usize]).dl[k].next = next;
                }
                if (*st).tails[k] == c {
                    (*st).tails[k] = prev;
                }
                if (*st).fingers[k] == c {
                    (*st).fingers[k] = prev;
                }
                break;
            }
            prev = c;
            c = (*cp).dl[k].next;
        }
        (*p).dl[k].next = NONE;
        (*p).dl[k].linked = false;
    }

    unsafe fn pop(&self, k: usize) -> *mut Slot {
        let st = self.st();
        let h = (*st).heads[k];
        let hp = (*st).slots[h as usize];
        (*st).heads[k] = (*hp).dl[k].next;
        if (*st).heads[k] == NONE {
            (*st).tails[k] = NONE;
        }
        if (*st).fingers[k] == h {
            (*st).fingers[k] = NONE;
        }
        (*hp).dl[k].next = NONE;
        (*hp).dl[k].linked = false;
        hp
    }

    /// How long the poller may sleep: to the soonest head - nothing armed, a second.
    unsafe fn next_deadline(&self) -> std::time::Duration {
        let st = self.st();
        let mut at = i64::MAX;
        for k in 0..3 {
            while (*st).heads[k] != NONE {
                let h = (*st).slots[(*st).heads[k] as usize];
                if (*h).dl[k].armed {
                    break;
                }
                self.pop(k);
            }
            if (*st).heads[k] != NONE {
                let h = (*st).slots[(*st).heads[k] as usize];
                at = at.min((*h).dl[k].at);
            }
        }
        for a in (*st).acceptors.iter().flatten() {
            if 0 < a.deaf_until {
                at = at.min(a.deaf_until);
            }
        }
        if self.ticks.is_some() {
            at = at.min((*st).next_tick).min((*st).next_reap);
        }
        for r in (*st).redials.iter() {
            at = at.min(r.0);
        }
        if at == i64::MAX {
            return std::time::Duration::from_secs(1);
        }
        let ns = (at - clock()).max(1_000_000);
        std::time::Duration::from_nanos(ns as u64)
    }

    /// Every key that has come up.
    unsafe fn expire(&self) {
        let st = self.st();
        let now = self.nanos();
        if let Some(t) = &self.ticks {
            if (*st).next_tick <= now {
                // the tick: one compare a pass until it comes up
                (*st).next_tick = now + t.tick_ms.load(Ordering::Relaxed) * 1_000_000;
                self.maintain(now);
                (*st).next_reap = now; // the parked sessions are looked at on the tick as well
            }
            if (*st).next_reap <= now {
                // a parked session ran out: the nearest of the rest is when the loop wakes again
                let nearest = t.reap(self.millis(), self.number);
                (*st).next_reap = if nearest == i64::MAX { i64::MAX } else { reap_at(nearest) };
            }
        }
        let mut i = 0;
        while i < (*st).redials.len() {
            if now < (*st).redials[i].0 {
                i += 1;
                continue;
            }
            // the wait of a client's parked session for its next dial is over: it dials, on this loop
            let (_, core) = (*st).redials.swap_remove(i);
            if let Some(core) = core.upgrade() {
                self.quietly(|| core.redial());
            }
        }
        for k in 0..3 {
            while (*st).heads[k] != NONE {
                let h = (*st).slots[(*st).heads[k] as usize];
                if now < (*h).dl[k].at {
                    break;
                }
                let c = self.pop(k);
                if !(*c).dl[k].armed || (*c).dead {
                    continue;
                }
                if now < (*c).dl[k].due {
                    let due = (*c).dl[k].due;
                    self.file(c, k, due);
                    continue;
                }
                (*c).dl[k].armed = false;
                match k {
                    R if (*c).connecting => guarded(c, || give_up_connect(c)),
                    R => guarded(c, || timed_out_read(c)),
                    T => guarded(c, || timed_out_write(c)),
                    _ => guarded(c, || timed_out_deadline(c)),
                }
            }
        }
        let mut deaf: Vec<*mut Acceptor> = Vec::new();
        for a in (*st).acceptors.iter_mut().flatten() {
            if 0 < a.deaf_until && a.deaf_until <= now {
                a.deaf_until = 0;
                deaf.push(&mut **a);
            }
        }
        for a in deaf {
            self.accept(a);
        }
    }

    /// A failure of the loop itself, to the handler of its engine.
    #[cold]
    fn failure(&self, e: &(dyn std::error::Error + 'static)) {
        let name = std::thread::current().name().unwrap_or("a loop").to_string();
        match self.on_failure.get() {
            // a handler that panics does not take the loop with it: its own failure is printed
            Some(f) => {
                if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&name, e))) {
                    (super::failure::print_console())(&name, &super::failure::panicked(&*why));
                }
            }
            None => (super::failure::print_console())(&name, e),
        }
    }

    /// Runs code of the application no connection answers for: a panic out of it goes to the
    /// handler of the engine and the loop goes on.
    #[inline]
    fn quietly(&self, f: impl FnOnce()) {
        if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            self.failure(&super::failure::panicked(&*why));
        }
    }

    fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.waker.wake();
    }
}

/// A listening socket on a loop.
struct Acceptor {
    listener: TcpListener,
    host: Arc<dyn Accept>,
    engine: Arc<Engine>,
    /// Its [`Listening::close`] was called.
    closed: Arc<AtomicBool>,
    /// While it is deaf (`accept` failed): when to listen again, by the loop's clock; 0 - it
    /// listens.
    deaf_until: i64,
    /// One per loop of the engine, made once with the listener: the sockets accepted for that loop
    /// (this loop's own unused - its sockets are served at once).
    handoffs: Box<[Arc<Handoff>]>,
    /// The loop the round goes on from, and the number of this loop among them.
    next: usize,
    own: usize,
}

/// The sockets one listener accepted for one loop: a ring of one writer - the listener's loop - and
/// one reader - that loop; made once with the listener, on that loop's stack of handoffs once for
/// any number of sockets. No allocation per socket.
pub(crate) struct Handoff {
    host: Arc<dyn Accept>,
    cells: [UnsafeCell<std::mem::MaybeUninit<(TcpStream, SocketAddr, bool)>>; ACCEPT_BURST],
    head: AtomicU64,
    tail: AtomicU64,
    /// On its loop's stack, or about to be.
    queued: AtomicBool,
    next: std::sync::atomic::AtomicPtr<Handoff>,
}

// SAFETY: a cell is written by the one writer before `tail` publishes it and read by the one reader
// after; `head` gives it back
unsafe impl Send for Handoff {}
unsafe impl Sync for Handoff {}

impl Handoff {
    fn new(host: Arc<dyn Accept>) -> Arc<Self> {
        Arc::new(Self {
            host,
            cells: std::array::from_fn(|_| UnsafeCell::new(std::mem::MaybeUninit::uninit())),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            queued: AtomicBool::new(false),
            next: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        })
    }

    /// The writer: a cell is free.
    #[inline]
    fn room(&self) -> bool {
        self.tail.load(Ordering::Relaxed) - self.head.load(Ordering::Acquire) < ACCEPT_BURST as u64
    }

    /// The listener's loop: one more socket for `to` - with [`room`](Self::room).
    unsafe fn put(self: &Arc<Self>, to: &Loop, socket: (TcpStream, SocketAddr, bool)) {
        let t = self.tail.load(Ordering::Relaxed);
        (*self.cells[(t % ACCEPT_BURST as u64) as usize].get()).write(socket);
        self.tail.store(t + 1, Ordering::SeqCst); // publishes the cell; written first, `queued` read second
        if !self.queued.load(Ordering::SeqCst) && !self.queued.swap(true, Ordering::SeqCst) {
            Arc::increment_strong_count(Arc::as_ptr(self));
            to.push_handoff(Arc::as_ptr(self));
        }
    }

    /// Its loop: a slot for every socket handed over.
    unsafe fn take(&self, lp: &Loop) {
        self.queued.store(false, Ordering::SeqCst); // written first, the tail read second: a socket put after it pushes again
        let t = self.tail.load(Ordering::SeqCst);
        let mut h = self.head.load(Ordering::Relaxed);
        while h < t {
            let (stream, peer, counted) = (*self.cells[(h % ACCEPT_BURST as u64) as usize].get()).assume_init_read();
            h += 1;
            self.head.store(h, Ordering::Release); // the cell is the writer's again
            let host = self.host.clone();
            if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.accepted(lp, stream, peer, counted))) {
                self.host.failure(&super::failure::panicked(&*why));
            }
        }
    }
}

impl Drop for Handoff {
    /// The sockets nobody took: they close.
    fn drop(&mut self) {
        for h in *self.head.get_mut()..*self.tail.get_mut() {
            // SAFETY: the cells between head and tail hold what was put and not taken
            unsafe { (*self.cells[(h % ACCEPT_BURST as u64) as usize].get()).assume_init_drop() };
        }
    }
}

// Slot: one full-duplex connection on a loop

/// One of a connection's three waits (data, a parked write, the actor instances' deadline) and its
/// place in the loop's list.
#[derive(Clone, Copy)]
pub(crate) struct Dl {
    next: u32,
    due: i64,
    at: i64,
    armed: bool,
    linked: bool,
}

impl Default for Dl {
    fn default() -> Self {
        Self { next: NONE, due: 0, at: 0, armed: false, linked: false }
    }
}


/// One full-duplex TCP connection: the socket, the session on it, and everything between them.
/// Loop-thread only.
pub(crate) struct Slot {
    idx: u32,
    gen: u32,
    pub(crate) lp: *const Loop,
    pub(crate) ext: Arc<ExternalConnection>,
    pub(crate) stream: Option<TcpStream>,
    pub(crate) sink: Option<Box<dyn BytesDst>>,
    pub(crate) source: Option<Box<dyn BytesSrc>>,
    /// The receive buffer, the connection's until it goes ([`RxBuf`]), made with the first read:
    /// `[r_at, r_end)` received and not taken yet. A relay may take its storage over, putting one
    /// of its own in its place.
    pub(crate) rb: *mut RxBuf,
    pub(crate) r_at: usize,
    pub(crate) r_end: usize,
    /// The transmit buffer, taken from the loop for a write and given back once the source is
    /// empty: `[t_at, t_end)` to be written.
    pub(crate) tbuf: Option<Buf>,
    pub(crate) t_at: usize,
    pub(crate) t_end: usize,
    /// Where in the transmit buffer the framing may write.
    pub(crate) t_lo: usize,
    pub(crate) t_hi: usize,
    /// What the bytes of this socket go through ([`Link`]); null - the socket as it is.
    pub(crate) link: *mut Link,
    pub(crate) reading: bool,
    writing: bool,
    streaming: bool,
    running: bool,
    wake_pending: bool,
    /// The source gave nothing and said a pack waits (`read` 0): not empty - a graceful close waits
    /// for it.
    pub(crate) held: bool,
    aside_queued: bool,
    read_queued: bool,
    /// The last read ran into "would block": the next data comes with a readiness.
    read_blocked: bool,
    /// The peer's FIN (or an error) came with a readiness: no readiness comes after it, a read goes
    /// on to the end.
    read_hup: bool,
    in_read: bool,
    pub(crate) pending_tail: bool,
    receiving: bool,
    receive_woken: bool,
    /// The source is giving the transmit its bytes: a receive asked for meanwhile runs after it.
    filling: bool,
    /// A graceful close is under way: the queue drains, then the FIN (or the WebSocket CLOSE
    /// frame).
    pub(crate) closing: bool,
    fin_sent: bool,
    /// The socket is connecting.
    connecting: bool,
    /// Client side: who waits for the socket to connect and its session to start - at once for TCP,
    /// with the 101 for a WebSocket ([`report`]) -, and by when the session is to start.
    pub(crate) report: Option<super::TCP::Report>,
    dial_until: Option<i64>,
    /// When an accepted connection has to be done with its handshakes, a deadline of the loop; 0 -
    /// no such wait.
    handshake_due: i64,
    /// What the host does once the connection is gone (its registry, its admission).
    pub(crate) on_gone: Option<Box<dyn FnOnce()>>,
    pub(crate) ws: Option<Box<Codec>>,
    dl: [Dl; 3],
    dead: bool,
}


#[inline]
pub(crate) unsafe fn lp<'a>(p: *mut Slot) -> &'a Loop {
    &*(*p).lp
}

/// The receive buffer of the connection.
#[inline(always)]
pub(crate) unsafe fn rbuf<'a>(p: *mut Slot) -> &'a RxBuf {
    &*(*p).rb
}

/// A byte of the receive buffer: one of the connection's own.
#[inline(always)]
pub(crate) unsafe fn rbyte(p: *mut Slot, i: usize) -> u8 {
    *rbuf(p).as_ptr().add(i)
}

/// `[at, end)` of the receive buffer, to write: the connection's own bytes.
#[inline(always)]
pub(crate) unsafe fn rbytes<'a>(p: *mut Slot, at: usize, end: usize) -> &'a mut [u8] {
    rbuf(p).slice_mut(at, end)
}

#[inline]
pub(crate) unsafe fn active(p: *mut Slot) -> bool {
    !(*p).dead && (*p).ext.is_open.load(Ordering::Acquire)
}

/// How many receive buffers the connection of `state` holds, asked on its loop: for the tests.
#[cfg(test)]
pub(crate) fn receive_buffers(state: &super::TCP::external_connection::ExternalConnection) -> usize {
    let Some((lp, key)) = state.link() else { return usize::MAX };
    let (tx, rx) = std::sync::mpsc::channel();
    lp.submit_fn(Box::new(move |lp| unsafe {
        on_slot(lp, key, |p| {
            let _ = tx.send(usize::from(!(*p).rb.is_null()));
        })
    }));
    rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or(usize::MAX)
}

/// Runs `f` on the slot `key` names, on its loop's thread.
pub(crate) unsafe fn on_slot(l: &Loop, key: u64, f: impl FnOnce(*mut Slot)) {
    let p = l.slot(key);
    if !p.is_null() && !(*p).dead && !(*p).connecting {
        f(p) // still connecting: what was asked for runs when its session starts
    }
}

/// The work of `kind` on the slot `key` names, asked on its loop's thread: as the task of that kind
/// from another thread runs ([`Loop::task_of`]) - while connecting, a graceful close tears the
/// connect down and a deadline is filed, the rest waits for the session.
#[inline]
pub(crate) unsafe fn on_slot_kind(l: &Loop, key: u64, kind: u64, f: impl FnOnce(*mut Slot)) {
    let p = l.slot(key);
    if p.is_null() || (*p).dead {
        return;
    }
    if !(*p).connecting {
        return f(p);
    }
    match kind {
        GRACE => close_on_loop(p),
        DEADLINE => arm_deadline_from_state(p),
        _ => {}
    }
}

/// The teardown of the slot `key` names, on its loop's thread: whatever it is doing, connecting
/// too.
pub(crate) unsafe fn close_slot(l: &Loop, key: u64) {
    let p = l.slot(key);
    if !p.is_null() && !(*p).dead {
        close_on_loop(p)
    }
}

/// A connect is over: whoever waits for it hears it on this slot - connected, its addresses taken
/// and only the reads watched from now on, or what went wrong.
unsafe fn finish_connect(p: *mut Slot) {
    let stream = (*p).stream.as_ref().unwrap();
    let r = match stream.take_error() {
        Ok(Some(e)) | Err(e) => Err(e),
        Ok(None) => match stream.peer_addr() {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotConnected => return, // not yet: a readiness of another kind
            Err(e) => Err(e),
        },
    };
    (*p).connecting = false;
    (*p).dl[R].armed = false; // the wait for the connect is over: its entry comes up and is passed by
    if r.is_ok() {
        (*p).ext.set_addresses(stream.local_addr().ok(), stream.peer_addr().ok()); // Publish local and peer socket addresses.
        interest(p, false);
    }
    connected(p, r);
}

/// The wait for a connect ran out: whoever waits hears `TimedOut`, on this slot.
unsafe fn give_up_connect(p: *mut Slot) {
    (*p).connecting = false;
    report(p, Err(io::Error::new(io::ErrorKind::TimedOut, "no connect in time")));
}

/// The connect is over: a WebSocket waits for its 101 for what is left of the time, its actors
/// start; raw TCP has its session.
unsafe fn connected(p: *mut Slot, r: io::Result<()>) {
    if let Err(e) = r {
        return report(p, Err(e)); // a reconnect's session parks again
    }
    let ext = (*p).ext.clone();
    if (*p).ws.is_some() || !(*p).link.is_null() && (*(*p).link).handshaking() {
        if let Some(u) = (*p).dial_until {
            wait_session(p, u); // what is left of the timeout: for the 101, for the handshake of TLS
        }
        // the socket is there, its actors start; the 101, or the end of the handshake, says the session
        // started
        guarded(p, || ext.fire(Event::THIS_CONNECT));
    } else {
        guarded(p, || ext.fire(Event::THIS_CONNECT));
        report(p, Ok(()));
    }
    if active(p) {
        start(p);
    }
}

/// Tells whoever waits for the connect of the slot how it went - once: the connection, or why not;
/// a failure is the handler's too and tears the connection down.
pub(crate) unsafe fn report(p: *mut Slot, r: io::Result<()>) {
    let Some(rep) = (*p).report.take() else { return };
    match r {
        Ok(()) => rep.call(Ok((*p).ext.clone())),
        Err(e) => {
            let ext = (*p).ext.clone();
            ext.failure(&e);
            ext.disconnect(Event::THIS_CLOSE_ABRUPTLY);
            rep.call(Err(e))
        }
    }
}

/// Connects `stream` - a socket whose connect is under way - in the slot of `ext` on this loop,
/// with the protocol layer seated there: `report` hears on this slot once the session started, or
/// what went wrong - `TimedOut` when `until` ([`Loop::deadline_of`], taken when the connect was
/// asked for) came first, the loop times it as any wait of the slot. Nothing else of the connection
/// runs until then; a close meanwhile is heard as `ConnectionAborted`.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn connect(l: &Loop, stream: TcpStream, ext: Arc<ExternalConnection>, sink: Box<dyn BytesDst>, source: Box<dyn BytesSrc>, ws: Option<Box<Codec>>, until: Option<i64>, rep: super::TCP::Report) {
    match l.attach(stream, ext.clone(), sink, source, ws, true) {
        Ok(p) => {
            (*p).connecting = true;
            (*p).report = Some(rep);
            (*p).dial_until = until;
            if let Some(u) = until {
                l.arm(p, R, u); // the wait for the connect: a wait of the slot, from the moment it was asked for
            }
        }
        Err(e) => {
            ext.failure(&e);
            ext.disconnect(Event::THIS_CLOSE_ABRUPTLY);
            rep.call(Err(e))
        }
    }
}

/// Adopts a connected socket on this loop: a slot, the protocol layer seated on it. Nothing starts
/// yet.
pub(crate) unsafe fn adopt(l: &Loop, stream: TcpStream, ext: Arc<ExternalConnection>, sink: Box<dyn BytesDst>, source: Box<dyn BytesSrc>, ws: Option<Box<Codec>>) -> io::Result<*mut Slot> {
    ext.set_addresses(stream.local_addr().ok(), stream.peer_addr().ok()); // Publish local and peer socket addresses.
    l.attach(stream, ext, sink, source, ws, false)
}

/// The session of a client is to start by `until` (a deadline of the loop, [`Loop::deadline_of`]):
/// the wait for data is armed to it, and running out it fails the connect ([`Slot::report`]).
pub(crate) unsafe fn wait_session(p: *mut Slot, until: i64) {
    lp(p).arm(p, R, until);
}

/// The session started (a WebSocket's 101 came): its wait is over, the receive deadline is the
/// connection's from here.
pub(crate) unsafe fn session_started(p: *mut Slot) {
    (*p).dl[R].armed = false;
}

/// A handshake of this connection is still under way: TLS, Noise, the upgrade of a WebSocket.
unsafe fn handshaking(p: *mut Slot) -> bool {
    (*p).ext.upgrading.load(Ordering::Acquire) || !(*p).link.is_null() && (*(*p).link).handshaking() || (*p).ext.noise().is_some_and(|n| !n.ready())
}

/// An accepted connection is to be done with its handshakes in `ms` milliseconds: with none under
/// way, or no limit, there is no such wait. Before it starts.
pub(crate) unsafe fn limit_handshake(p: *mut Slot, ms: i64) {
    if 0 < ms && handshaking(p) {
        (*p).handshake_due = lp(p).nanos() + ms * 1_000_000;
    }
}

/// Both directions start: what the source holds goes out, the connection waits for data.
pub(crate) unsafe fn start(p: *mut Slot) {
    let l = lp(p);
    let now = l.millis();
    (*p).ext.last_rx_ms.store(now, Ordering::Relaxed);
    (*p).ext.last_tx_ms.store(now, Ordering::Relaxed);
    wake_on_loop(p);
    if active(p) {
        wait_for_data(p);
        arm_deadline_from_state(p); // the deadline of the session's actors: a resumed session goes on with it on this socket
    }
}

// receiving

/// The receive buffer, made with the first read.
#[inline(always)]
unsafe fn receive_buffer(p: *mut Slot) {
    if (*p).rb.is_null() {
        // through a chain its window fits what a peer of the same size asked for sends: its record with the headers
        (*p).rb = Box::into_raw(RxBuf::new(lp(p).buffer_size + if (*p).link.is_null() { 0 } else { HEADERS }));
    }
}

/// Reads into the receive buffer and hands it to [`receive`], until the socket would block. A
/// bounded number of times: the loop's other connections are waiting too, so the rest is read on
/// the next pass.
unsafe fn do_read(p: *mut Slot) {
    if !(*p).link.is_null() {
        return do_read_chain(p);
    }
    let l = lp(p);
    (*p).in_read = true;
    for _ in 0..BURST {
        receive_buffer(p);
        let end = (*p).r_end;
        let r = {
            let len = rbuf(p).len();
            if end == len {
                (*p).in_read = false;
                return; // nothing taken and no room: a handshake that does not fit refuses itself
            }
            (*p).stream.as_ref().unwrap().read(rbytes(p, end, len))
        };
        match r {
            Ok(0) => {
                (*p).in_read = false;
                let ev = if closing(p) { Event::THIS_CLOSE_GRACEFUL } else { Event::REMOTE_CLOSE_GRACEFUL };
                return disconnect(p, ev);
            }
            Ok(n) => {
                if (*p).report.is_none() {
                    (*p).dl[R].armed = false; // data came: the deadline is off until the next wait
                    (*p).ext.heard.store(true, Ordering::Relaxed); // the peer serves ( TCP::Redial )
                } // else the wait for the session (a WebSocket's 101) runs on: data that is not all of it does not
                // stop it
                (*p).r_end = end + n;
                (*p).ext.carry(n);
                (*p).streaming = (*p).r_end == rbuf(p).len();
                let emptied = EDGE_SHORT && !(*p).streaming && !(*p).read_hup;
                (*p).ext.last_rx_ms.store(l.millis(), Ordering::Relaxed);
                receive(p);
                if !active(p) || !(*p).reading {
                    (*p).in_read = false;
                    return;
                }
                if emptied {
                    (*p).read_blocked = true; // less than asked: the socket is empty, the next data comes with an edge
                    (*p).in_read = false;
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                (*p).read_blocked = true;
                (*p).in_read = false;
                return;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                (*p).in_read = false;
                return disconnect(p, Event::REMOTE_CLOSE_ABRUPTLY);
            }
        }
    }
    (*p).in_read = false;
    queue_read(p);
}

/// As [`do_read`], through the chain of the connection.
unsafe fn do_read_chain(p: *mut Slot) {
    let l = lp(p);
    let t = (*p).link;
    (*p).in_read = true;
    for _ in 0..BURST {
        receive_buffer(p);
        (*t).before_read(p);
        let at = (*t).at((*p).r_end);
        (*t).receiving(p);
        let end = (*p).r_end;
        let len = Link::window(rbuf(p).len());
        if end == len {
            (*p).in_read = false;
            return; // nothing taken and no room: a handshake that does not fit refuses itself
        }
        match (*p).stream.as_ref().unwrap().read(rbytes(p, end, len)) {
            Ok(0) => {
                (*p).in_read = false;
                let ev = if closing(p) { Event::THIS_CLOSE_GRACEFUL } else { Event::REMOTE_CLOSE_GRACEFUL };
                return disconnect(p, ev);
            }
            Ok(n) => {
                if (*p).report.is_none() {
                    (*p).dl[R].armed = false; // data came: the deadline is off until the next wait
                    (*p).ext.heard.store(true, Ordering::Relaxed); // the peer serves ( TCP::Redial )
                }
                (*p).r_end = end + n;
                (*p).ext.carry(n);
                (*p).streaming = end + n == len;
                let emptied = EDGE_SHORT && !(*p).streaming && !(*p).read_hup;
                (*p).ext.last_rx_ms.store(l.millis(), Ordering::Relaxed);
                match (*t).received(p, at) {
                    Err(e) => {
                        (*p).in_read = false;
                        return transport_failed(p, e);
                    }
                    Ok(false) => {
                        // nothing to take up: its own records, the handshake, a part of a record
                        if !active(p) {
                            (*p).in_read = false;
                            return;
                        }
                        wait_for_data(p);
                    }
                    Ok(true) => {
                        receive(p);
                        if !active(p) || !(*p).reading {
                            (*p).in_read = false;
                            return;
                        }
                    }
                }
                (*t).big_done(p);
                if emptied {
                    (*p).read_blocked = true; // less than asked: the socket is empty, the next data comes with an edge
                    (*p).in_read = false;
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                (*p).read_blocked = true;
                (*p).in_read = false;
                return;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                (*p).in_read = false;
                return disconnect(p, Event::REMOTE_CLOSE_ABRUPTLY);
            }
        }
    }
    (*p).in_read = false;
    queue_read(p);
}

/// A handshake or a record of the chain failed: its last word goes out, whoever waits hears it, and
/// the connection closes, it does not hang.
#[cold]
pub(crate) unsafe fn transport_failed(p: *mut Slot, e: io::Error) {
    if let Some(s) = (*p).stream.as_ref().filter(|_| !pending(p)) { // a record half out: anything behind it would break TLS
        (*(*p).link).failed(s);
    }
    if (*p).report.is_some() {
        report(p, Err(e)); // the handler too, then the teardown
    } else {
        (*p).ext.failure(&e);
        disconnect(p, Event::THIS_CLOSE_ABRUPTLY);
    }
}

/// The run in hand of the receive buffer is all taken: the next one of the chain, or the buffer is
/// empty.
#[inline]
pub(crate) unsafe fn run_over(p: *mut Slot) -> bool {
    if !(*p).link.is_null() && (*(*p).link).next_piece(p) {
        return true;
    }
    (*p).r_at = 0;
    (*p).r_end = 0;
    (*p).pending_tail = false;
    false
}

#[inline]
unsafe fn queue_read(p: *mut Slot) {
    if !(*p).read_queued {
        (*p).read_queued = true;
        lp(p).later(task(p, READ));
    }
}

#[inline]
pub(crate) unsafe fn task(p: *mut Slot, kind: u64) -> u64 {
    ((*p).idx as u64) << 32 | ((*p).gen as u64) << 8 | kind
}

/// One drain of the receive buffer into the sink. All taken - the connection waits again. Refused -
/// the tail stays parked and reads stop until [`drain_on_loop`]. A panic of the sink goes to the
/// handler of the host and the connection goes on.
pub(crate) unsafe fn receive(p: *mut Slot) {
    if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| receive_on(p))) {
        (*p).receiving = false;
        (*p).ext.failure(&super::failure::panicked(&*why));
    }
}

unsafe fn receive_on(p: *mut Slot) {
    (*p).receiving = true;
    loop {
        (*p).receive_woken = false;
        (*p).pending_tail = true;
        if (*p).ws.is_some() {
            super::TCP::web_socket::receive(p);
        } else {
            receive_tcp(p);
        }
        if !active(p) {
            (*p).receiving = false;
            return;
        }
        if !((*p).pending_tail && (*p).receive_woken) {
            break; // refused, but its destination made room meanwhile: offered once more
        }
    }
    (*p).receiving = false;
    if (*p).pending_tail {
        park(p);
        return;
    }
    wait_for_data(p);
}

/// Raw TCP: the bytes go to the sink where they lie, and it may take the buffer whole.
unsafe fn receive_tcp(p: *mut Slot) {
    loop {
        let (at, end) = ((*p).r_at, (*p).r_end);
        if at == end {
            if run_over(p) {
                continue;
            }
            return;
        }
        let r = match (*p).sink.as_mut() {
            Some(s) => {
                rbuf(p).set_filled(if (*p).link.is_null() { end } else { 0 }); // a relay may take the storage over: a region that runs to here leaves nothing behind; not of a chain - other pieces lie behind it
                s.on_block(rbuf(p), at, end)
            }
            None => Some(end - at),
        };
        if !active(p) {
            return;
        }
        match r {
            None => {
                (*p).pending_tail = false;
                return reject(p);
            }
            Some(n) => {
                (*p).r_at = at + n;
                if (*p).r_at == end {
                    if run_over(p) {
                        continue;
                    }
                    return;
                }
                if n == 0 {
                    return; // backpressure: the destination refused the tail, a wake offers it again
                }
                // part taken, part left and nothing parked: the rest goes to the destination at
                // once
            }
        }
    }
}

/// The sink gave up on the stream (an unknown pack id, a forged record): the connection is lost the
/// way a broken link is. A pack it could not parse broke the stream for good: the connection ends
/// by our own verdict, its session does not park.
pub(crate) unsafe fn reject(p: *mut Slot) {
    if (*p).sink.as_ref().is_some_and(|s| s.broken()) {
        return disconnect(p, Event::THIS_CLOSE_ABRUPTLY);
    }
    let graceful = (*p).sink.as_ref().is_some_and(|s| s.graceful_close());
    disconnect(p, if graceful { Event::REMOTE_CLOSE_GRACEFUL } else { Event::REMOTE_CLOSE_ABRUPTLY });
}

/// The destination refused the tail: no more reads until it wakes the receive.
unsafe fn park(p: *mut Slot) {
    (*p).reading = false;
    (*p).dl[R].armed = false;
}

/// The connection wants data: the receive deadline runs. Holds no buffer - one is taken when the
/// data is there.
pub(crate) unsafe fn wait_for_data(p: *mut Slot) {
    if !active(p) {
        return;
    }
    (*p).reading = true;
    let l = lp(p);
    if (*p).handshake_due != 0 && handshaking(p) {
        l.arm(p, R, (*p).handshake_due);
    } else {
        if (*p).handshake_due != 0 {
            (*p).handshake_due = 0; // over: the receive deadline is the session's own from here
            (*p).dl[R].armed = false;
        }
        let rx = (*p).ext.receive_timeout_ms.load(Ordering::Relaxed);
        let t = if closing(p) { rx.min((*p).ext.linger_timeout_ms.load(Ordering::Relaxed)) } else { rx };
        // while the session is waited for (a WebSocket's 101) its own deadline stands: a receive
        // timeout does not replace it
        if 0 < t && t < i32::MAX as i64 && (*p).report.is_none() {
            l.arm(p, R, l.nanos() + t * 1_000_000);
        }
    }
    if !(*p).in_read && !(*p).read_blocked {
        queue_read(p); // the socket was not read to its end: the readiness that would say so is not coming again
    }
}

/// The destination that refused the tail has room again.
pub(crate) unsafe fn drain_on_loop(p: *mut Slot) {
    if !active(p) || !(*p).pending_tail { // not paused
        return;
    }
    if (*p).receiving {
        (*p).receive_woken = true; // from inside this very receive: the first pass goes on by itself
        return;
    }
    if (*p).filling {
        // from inside the transmit of this very connection: the record layer under both directions (Noise) is in the middle of
        // it - the receive runs once the transmit is over
        return lp(p).later(task(p, DRAIN));
    }
    (*p).ext.last_rx_ms.store(lp(p).millis(), Ordering::Relaxed);
    receive(p);
}

// transmitting

/// There is something to send: the runner starts here, within the inline bound.
pub(crate) unsafe fn wake_on_loop(p: *mut Slot) {
    if !active(p) {
        return;
    }
    if (*p).running {
        (*p).wake_pending = true; // the runner turns once more before it stops
        return;
    }
    let st = lp(p).st();
    if (*st).serving || (*p).receiving {
        // after the ready sockets: the packs their reads bring about go out together; after the receive of this very connection:
        // the record layer under both directions (Noise) is in the middle of it
        return step_aside(p);
    }
    if (*st).depth < INLINE_DEPTH {
        (*st).depth += 1;
        transmit(p);
        (*st).depth -= 1;
    } else {
        step_aside(p);
    }
}

unsafe fn step_aside(p: *mut Slot) {
    if (*p).aside_queued {
        return;
    }
    (*p).aside_queued = true;
    lp(p).later(task(p, RUN));
}

/// The transmit runner: pulls from the source into the transmit buffer and writes it, turn after
/// turn, until the source has nothing left. A write the socket would not take whole parks and
/// suspends the runner until the socket is writable again. After [`BURST`] turns it steps aside for
/// the loop's other connections.
pub(crate) unsafe fn transmit(p: *mut Slot) {
    let l = lp(p);
    (*p).running = true;
    let mut turn = 0;
    loop {
        (*p).wake_pending = false;
        if !active(p) || (*p).writing {
            break;
        }
        if turn == BURST {
            step_aside(p);
            return;
        }
        turn += 1;
        if (*p).tbuf.is_none() {
            (*p).tbuf = Some(l.take_buf()); // the loop's, for as long as the source gives
        }
        (*p).filling = true;
        let filled = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fill(p))) {
            Ok(f) => f,
            Err(why) => {
                // the source panicked: its handler hears it, the connection goes on with nothing to
                // send
                (*p).held = false;
                (*p).ext.failure(&super::failure::panicked(&*why));
                false
            }
        };
        (*p).filling = false;
        if !active(p) {
            break;
        }
        if filled {
            do_write(p);
            if !active(p) || (*p).writing {
                break;
            }
            continue; // all out: pull more
        }
        if (*p).wake_pending {
            continue;
        }
        if let Some(b) = (*p).tbuf.take() {
            l.give_buf(b); // nothing to send: hold nothing
        }
        (*p).running = false;
        if !(*p).held {
            on_drained(p); // empty: a graceful close goes on; a pack that waits is woken by its producer
        }
        return;
    }
    if !(*p).writing {
        (*p).running = false;
    }
}

unsafe fn fill_tcp(p: *mut Slot) -> bool {
    let Some(src) = (*p).source.as_mut() else { return false };
    let (lo, hi) = ((*p).t_lo, (*p).t_hi);
    let n = src.read(&mut (*p).tbuf.as_mut().unwrap()[lo..hi]);
    (*p).held = n == 0;
    if n <= 0 {
        return false;
    }
    (*p).t_at = lo;
    (*p).t_end = lo + n as usize;
    true
}

/// One fill of the transmit buffer in hand from the source: false - nothing to send.
#[inline]
unsafe fn fill(p: *mut Slot) -> bool {
    if (*p).link.is_null() {
        return fill_framed(p);
    }
    match (*(*p).link).transmit(p) {
        Ok(filled) => filled,
        Err(e) => {
            transport_failed(p, e);
            false
        }
    }
}

/// One fill by the framing, in the window of the transmit buffer.
#[inline]
pub(crate) unsafe fn fill_framed(p: *mut Slot) -> bool {
    if (*p).ws.is_some() {
        super::TCP::web_socket::fill(p)
    } else {
        fill_tcp(p)
    }
}

/// Something of the write is not out yet.
#[inline]
unsafe fn pending(p: *mut Slot) -> bool {
    (*p).t_at < (*p).t_end
}

/// Writes what the transmit buffer holds: one fill, one buffer, one write - what a connection sends
/// at once is the size of its buffer, which the user sets. What the socket does not take stays,
/// parked, with the transmit deadline running; the runner waits for the socket.
unsafe fn do_write(p: *mut Slot) {
    let l = lp(p);
    loop {
        if !pending(p) {
            break;
        }
        let r = (*p).stream.as_ref().unwrap().write(&(*p).tbuf.as_ref().unwrap()[(*p).t_at..(*p).t_end]);
        match r {
            Ok(0) => return disconnect(p, Event::REMOTE_CLOSE_ABRUPTLY),
            Ok(n) => {
                (*p).t_at += n;
                (*p).ext.carry(n);
                if EDGE_SHORT && pending(p) {
                    break; // less than given: the socket is full, its edge says when it takes more
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return disconnect(p, Event::REMOTE_CLOSE_ABRUPTLY),
        }
    }
    (*p).ext.last_tx_ms.store(l.millis(), Ordering::Relaxed);
    if pending(p) {
        if (*p).writing {
            return;
        }
        (*p).writing = true;
        (*p).ext.writing.store(true, Ordering::Relaxed); // for the watch of the client's link ( TCP::Redial::stalled )
        interest(p, true);
        let t = (*p).ext.transmit_timeout_ms.load(Ordering::Relaxed);
        if 0 < t && t < i32::MAX as i64 {
            l.arm(p, T, l.nanos() + t * 1_000_000);
        }
        return;
    }
    if (*p).writing {
        (*p).writing = false;
        (*p).ext.writing.store(false, Ordering::Relaxed);
        (*p).dl[T].armed = false;
        interest(p, false);
    }
}

/// What the poller watches the socket for: readability always, writability while a write is parked.
/// A syscall per parked write - never per message of a socket that takes what it is given.
unsafe fn interest(p: *mut Slot, writable: bool) {
    if EDGE_SHORT {
        return; // both from the start ([`Loop::attach`]): a writability edge comes only after the socket was full
    }
    let l = lp(p);
    if let Some(s) = (*p).stream.as_mut() {
        let i = if writable { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
        if (*l.st()).poll.registry().reregister(s, Token((*p).idx as usize), i).is_err() {
            disconnect(p, Event::REMOTE_CLOSE_ABRUPTLY);
        }
    }
}

/// A graceful close is asked for: a negative timeout (`LastTransmission`), or
/// [`ExternalConnection::close`] - both set the flag.
#[inline]
pub(crate) unsafe fn closing(p: *mut Slot) -> bool {
    (*p).closing || (*p).ext.closing_gracefully.load(Ordering::Relaxed)
}

/// The source is empty: a scheduled graceful close goes on - the FIN, or the WebSocket CLOSE frame.
unsafe fn on_drained(p: *mut Slot) {
    if (*p).ws.is_some() {
        return super::TCP::web_socket::on_drained(p);
    }
    if closing(p) {
        shutdown_output(p);
    }
}

/// The FIN: nothing more is sent. Reading goes on until the peer's FIN, bounded by the linger.
pub(crate) unsafe fn shutdown_output(p: *mut Slot) {
    (*p).closing = true;
    if (*p).fin_sent || !active(p) {
        return;
    }
    (*p).fin_sent = true;
    if let Some(s) = (*p).stream.as_ref() {
        if !(*p).link.is_null() {
            (*(*p).link).closing(s);
        }
        if let Err(e) = s.shutdown(std::net::Shutdown::Write) {
            (*p).ext.failure(&e); // Report the shutdown failure before aborting.
            return disconnect(p, Event::THIS_CLOSE_ABRUPTLY);
        }
    }
    if (*p).reading {
        wait_for_data(p); // the wait for the peer's FIN, bounded by the linger
    }
}

/// [`ExternalConnection::close`] on the loop: what is queued drains, then the FIN (the CLOSE
/// frame).
pub(crate) unsafe fn grace(p: *mut Slot) {
    if !active(p) {
        return;
    }
    (*p).closing = true; // a WebSocket too: what is queued drains, then its CLOSE (web_socket::on_drained)
    if !(*p).running {
        wake_on_loop(p);
    }
}

// deadlines

/// The receive deadline ran out. A timeout is a question before it is a verdict: the application is
/// asked how long it still waits, and only a close answer - or no one to ask - closes the
/// connection.
unsafe fn timed_out_read(p: *mut Slot) {
    if !active(p) {
        return;
    }
    if (*p).handshake_due != 0 {
        (*p).handshake_due = 0;
        if !handshaking(p) {
            return wait_for_data(p);
        }
        (*p).ext.failure(&io::Error::new(io::ErrorKind::TimedOut, "The handshake did not end in time: the connection closes."));
        return disconnect(p, if (*p).ws.is_some() { Event::WEBSOCKET_HANDSHAKE_FAILURE } else { Event::RECEIVE_TIMEOUT });
    }
    if (*p).report.is_some() {
        // the session did not start in time (a WebSocket's 101): whoever waits for the connect
        // hears it
        report(p, Err(io::Error::new(io::ErrorKind::TimedOut, "the session did not start in time")));
        return disconnect(p, Event::THIS_CLOSE_ABRUPTLY);
    }
    if closing(p) {
        return disconnect(p, Event::REMOTE_CLOSE_ABRUPTLY); // the peer ignored our FIN
    }
    let answer = match (*p).ext.timeout_handler() {
        Some(h) => h(TimeoutKind::Receive),
        None => TimeoutAnswer::Close,
    };
    if !active(p) {
        return; // the handler ended it
    }
    if closing(p) {
        return wait_for_data(p); // the handler began a graceful close: from here it is the wait for the peer's FIN
    }
    match answer {
        TimeoutAnswer::Extend(ms) if 0 < ms => {
            if ms < i32::MAX as i64 {
                let l = lp(p);
                l.arm(p, R, l.nanos() + ms * 1_000_000);
            }
        }
        TimeoutAnswer::Disarm => {}
        _ => disconnect(p, if (*p).ws.is_some() { Event::WEBSOCKET_RECEIVE_TIMEOUT } else { Event::RECEIVE_TIMEOUT }),
    }
}

/// The transmit deadline ran out: the peer has not taken what the socket holds. The same question.
unsafe fn timed_out_write(p: *mut Slot) {
    if !active(p) {
        return;
    }
    let ev = if (*p).ws.is_some() { Event::WEBSOCKET_TRANSMIT_TIMEOUT } else { Event::TRANSMIT_TIMEOUT };
    match (*p).ext.timeout_handler() {
        Some(h) => match h(TimeoutKind::Transmit) {
            TimeoutAnswer::Extend(ms) if 0 < ms => {
                if active(p) && (*p).writing && ms < i32::MAX as i64 {
                    let l = lp(p);
                    l.arm(p, T, l.nanos() + ms * 1_000_000);
                }
            }
            TimeoutAnswer::Disarm => {}
            _ => disconnect(p, ev),
        },
        None => disconnect(p, ev),
    }
}

/// The nearest deadline of the actor instances is due: they fire, and the connection is filed under
/// the nearest of the rest.
unsafe fn timed_out_deadline(p: *mut Slot) {
    if !active(p) {
        return;
    }
    let l = lp(p);
    (*p).ext.fire_deadline(l.millis().max((*p).ext.deadline_at.load(Ordering::Acquire)));
    arm_deadline_from_state(p);
}

/// Files the connection under the deadline its state holds, when it is nearer than the one it waits
/// for.
pub(crate) unsafe fn arm_deadline_from_state(p: *mut Slot) {
    if !active(p) {
        return;
    }
    let at = (*p).ext.deadline_at.load(Ordering::Acquire);
    if at == 0 {
        return;
    }
    let l = lp(p);
    let due = l.nanos() + (at - l.millis()).max(0) * 1_000_000;
    if !(*p).dl[A].armed || due < (*p).dl[A].due {
        l.arm(p, A, due);
    }
}

// teardown

/// Runs application code for connection `p`, catching panics at the connection boundary.
///
/// Reports a panic to the host failure handler and keeps servicing other connections. Source and
/// sink panics are caught closer to their calls by `transmit` and `receive`. A decoder that loses
/// its parse position marks the stream broken so normal teardown can close it.
#[inline]
pub(crate) unsafe fn guarded(p: *mut Slot, f: impl FnOnce()) {
    if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        panicked(p, &*why);
    }
}

#[cold]
#[inline(never)]
unsafe fn panicked(p: *mut Slot, why: &(dyn std::any::Any + Send)) {
    (*p).ext.failure(&super::failure::panicked(why)); // Report the callback panic to the host.
}

/// Runs code of the application in the teardown of `p`: a panic out of it goes to the handler of
/// its host and the teardown goes on - the connection is going anyway.
#[inline]
unsafe fn quiet_slot(p: *mut Slot, f: impl FnOnce()) {
    if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        (*p).ext.failure(&super::failure::panicked(&*why));
    }
}

/// Claims the teardown - exactly one caller wins, whichever thread - and runs it on the loop.
pub(crate) unsafe fn disconnect(p: *mut Slot, event: Event) {
    if (*p).ext.claim_close(event) {
        close_on_loop(p);
    }
}

/// The teardown, on the loop. Nothing of the connection runs after it: every operation is switched
/// off here with its flags. The slot itself, its buffers, its sink and source go on the next pass
/// ([`reap`]) - whoever is up the stack still holds them.
pub(crate) unsafe fn close_on_loop(p: *mut Slot) {
    if (*p).dead {
        return;
    }
    (*p).dead = true;
    let l = lp(p);
    let event = (*p).ext.close_event();
    let graceful = closing(p) && !(*p).ext.grace_undone.swap(false, Ordering::Relaxed);
    quiet_slot(p, || {
        if !(*p).ext.loss(event, graceful) {
            (*p).ext.fire(event); // the closing event says the session is finished - a parked one is not, it waits for the reap
        }
    });
    quiet_slot(p, || (*p).ext.session_closed()); // OnClose: this connection of the session is over, parked or not
    for d in (*p).dl.iter_mut() {
        d.armed = false;
    }
    if let Some(s) = (*p).stream.as_mut() {
        if !EDGE_SHORT {
            let _ = (*l.st()).poll.registry().deregister(s); // epoll drops the socket itself when it closes: one call less a close
        }
        if !(*p).fin_sent {
            if !(*p).link.is_null() && !pending(p) { // a record half out: anything behind it would break TLS
                (*(*p).link).closing(s);
            }
            let _ = s.shutdown(std::net::Shutdown::Write); // FIN, so the peer sees a close rather than a reset
        }
    }
    if (*p).report.is_some() {
        let why = if (*p).connecting { "the connection closed before it connected" } else { "the connection closed before its session started" };
        (*p).connecting = false;
        quiet_slot(p, || report(p, Err(io::Error::new(io::ErrorKind::ConnectionAborted, why))));
    }
    (*p).reading = false;
    (*p).writing = false;
    (*p).ext.writing.store(false, Ordering::Relaxed);
    (*p).running = false;
    (*p).wake_pending = false;
    // the timeouts were this socket's: a session that goes on on the slot - a reconnect - sets its
    // own
    (*p).ext.receive_timeout_ms.store(i32::MAX as i64, Ordering::Relaxed);
    (*p).ext.transmit_timeout_ms.store(i32::MAX as i64, Ordering::Relaxed);
    (*p).ext.pulse_ms.store(0, Ordering::Relaxed);
    (*p).ext.closing_gracefully.store(false, Ordering::Relaxed);
    (*p).ext.socket_gone();
    if let Some(g) = (*p).on_gone.take() {
        g();
    }
    l.later(task(p, REAP));
}

/// The slot goes: the socket closes, the buffers go, the sink and the source are dropped.
unsafe fn reap(p: *mut Slot) {
    let l = lp(p);
    drop((*p).stream.take());
    if !(*p).link.is_null() {
        (*(*p).link).close(p);
        l.give_link(std::mem::replace(&mut (*p).link, std::ptr::null_mut()));
    }
    if !(*p).rb.is_null() {
        drop(Box::from_raw(std::mem::replace(&mut (*p).rb, std::ptr::null_mut())));
    }
    if let Some(b) = (*p).tbuf.take() {
        l.give_buf(b);
    }
    if let Some(ws) = (*p).ws.take() {
        ws.dispose(l);
    }
    if let Some(mut s) = (*p).sink.take() {
        if (*p).ext.resumable.load(Ordering::Acquire) != 0 {
            quiet_slot(p, || s.suspend()); // the session parks: the guaranteed pack in progress goes to it, with the parse
        }
        drop(s);
    }
    drop((*p).source.take());
    let ext = (*p).ext.clone();
    if !ext.of_host() {
        for k in 0..3 {
            if (*p).dl[k].linked {
                l.unfile(p, k); // the slot goes: its index may serve another connection before the key comes up
            }
        }
        l.free_slot(p);
    } // else the place stays the slot's: its waits come up and are passed by while it is dead
    ext.finish_dispose(); // the transport is done with the connection: its host takes the slot back, or parks its session
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    /// A loop asleep wakes for work from another thread, every time.
    #[test]
    fn an_idle_loop_wakes_for_work_from_another_thread() {
        let engine = Engine::new("idle", 1, MIN_BUFFER);
        let lp = engine.loops[0].clone();
        for i in 0..300u64 {
            std::thread::sleep(Duration::from_millis(1 + i % 7)); // asleep by now, or nearly
            let (tx, rx) = std::sync::mpsc::channel();
            lp.submit_fn(Box::new(move |_| {
                let _ = tx.send(());
            }));
            assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok(), "round {i}: the loop did not wake for a function");
        }
        engine.close();
    }
}
