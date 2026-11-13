// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Reliable datagram transport with acknowledgement, replay, and congestion control.
//!
//! [`Engine`] runs I/O loops owning [`Endpoint`] sockets and [`Peer`] connection slots. Each
//! connection stays on one loop; other threads submit atomic request flags. Intrusive deadline
//! lists schedule retransmission and maintenance.
//!
//! Sources serialize into outgoing datagrams and sinks consume incoming payloads. The [`Resumable`]
//! journal retains guaranteed bytes until acknowledged and supplies replay after loss. Opening a
//! connection uses an initial datagram, address-validation retry, and accept response. The Noise the
//! protocol layer declares as it is seated (NK, IK or KK) rides the INITIAL and the ACCEPT; then every
//! datagram is sealed in place with the transport keys of the handshake, ChaCha20-Poly1305: its header
//! clear and authenticated, the rest encrypted, the tag behind it; one without data has its counter
//! behind the tag, clear - the nonce is made of it.
//!
//! RTT estimation follows RFC 6298; probe timeout follows RFC 9002. NewReno congestion control uses
//! pacing. Applications interact through [`ExternalConnection`] and connection factories whose
//! callbacks run on the assigned loop.

#![allow(dangerous_implicit_autorefs)] // the connections and the state of a loop are reached through raw pointers on purpose: a handler
                                       // up the stack may hold them

use std::cell::UnsafeCell;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token, Waker};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use sha2::Sha256;

use super::TCP::external_connection::Event;
use super::TCP::external_connection::{now_ms, now_sec, ConnId, ExternalConnection, TimeoutAnswer, TimeoutKind};
use super::TCP::ConnectionFactory;
use crate::lib::adhoc::connection::transmitter::resumable::{Journal, Resumable};
use crate::lib::adhoc::{BytesDst, BytesSrc};
use crate::lib::chacha20::{Aead, TAG};
use crate::lib::chain::Chain;
use crate::lib::noise::{NoiseNK, KEY};

// The wire

/// The top bit of the first byte: a datagram of the handshake.
pub const LONG: u8 = 0x80;
/// The version: the second byte of every datagram of the handshake.
pub const VERSION: u8 = 2;
pub const INITIAL: u8 = 0;
pub const RETRY: u8 = 1;
pub const ACCEPT: u8 = 2;

// A datagram of a connection: its first byte is the flags, the top bit clear. DATA bytes of the
// stream follow the header QUESTION the peer is to answer at once REPORT how many bytes of the
// journal arrived, the low 32 bits CONTINUE with REPORT: a new series is asked for - the position,
// whole, and the series LOST the journal does not hold the position asked for: the position the
// stream goes on from, whole CLOSE the connection closes, the code follows ANSWER the answer to a
// QUESTION
pub const DATA: u8 = 0x01;
pub const QUESTION: u8 = 0x02;
pub const REPORT: u8 = 0x04;
pub const CONTINUE: u8 = 0x08;
pub const LOST: u8 = 0x10;
pub const CLOSE: u8 = 0x20;
pub const ANSWER: u8 = 0x40;

pub const MIN_INITIAL: usize = 1200;
pub const TOKEN: usize = 16;
pub const TOKEN_SECONDS: u32 = 10;
/// The flags, the connection, the series, the number.
pub const HEADER: usize = 1 + 4 + 1 + 3;
/// The smallest datagram a connection fills: a header with all it says and some data.
pub const MTU_MIN: usize = HEADER + 9 + 8 + TAG + 64;
/// The largest datagram there is: the UDP payload over IPv4 (65 535 - the IP header 20 - the UDP
/// header 8). A larger one is never sent - every send of it would fail, and the connection stall.
pub const MTU_MAX: usize = 65_507;

/// The longest message of the handshakes of a connection: the first of IK - the ephemeral key, the
/// static key sealed, the tag of the payload.
const HS_MAX: usize = KEY + KEY + TAG + TAG;

/// The low bits of a connection id are the slot of the connection in its loop, the high ones are
/// random.
pub const SLOT_BITS: u32 = 20;
pub const SLOT_MASK: u32 = (1 << SLOT_BITS) - 1;

/// What the Noise handshake of a connection is bound to: the protocol, its version, the client's
/// connection id.
fn prologue(client_cid: u32) -> [u8; 14] {
    let mut p = [0u8; 14];
    p[..9].copy_from_slice(b"AdHoc UDP");
    p[9] = VERSION;
    p[10..14].copy_from_slice(&client_cid.to_le_bytes());
    p
}

/// The prologue of an INITIAL that names its connection on a multiplexed port ( `named` - the uid
/// of the host that calls ): the uid follows, so the handshake does not hold for another
/// connection. `None` - not named.
fn prologue_named(client_cid: u32, named: Option<u8>) -> Vec<u8> {
    let mut p = prologue(client_cid).to_vec();
    if let Some(uid) = named {
        p.push(uid);
    }
    p
}

#[inline]
fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
#[inline]
fn put32(b: &mut [u8], i: usize, v: u32) {
    b[i..i + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
fn le64(b: &[u8], i: usize) -> i64 {
    i64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
#[inline]
fn put64(b: &mut [u8], i: usize, v: i64) {
    b[i..i + 8].copy_from_slice(&v.to_le_bytes());
}

/// A fresh connection id for `slot`: the slot in the low bits, random ones above.
fn cid(slot: u32) -> u32 {
    let mut hi = [0u8; 4];
    let _ = getrandom::fill(&mut hi);
    slot | u32::from_le_bytes(hi) << SLOT_BITS
}

// Time

/// The clock: nanoseconds ([`super::engine::clock`]); ticks per millisecond.
const TICKS_MS: i64 = 1_000_000;

/// Deadlines are filed rounded up to a beat - the largest power of two of clock ticks not over a
/// millisecond.
const BEAT: i64 = 1 << 19;

#[inline]
fn beat(due: i64) -> i64 {
    (due | (BEAT - 1)) + 1
}

// States, deadline kinds, requests

pub const FREE: u8 = 0;
pub const CONNECTING: u8 = 1;
pub const ACTIVE: u8 = 2;
pub const CLOSING: u8 = 3;
pub const PARKED: u8 = 4;

/// The kinds of wait: nothing received, nothing sent, the handshake, the close, the probe, the
/// parked session, the pacing, the series asked for, the deadline of the actor instances.
const RECEIVE: usize = 0;
const KEEPALIVE: usize = 1;
const HANDSHAKE: usize = 2;
const LINGER: usize = 3;
const PROBE: usize = 4;
const PARK: usize = 5;
const PACE: usize = 6;
const REQUEST: usize = 7;
const DEADLINE: usize = 8;
const KINDS: usize = 9;

pub(crate) const CLOSE_: u32 = 1;
pub(crate) const ABORT_: u32 = 2;
pub(crate) const TX_: u32 = 8;
pub(crate) const CONTINUE_: u32 = 16;
pub(crate) const DEADLINE_: u32 = 32;
/// The receive timeout changed: the wait for data is filed anew from when the connection heard
/// last.
pub(crate) const TIMEOUT_: u32 = 64;
/// The keepalive of the connection changed ([`ExternalConnection::pulse`]): the next one is filed
/// anew from now.
pub(crate) const PULSE_: u32 = 128;
/// A receive buffer: the largest datagram there is.
const RX_BUF: usize = 1 << 16;
/// The receive buffers the pool of a loop keeps; more go back to the allocator.
const POOL_KEEP: usize = 8;

/// The sink that did not take everything has room again (`wake_receive`): what it holds is offered
/// to it again.
pub(crate) const RX_: u32 = 4;

/// What a client hears of its connect, on the thread of the connection's loop: the state once the
/// server accepted, or what went wrong - `TimedOut` when no answer came in time.
pub type OnConnected = Box<dyn FnOnce(io::Result<Arc<ExternalConnection>>) + Send>;

type Task = Box<dyn FnOnce(&'static Loop) + Send>;

// Engine

/// The threads of the UDP transport: loops. A connection lives on one loop for its life. One engine
/// serves any number of hosts. The loops are never freed: a state the application still holds
/// points into them.
pub struct Engine {
    loops: Box<[&'static Loop]>,
    next: AtomicUsize,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Where the failures of its loops go, and of its hosts given no handler.
    on_failure: Arc<OnceLock<super::failure::OnFailure>>,
}

impl Engine {
    /// `threads` loops, capped at the processor count. A listening port is read by one loop, except
    /// on Linux: there every loop reads a socket of its own on the port (`SO_REUSEPORT`), and the
    /// kernel keeps each peer on one of them.
    pub fn new(name: &str, threads: usize) -> io::Result<Arc<Self>> {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let n = threads.clamp(1, cpus);
        let mut loops = Vec::with_capacity(n);
        let mut handles = Vec::with_capacity(n);
        let on_failure = Arc::new(OnceLock::new());
        for i in 0..n {
            let lp: &'static Loop = Box::leak(Box::new(Loop::new(i, on_failure.clone())?));
            loops.push(lp);
            handles.push(std::thread::Builder::new().name(format!("{name}-udp-{i}")).spawn(move || lp.run())?);
        }
        Ok(Arc::new(Self {
            loops: loops.into_boxed_slice(),
            next: AtomicUsize::new(0),
            threads: Mutex::new(handles),
            on_failure,
        }))
    }

    /// As [`new`](Self::new), with where the failures of its loops and of its hosts given none go:
    /// set before any host is built on it.
    pub fn with_failure(name: &str, threads: usize, on_failure: super::failure::OnFailure) -> io::Result<Arc<Self>> {
        let engine = Self::new(name, threads)?;
        engine.set_on_failure(on_failure);
        Ok(engine)
    }

    /// Where the failures of the loops of this engine go, and of its hosts given none;
    /// [`super::failure::print_console`] until it is set. Once, before the first host.
    pub fn set_on_failure(&self, f: super::failure::OnFailure) {
        let _ = self.on_failure.set(f);
    }

    /// The handler of this engine.
    pub(crate) fn on_failure(&self) -> super::failure::OnFailure {
        self.on_failure.get().cloned().unwrap_or_else(super::failure::print_console)
    }

    /// The engine of the process: a loop per processor.
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<Engine>> = OnceLock::new();
        SHARED.get_or_init(|| Engine::new("adhoc", usize::MAX).expect("the loops of the UDP engine")).clone()
    }

    pub fn threads(&self) -> usize {
        self.loops.len()
    }

    /// Round-robin, a plain load and store: a race hands two connections to one loop, which is
    /// harmless.
    fn pick(&self) -> &'static Loop {
        let i = self.next.load(Ordering::Relaxed);
        self.next.store(if i + 1 == self.loops.len() { 0 } else { i + 1 }, Ordering::Relaxed);
        self.loops[i]
    }

    /// Stops the loops. What is still connected is dropped without a word.
    pub fn close(&self) {
        for lp in self.loops.iter() {
            lp.running.store(false, Ordering::Release);
            let _ = lp.waker.wake();
        }
        let me = std::thread::current().id();
        for h in self.threads.lock().drain(..) {
            if h.thread().id() != me {
                let _ = h.join();
            }
        }
    }
}

// Endpoint

/// One socket on one loop, and the host it serves.
pub(crate) struct Endpoint {
    socket: UdpSocket,
    host: Arc<HostInner>,
    listening: bool,
    /// Listening: the accepted connections by the address and the connection id of their peer, for
    /// a repeated INITIAL.
    accepted: FxHashMap<(SocketAddr, u32), *mut Peer>,
    /// The connections that wait for the socket to send its held datagram: they go on once it is
    /// out.
    blocked: Vec<*mut Peer>,
    /// The datagram being made for this socket, `mtu` bytes (an INITIAL at least). Of this socket,
    /// not of the loop: a socket that takes nothing holds back only the connections on it.
    tx: Box<[u8]>,
    /// The first `held` bytes of `tx` are a datagram the socket would not take (0 - none), for
    /// `held_to`: it goes as it is, first, once the socket takes more - its number spent and not
    /// sent would be a gap, the series broken. Nothing is made for this socket until then.
    held: usize,
    held_to: SocketAddr,
    /// An IPv6 socket that serves IPv4 too (dual stack): an IPv4 address goes out mapped
    /// (`::ffff:a.b.c.d`).
    v6: bool,
    /// The queue of the sockets the poller said have datagrams: this one is on it, and the next.
    queued: bool,
    next_readable: *mut Endpoint,
}

impl Endpoint {
    fn new(socket: UdpSocket, host: Arc<HostInner>, listening: bool) -> Box<Self> {
        let tx = vec![0u8; host.mtu.max(MIN_INITIAL)].into_boxed_slice();
        let v6 = socket.local_addr().is_ok_and(|a| a.is_ipv6());
        Box::new(Endpoint {
            socket,
            host,
            listening,
            accepted: FxHashMap::default(),
            blocked: Vec::new(),
            tx,
            held: 0,
            held_to: SocketAddr::from(([0, 0, 0, 0], 0)),
            v6,
            queued: false,
            next_readable: null_mut(),
        })
    }
}

// SAFETY: an endpoint is made on one thread and handed to its loop once; from there only the loop
// touches it.
unsafe impl Send for Endpoint {}

// Loop

const WAKER: Token = Token(usize::MAX);

#[derive(Clone, Copy)]
struct List {
    head: *mut Peer,
    tail: *mut Peer,
    finger: *mut Peer,
}

/// What only the thread of the loop touches.
struct St {
    poll: Poll,
    events: Events,
    endpoints: Vec<Box<Endpoint>>,
    /// Every connection this loop ever made, by slot: what the low bits of a connection id point
    /// at.
    slots: Vec<*mut Peer>,
    /// The datagram that came, the largest there is: a smaller buffer cuts a larger datagram
    /// (Linux) or loses it (Windows). Only received into: a datagram is made in the buffer of its
    /// socket (`Endpoint::tx`).
    buf: Box<[u8]>,
    /// Receive buffers to take the place of one a connection keeps ([`keep`]): a few, they come
    /// back once its sink has taken what it kept.
    pool: Vec<Box<[u8]>>,
    dl: [List; KINDS],
    now: i64,
    stale: bool,
    /// The sender of the datagram in hand.
    from: SocketAddr,
    /// The sockets the poller said have datagrams, in turn: only they are read - not every socket
    /// of the loop every pass -, a burst each; one left with more goes behind the others.
    readable: *mut Endpoint,
    readable_tail: *mut Endpoint,
    readable_n: usize,
}

/// One thread: its sockets, the connections on them, their deadlines. Each pass: sleep until the
/// earliest deadline or a datagram or a request, read what the sockets hold, serve the requests and
/// the tasks, time out what is due.
pub(crate) struct Loop {
    index: usize,
    /// The handler of its engine: the failures of the loop itself.
    on_failure: Arc<OnceLock<super::failure::OnFailure>>,
    /// The slots of [`St::slots`], as addresses, for another loop to look a parked session up by
    /// its token. Written only when a slot is made.
    shared: parking_lot::RwLock<Vec<usize>>,
    running: AtomicBool,
    waker: Waker,
    /// Connections with requests, a Treiber stack threaded through [`Peer::next_request`]: a
    /// connection is on it at most once - it is pushed by whoever raises its requests from none -
    /// so it allocates nothing.
    requested: AtomicPtr<Peer>,
    /// The rare things: a bind, a connect, a shutdown - not per message.
    tasks: Mutex<Vec<Task>>,
    has_tasks: AtomicBool,
    /// Raised right before the loop sleeps. Each side writes first and reads second, through full
    /// barriers: either the loop sees the work and does not sleep, or the producer sees the flag
    /// and wakes it.
    asleep: AtomicI32,
    thread: OnceLock<std::thread::ThreadId>,
    st: UnsafeCell<St>,
}

// SAFETY: `st` is touched by the thread of the loop only; the rest are atomics and locks.
unsafe impl Sync for Loop {}
unsafe impl Send for Loop {}

impl Loop {
    fn new(index: usize, on_failure: Arc<OnceLock<super::failure::OnFailure>>) -> io::Result<Self> {
        let poll = Poll::new()?;
        let waker = Waker::new(poll.registry(), WAKER)?;
        let empty = List {
            head: null_mut(),
            tail: null_mut(),
            finger: null_mut(),
        };
        Ok(Self {
            index,
            on_failure,
            shared: parking_lot::RwLock::new(Vec::new()),
            running: AtomicBool::new(true),
            waker,
            requested: AtomicPtr::new(null_mut()),
            tasks: Mutex::new(Vec::new()),
            has_tasks: AtomicBool::new(false),
            asleep: AtomicI32::new(0),
            thread: OnceLock::new(),
            st: UnsafeCell::new(St {
                poll,
                events: Events::with_capacity(256),
                endpoints: Vec::new(),
                slots: Vec::new(),
                buf: vec![0u8; RX_BUF].into_boxed_slice(),
                pool: Vec::new(),
                dl: [empty; KINDS],
                now: 0,
                stale: true,
                from: SocketAddr::from(([0, 0, 0, 0], 0)),
                readable: null_mut(),
                readable_tail: null_mut(),
                readable_n: 0,
            }),
        })
    }

    /// SAFETY of the lifetime: a loop is leaked at its creation - it lives as long as the process.
    #[inline]
    fn me(&self) -> &'static Loop {
        unsafe { &*(self as *const Loop) }
    }

    #[inline]
    fn st(&self) -> *mut St {
        self.st.get()
    }

    fn in_loop(&self) -> bool {
        self.thread.get() == Some(&std::thread::current().id())
    }

    /// The clock, read once per pass: everything served in a pass stamps and arms from the same
    /// reading.
    #[inline]
    unsafe fn now(&self) -> i64 {
        let st = self.st();
        if (*st).stale {
            (*st).now = super::engine::clock();
            (*st).stale = false;
        }
        (*st).now
    }

    // the doors in

    fn post(&self, p: *mut Peer) {
        let mut h = self.requested.load(Ordering::Relaxed);
        loop {
            // SAFETY: the connection is on no stack: only the one who raised its requests from none
            // pushes it
            unsafe { *(*p).next_request.get() = h };
            match self.requested.compare_exchange_weak(h, p, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(_) => break,
                Err(x) => h = x,
            }
        }
        if self.asleep.load(Ordering::SeqCst) != 0 {
            let _ = self.waker.wake();
        }
    }

    /// Runs `task` on this loop.
    fn submit(&self, task: Task) {
        self.tasks.lock().push(task);
        self.has_tasks.store(true, Ordering::SeqCst);
        if self.asleep.load(Ordering::SeqCst) != 0 {
            let _ = self.waker.wake();
        }
    }

    /// Runs `task` on this loop and waits for it.
    fn call<R: Send + 'static>(&'static self, task: impl FnOnce(&'static Loop) -> R + Send + 'static) -> R {
        if self.in_loop() {
            return task(self);
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.submit(Box::new(move |lp| {
            let _ = tx.send(task(lp));
        }));
        rx.recv().expect("the loop is gone")
    }

    #[inline]
    fn idle(&self) -> bool {
        self.requested.load(Ordering::SeqCst).is_null() && !self.has_tasks.load(Ordering::SeqCst)
    }

    fn run(&'static self) {
        let _ = self.thread.set(std::thread::current().id());
        let st = self.st();
        while self.running.load(Ordering::Acquire) {
            // SAFETY: the thread of the loop
            unsafe {
                let mut sleep = (*st).readable.is_null() && self.idle();
                if sleep {
                    self.asleep.swap(1, Ordering::SeqCst); // written first...
                    sleep = (*st).readable.is_null() && self.idle(); // ...read second
                }
                // the poller every pass: what is readable, what takes its held datagram - one call,
                // not a read of every socket
                let timeout = if !sleep {
                    Some(Duration::ZERO)
                } else {
                    let at = self.next_deadline();
                    if at == i64::MAX {
                        None
                    } else {
                        let t = at - super::engine::clock();
                        Some(if t <= 0 { Duration::ZERO } else { Duration::from_millis(((t + TICKS_MS - 1) / TICKS_MS) as u64) })
                    }
                };
                let st_ = &mut *st;
                let _ = st_.poll.poll(&mut st_.events, timeout);
                for e in st_.events.iter() {
                    if e.token() == WAKER {
                        continue;
                    }
                    let ep = e.token().0 as *mut Endpoint;
                    if e.is_writable() {
                        self.writable(ep);
                    }
                    if e.is_readable() || e.is_error() {
                        self.readable(ep); // an error too: the read takes it (an ICMP of an earlier send)
                    }
                }
                if self.asleep.load(Ordering::Relaxed) != 0 {
                    self.asleep.store(0, Ordering::Relaxed);
                }
                (*st).stale = true;
                self.receive();
                self.serve();
                self.expire();
            }
        }
        // SAFETY: the thread of the loop, at its end
        unsafe {
            for &p in (*st).slots.iter() {
                (*p).sink = None;
                (*p).source = None;
            }
            (*st).endpoints.clear(); // the sockets close
        }
    }

    /// A failure of the loop itself, to the handler of its engine.
    #[cold]
    fn failure(&self, e: &(dyn std::error::Error + 'static)) {
        let name = std::thread::current().name().unwrap_or("a loop").to_string();
        match self.on_failure.get() {
            // a handler that panics does not take the loop with it: its own failure is printed, as TCP's
            Some(f) => {
                if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&name, e))) {
                    (super::failure::print_console())(&name, &super::failure::panicked(&*why));
                }
            }
            None => (super::failure::print_console())(&name, e),
        }
    }

    /// Runs code no connection answers for: a panic out of it goes to the handler of the engine and
    /// the loop goes on.
    #[inline]
    fn quietly(&self, f: impl FnOnce()) {
        if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            self.failure(&super::failure::panicked(&*why));
        }
    }

    unsafe fn next_deadline(&self) -> i64 {
        let mut at = i64::MAX;
        for k in 0..KINDS {
            at = at.min(self.dl_next(k));
        }
        at
    }

    unsafe fn expire(&self) {
        let t = self.now();
        for k in 0..KINDS {
            self.dl_expire(k, t);
        }
    }

    unsafe fn serve(&self) {
        if self.has_tasks.load(Ordering::Acquire) && self.has_tasks.swap(false, Ordering::SeqCst) {
            let tasks = std::mem::take(&mut *self.tasks.lock());
            let me = self.me();
            for t in tasks {
                self.quietly(|| t(me)); // Report loop failure through the engine handler.
            }
        }
        let mut p = self.requested.swap(null_mut(), Ordering::SeqCst);
        while !p.is_null() {
            let next = *(*p).next_request.get();
            *(*p).next_request.get() = null_mut();
            let what = (*p).requests.swap(0, Ordering::AcqRel); // from here a new request pushes it again
            guarded(p, || serve(p, what)); // Report peer failure through the host handler.
            p = next;
        }
    }

    // receiving

    /// `ep` behind the sockets to read: once on the queue at most.
    #[inline]
    unsafe fn readable(&self, ep: *mut Endpoint) {
        if (*ep).queued {
            return;
        }
        let st = self.st();
        (*ep).queued = true;
        (*ep).next_readable = null_mut();
        if (*st).readable_tail.is_null() {
            (*st).readable = ep;
        } else {
            (*(*st).readable_tail).next_readable = ep;
        }
        (*st).readable_tail = ep;
        (*st).readable_n += 1;
    }

    /// Reads the sockets the poller said have datagrams, a bounded burst each: one left with more
    /// goes behind the others, read on the next pass (the poller tells an edge once: it is read
    /// until it would block).
    unsafe fn receive(&self) {
        let st = self.st();
        for _ in 0..(*st).readable_n {
            let ep = (*st).readable;
            (*st).readable = (*ep).next_readable;
            if (*st).readable.is_null() {
                (*st).readable_tail = null_mut();
            }
            (*ep).next_readable = null_mut();
            (*ep).queued = false;
            (*st).readable_n -= 1;
            let mut burst = 0;
            loop {
                if burst == 256 {
                    self.readable(ep); // one burst a pass: the others first
                    break;
                }
                burst += 1;
                match (*ep).socket.recv_from(&mut (*st).buf) {
                    Ok((len, from)) => {
                        (*st).from = canonical(from); // a dual-stack socket names an IPv4 peer as::ffff:a.b.c.d
                        #[cfg(test)]
                        {
                            match (*ep).host.wire_hook.as_ref().map(|h| h(WireDir::Rx, &(&(*st).buf)[..len])) {
                                Some(WireAction::Drop) => continue,
                                Some(WireAction::Duplicate) => {
                                    let copy = (&(*st).buf)[..len].to_vec();
                                    self.dispatch(ep, len);
                                    (&mut (*st).buf)[..len].copy_from_slice(&copy);
                                }
                                _ => {}
                            }
                        }
                        self.dispatch(ep, len);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {} // an ICMP of an earlier send (Windows)
                    Err(_) => {
                        self.readable(ep); // read again next pass: an edge not read to the end is not told again
                        break;
                    }
                }
            }
        }
    }

    unsafe fn dispatch(&self, ep: *mut Endpoint, n: usize) {
        let st = self.st();
        if n < 2 {
            return;
        }
        let rx = &(*st).buf;
        let b0 = rx[0];
        if b0 & LONG == 0 {
            if n < HEADER {
                return;
            }
            let p = self.find(ep, le32(rx, 1));
            if !p.is_null() {
                let now = self.now();
                guarded(p, || incoming(p, now, n));
            }
            return;
        }
        if rx[1] != VERSION {
            return;
        }
        match b0 & !LONG {
            INITIAL => {
                if (*ep).listening && MIN_INITIAL <= n {
                    self.quietly(|| self.initial(ep, n)); // a panic here is no connection's yet: the loop and its connections go on
                }
            }
            RETRY => {
                if 22 <= n {
                    let p = self.find(ep, le32(rx, 2));
                    if !p.is_null() {
                        guarded(p, || retry(p));
                    }
                }
            }
            ACCEPT => {
                if 11 <= n {
                    let p = self.find(ep, le32(rx, 2));
                    if !p.is_null() {
                        let server = le32(rx, 6);
                        guarded(p, || accepted(p, server, n));
                    }
                }
            }
            _ => {}
        }
    }

    /// The connection a packet names, if it came through the right socket from the right address.
    #[inline]
    unsafe fn find(&self, ep: *mut Endpoint, cid: u32) -> *mut Peer {
        let st = self.st();
        let slot = (cid & SLOT_MASK) as usize;
        let Some(&p) = (*st).slots.get(slot) else { return null_mut() };
        let s = (*p).state;
        if (*p).cid == cid && (s == CONNECTING || s == ACTIVE || s == CLOSING) && (*p).ep == ep && (*p).address == Some((*st).from) {
            p
        } else {
            null_mut()
        }
    }

    /// An INITIAL on a listening socket: a RETRY for one without a valid token, a connection for
    /// one with it.
    unsafe fn initial(&self, ep: *mut Endpoint, n: usize) {
        let st = self.st();
        let host = (*ep).host.clone();
        let from = (*st).from;
        let rx = &(*st).buf;
        let scid = le32(rx, 2);
        if rx[6] as usize != TOKEN || !host.token_valid(&rx[7..7 + TOKEN], &from, scid) {
            if (*ep).held != 0 {
                return; // the socket holds a datagram back: no RETRY, the client sends its INITIAL again
            }
            let tx = &mut (*ep).tx;
            tx[0] = LONG | RETRY;
            tx[1] = VERSION;
            put32(tx, 2, scid);
            host.token_make(&mut tx[6..6 + TOKEN], &from, scid);
            self.send(ep, from, 22);
            return;
        }
        if let Some(&p) = (*ep).accepted.get(&(from, scid)) {
            if (*p).state == CONNECTING || (*p).state == ACTIVE {
                send_accept(p); // the first ACCEPT was lost
            }
            return;
        }
        if 0 < host.max_connections && host.max_connections as i64 <= host.live.load(Ordering::Relaxed) {
            return;
        }
        let hs_at = 7 + TOKEN;
        let hs_len = rx[hs_at] as usize;
        // A multiplexed port: the INITIAL names the connection - the uid of the host that calls -
        // after the handshake. One port is either: an unnamed INITIAL on a multiplexed one, a named
        // one on an ordinary one, gets nothing.
        let name_at = hs_at + 1 + hs_len;
        if n < name_at {
            return;
        }
        let named = if name_at + 1 < n && rx[name_at] == 1 { Some(rx[name_at + 1]) } else { None };
        let factory = match (&host.mux, named) {
            (Some(pick), Some(uid)) => match pick(uid) {
                Some(f) => f,
                None => {
                    return (host.on_failure)(
                        &host.name,
                        &io::Error::new(io::ErrorKind::InvalidData, format!("A multiplexed port has no connection for the host uid {uid}")),
                    )
                }
            },
            (None, None) => match host.on_connect.clone() {
                Some(f) => f,
                None => return,
            },
            _ => return,
        };
        let Some(p) = self.allocate(&host, ep) else {
            // no slot left: the INITIAL goes unanswered, and the host hears it every time - a flood
            // of them is an alarm
            return (host.on_failure)(&host.name, &io::Error::new(io::ErrorKind::OutOfMemory, "A loop holds 1 048 576 UDP connections at most."));
        };
        (*p).address = Some(from);
        (*p).rcid = scid;
        (*ep).accepted.insert((from, scid), p);
        (*p).ext.set_addresses((*ep).socket.local_addr().ok(), Some(from)); // the region of the keys sees where the connection is
        let (sink, source) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| factory(&(*p).ext))) {
            Ok(s) => s,
            Err(why) => {
                (*p).ext.failure(&super::failure::panicked(&*why)); // the connection is refused: no protocol layer for it
                free(p);
                return;
            }
        };
        (*p).sink = Some(sink);
        (*p).source = Some(source);
        // The handshake: the protocol layer seated declares the Noise of the connection, and the
        // client's first message is read by it - one not written for its keys, or not written at
        // all, gets nothing. A layer that declares none serves only clients that do not encrypt.
        match (*p).ext.take_secured() {
            Some(None) => {
                (*p).ext.failure(&super::TCP::refused()); // no keys for it
                return free(p);
            }
            Some(Some(c)) if 0 <= c.pattern => {
                let mut h = c.handshake(false, &prologue_named(scid, named));
                if hs_len == 0 || h.read_message(&rx[hs_at + 1..name_at], &mut []).is_err() || !keyed(p, h, &c) {
                    return free(p);
                }
            }
            _ => {
                if hs_len != 0 {
                    return free(p); // a client that encrypts
                }
            }
        }
        accept(p);
    }

    /// A connection slot of `host` on this loop: a free one of that host, or a new one; `None` -
    /// the loop holds all it can.
    unsafe fn allocate(&self, host: &Arc<HostInner>, ep: *mut Endpoint) -> Option<*mut Peer> {
        let st = self.st();
        let free = host.free[self.index].get();
        let p = if !(*free).is_null() {
            let p = *free;
            *free = (*p).next_free;
            p
        } else {
            let slot = (*st).slots.len() as u32;
            if SLOT_MASK < slot {
                return None; // A loop holds 1 048 576 UDP connections at most.
            }
            let ext = ExternalConnection::closed(ConnId((self.index as u32) << SLOT_BITS | slot)); // open once its session starts
            ext.set_on_failure(host.on_failure.clone()); // the slot serves this host for life
            let p = Box::into_raw(Box::new(Peer::new(self.me(), host.clone(), slot, ext)));
            let _ = (*p).ext.udp.set(Link(p));
            (*st).slots.push(p);
            self.shared.write().push(p as usize);
            p
        };
        (*p).next_free = null_mut();
        (*p).ep = ep;
        // this side made the connection: its socket is not a listening one. The journal tells the two
        // sides apart by it - see the order in the description of the module resumable
        (*p).ext.dialing.store(!(*ep).listening, Ordering::Release);
        (*p).cid = cid((*p).slot);
        Some(p)
    }

    unsafe fn release(&self, p: *mut Peer) {
        let free = (*p).host.free[self.index].get();
        (*p).next_free = *free;
        *free = p;
    }

    /// The first `len` bytes of the socket's `tx` go to `to`: false - the socket has no room for
    /// them now, and they stay there as its held datagram (a send takes a datagram whole or not at
    /// all), first out once it takes more. Any other failure is a datagram lost on this side, as
    /// one lost on the way.
    #[inline]
    unsafe fn send(&self, ep: *mut Endpoint, to: SocketAddr, len: usize) -> bool {
        let bytes = &(&(*ep).tx)[..len];
        #[cfg(test)]
        {
            match (*ep).host.wire_hook.as_ref().map(|h| h(WireDir::Tx, bytes)) {
                Some(WireAction::Drop) => return true,
                Some(WireAction::Duplicate) => {
                    let _ = (*ep).socket.send_to(bytes, wire_to(ep, to));
                }
                _ => {}
            }
        }
        match (*ep).socket.send_to(bytes, wire_to(ep, to)) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                (*ep).held = len;
                (*ep).held_to = to;
                self.interest(ep, Interest::READABLE | Interest::WRITABLE); // told when it takes more
                false
            }
            _ => true,
        }
    }

    /// The socket takes datagrams again: its held datagram goes first, then the connections that
    /// waited for it go on.
    unsafe fn writable(&self, ep: *mut Endpoint) {
        if (*ep).held == 0 {
            return;
        }
        if let Err(e) = (*ep).socket.send_to(&(&(*ep).tx)[..(*ep).held], wire_to(ep, (*ep).held_to)) {
            if e.kind() == io::ErrorKind::WouldBlock {
                return; // the room it had was less than the datagram: the next writable event
            }
        }
        (*ep).held = 0;
        self.interest(ep, Interest::READABLE);
        for p in std::mem::take(&mut (*ep).blocked) {
            if (*p).tx_blocked && (*p).ep == ep {
                (*p).tx_blocked = false;
                request(p, TX_);
            }
        }
    }

    /// What the poller tells of the socket of `ep`. The writability only while a datagram is held:
    /// registered for good, every datagram the socket sends would wake the loop as its room comes
    /// back.
    #[cold]
    unsafe fn interest(&self, ep: *mut Endpoint, i: Interest) {
        let _ = (*self.st()).poll.registry().reregister(&mut (*ep).socket, Token(ep as usize), i);
    }

    unsafe fn add(&self, ep: Box<Endpoint>) -> io::Result<()> {
        let st = self.st();
        let mut ep = ep;
        let token = Token(&mut *ep as *mut Endpoint as usize);
        (*st).poll.registry().register(&mut ep.socket, token, Interest::READABLE)?;
        (*st).endpoints.push(ep);
        Ok(())
    }

    unsafe fn remove_of(&self, host: &Arc<HostInner>) {
        let st = self.st();
        let st_ = &mut *st;
        // the sockets that go come off the queue of the readable first: their memory goes with them
        let mut p = std::mem::replace(&mut st_.readable, null_mut());
        st_.readable_tail = null_mut();
        st_.readable_n = 0;
        while !p.is_null() {
            let next = (*p).next_readable;
            (*p).next_readable = null_mut();
            (*p).queued = false;
            if !Arc::ptr_eq(&(*p).host, host) {
                self.readable(p);
            }
            p = next;
        }
        let registry = st_.poll.registry();
        st_.endpoints.retain_mut(|ep| {
            if Arc::ptr_eq(&ep.host, host) {
                let _ = registry.deregister(&mut ep.socket);
                false
            } else {
                true
            }
        });
    }

    // Intrusive deadline lists sorted by the deadline stored at insertion. A busy connection
    // updates its desired deadline without relinking immediately. When the saved deadline becomes
    // due, the loop reschedules a renewed wait or drops a cancelled one.

    #[inline]
    unsafe fn arm(&self, p: *mut Peer, k: usize, due: i64) {
        let d = &mut (*p).dl[k];
        d.due = due;
        d.armed = true;
        if d.linked {
            if d.at <= beat(due) {
                return; // comes up no later than this deadline, and is filed under the one it has then
            }
            self.unfile(p, k); // the wait got shorter than its key
        }
        self.file(p, k, due);
    }

    #[inline]
    unsafe fn disarm(&self, p: *mut Peer, k: usize) {
        (*p).dl[k].armed = false;
    }

    unsafe fn file(&self, p: *mut Peer, k: usize, due: i64) {
        let l = &mut (*self.st()).dl[k];
        let at = beat(due);
        (*p).dl[k].at = at;
        (*p).dl[k].linked = true;
        if l.tail.is_null() {
            (*p).dl[k].next = null_mut();
            l.head = p;
            l.tail = p;
            return;
        }
        if (*l.tail).dl[k].at <= at {
            // the common case: the newest deadline is the latest
            (*p).dl[k].next = null_mut();
            (*l.tail).dl[k].next = p;
            l.tail = p;
            return;
        }
        if at < (*l.head).dl[k].at {
            (*p).dl[k].next = l.head;
            l.head = p;
            return;
        }
        let mut q = if !l.finger.is_null() && (*l.finger).dl[k].at <= at { l.finger } else { l.head };
        while (*(*q).dl[k].next).dl[k].at <= at {
            q = (*q).dl[k].next; // behind its equals; stops before the tail, whose key is later
        }
        (*p).dl[k].next = (*q).dl[k].next;
        (*q).dl[k].next = p;
        l.finger = p;
    }

    /// Out of the middle: a walk with the previous in hand. Rare - a wait shorter than the one it
    /// is filed under.
    unsafe fn unfile(&self, p: *mut Peer, k: usize) {
        let l = &mut (*self.st()).dl[k];
        let mut prev: *mut Peer = null_mut();
        let mut c = l.head;
        while !c.is_null() {
            if c == p {
                if prev.is_null() {
                    l.head = (*c).dl[k].next;
                } else {
                    (*prev).dl[k].next = (*c).dl[k].next;
                }
                if l.tail == c {
                    l.tail = prev;
                }
                if l.finger == c {
                    l.finger = prev;
                }
                break;
            }
            prev = c;
            c = (*c).dl[k].next;
        }
        (*p).dl[k].next = null_mut();
        (*p).dl[k].linked = false;
    }

    #[inline]
    unsafe fn pop(&self, k: usize) {
        let l = &mut (*self.st()).dl[k];
        let h = l.head;
        l.head = (*h).dl[k].next;
        if l.head.is_null() {
            l.tail = null_mut();
        }
        if l.finger == h {
            l.finger = null_mut();
        }
        (*h).dl[k].next = null_mut();
        (*h).dl[k].linked = false;
    }

    /// The key of the head; heads nobody waits on any more are dropped on the way.
    unsafe fn dl_next(&self, k: usize) -> i64 {
        loop {
            let h = (*self.st()).dl[k].head;
            if h.is_null() {
                return i64::MAX;
            }
            if (*h).dl[k].armed {
                return (*h).dl[k].at;
            }
            self.pop(k);
        }
    }

    /// Every key that has come up: a wait that is over times out, one that is later by now is filed
    /// again.
    unsafe fn dl_expire(&self, k: usize, now: i64) {
        loop {
            let c = (*self.st()).dl[k].head;
            if c.is_null() || 0 < (*c).dl[k].at - now {
                return;
            }
            self.pop(k);
            if !(*c).dl[k].armed {
                continue;
            }
            if now - (*c).dl[k].due < 0 {
                self.file(c, k, (*c).dl[k].due);
            } else {
                (*c).dl[k].armed = false;
                guarded(c, || timed_out(c, k, now));
            }
        }
    }
}

// Host

/// What a test hook decides for one datagram.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireAction {
    Pass,
    Drop,
    Duplicate,
}

/// The direction a hooked datagram travels.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireDir {
    Tx,
    Rx,
}

#[cfg(test)]
pub type WireHook = Arc<dyn Fn(WireDir, &[u8]) -> WireAction + Send + Sync>;

/// The builder of a UDP host.
pub mod host {
    use super::*;

    /// Builder for a datagram [`Host`].
    pub struct Builder {
        name: String,
        engine: Option<Arc<Engine>>,
        idle_timeout: Duration,
        keepalive: Duration,
        mtu: usize,
        window: i64,
        congestion: bool,
        handshake_retry: Duration,
        accept_timeout: Duration,
        close_linger: Duration,
        socket_buffer: usize,
        max_connections: usize,
        on_connect: Option<ConnectionFactory>,
        mux: Option<crate::lib::network::mux::MuxPick>,
        on_failure: Option<crate::lib::network::failure::OnFailure>,
        #[cfg(test)]
        wire_hook: Option<WireHook>,
    }

    impl Builder {
        pub fn new(name: impl Into<String>) -> Self {
            Self {
                name: name.into(),
                engine: None,
                idle_timeout: Duration::from_millis(30_000),
                keepalive: Duration::from_millis(15_000),
                mtu: 1200,
                window: 256 << 10,
                congestion: true,
                handshake_retry: Duration::from_millis(250),
                accept_timeout: Duration::from_millis(10_000),
                close_linger: Duration::from_millis(1_000),
                socket_buffer: 1 << 20,
                max_connections: 0,
                on_connect: None,
                mux: None,
                on_failure: None,
                #[cfg(test)]
                wire_hook: None,
            }
        }

        /// Where the failures of the host and of its connections go. Default: the handler of its
        /// engine.
        pub fn on_failure(mut self, f: crate::lib::network::failure::OnFailure) -> Self {
            self.on_failure = Some(f);
            self
        }

        /// The loops the connections run on. Default: [`Engine::shared`].
        pub fn engine(mut self, engine: Arc<Engine>) -> Self {
            self.engine = Some(engine);
            self
        }
        /// Nothing received for this long asks the timeout handler of the connection; zero - never.
        /// A datagram transport has no FIN: silence is how a peer is lost.
        pub fn idle_timeout(mut self, d: Duration) -> Self {
            self.idle_timeout = d;
            self
        }
        /// A question after this long without sending anything: keeps the NAT binding and the
        /// peer's idle timer.
        pub fn keepalive(mut self, d: Duration) -> Self {
            self.keepalive = d;
            self
        }
        /// The largest datagram sent, in bytes of UDP payload: 1200 passes every path. Held to
        /// [`MTU_MIN`]..=[`MTU_MAX`].
        pub fn mtu(mut self, mtu: usize) -> Self {
            self.mtu = mtu.clamp(MTU_MIN, MTU_MAX);
            self
        }
        /// How many bytes of the journal may be out unreported, per connection.
        pub fn window(mut self, bytes: i64) -> Self {
            self.window = bytes;
            self
        }
        /// NewReno with pacing. Off: as fast as the window lets - a LAN or a link of its own, never
        /// the Internet.
        pub fn congestion(mut self, on: bool) -> Self {
            self.congestion = on;
            self
        }
        /// The first repeat of an INITIAL; it doubles, up to a second.
        pub fn handshake_retry(mut self, d: Duration) -> Self {
            self.handshake_retry = d;
            self
        }
        /// How long an accepted connection waits for its client to show it got the ACCEPT.
        pub fn accept_timeout(mut self, d: Duration) -> Self {
            self.accept_timeout = d;
            self
        }
        /// How long a closing side answers the peer's packets with CLOSE again.
        pub fn close_linger(mut self, d: Duration) -> Self {
            self.close_linger = d;
            self
        }
        /// The kernel buffers of a socket; 0 - what the system gives.
        pub fn socket_buffer(mut self, bytes: usize) -> Self {
            self.socket_buffer = bytes;
            self
        }
        /// How many connections this host holds at once; 0 - no limit. An INITIAL past it is not
        /// answered.
        pub fn max_connections(mut self, n: usize) -> Self {
            self.max_connections = n;
            self
        }
        /// Seats the protocol layer on an accepted connection, on the thread of its loop. Required
        /// to bind.
        pub fn on_connect(mut self, f: ConnectionFactory) -> Self {
            self.on_connect = Some(f);
            self
        }
        /// The port is multiplexed: an INITIAL names the connection it opens with the uid of the host
        /// that calls, and `pick` gives the factory for it - `None` for an unknown one: the INITIAL
        /// gets nothing, and the host hears it. An unnamed INITIAL gets nothing either. Instead of
        /// [`on_connect`](Self::on_connect).
        pub fn mux(mut self, pick: crate::lib::network::mux::MuxPick) -> Self {
            self.mux = Some(pick);
            self
        }
        #[cfg(test)]
        pub fn wire_hook(mut self, f: WireHook) -> Self {
            self.wire_hook = Some(f);
            self
        }

        pub fn build(self) -> Host {
            let engine = self.engine.unwrap_or_else(Engine::shared);
            let n = engine.loops.len();
            let mut token_secret = [0u8; 32];
            let _ = getrandom::fill(&mut token_secret);
            Host {
                inner: Arc::new(HostInner {
                    name: self.name,
                    idle_ms: self.idle_timeout.as_millis().min(i32::MAX as u128) as i64,
                    keepalive: self.keepalive.as_nanos() as i64,
                    mtu: self.mtu,
                    window: self.window,
                    congestion: self.congestion,
                    handshake_retry: self.handshake_retry.as_nanos() as i64,
                    accept_timeout: self.accept_timeout.as_nanos() as i64,
                    close_linger: self.close_linger.as_nanos() as i64,
                    socket_buffer: self.socket_buffer,
                    max_connections: self.max_connections,
                    on_connect: self.on_connect,
                    mux: self.mux,
                    on_failure: self.on_failure.unwrap_or_else(|| engine.on_failure()),
                    #[cfg(test)]
                    wire_hook: self.wire_hook,
                    token_secret,
                    live: AtomicI64::new(0),
                    free: (0..n).map(|_| UnsafeCell::new(null_mut())).collect(),
                    clients: (0..n).map(|_| UnsafeCell::new(null_mut())).collect(),
                    bound: Mutex::new(Vec::new()),
                    engine,
                }),
            }
        }
    }
}

pub(crate) struct HostInner {
    name: String,
    engine: Arc<Engine>,
    idle_ms: i64,
    keepalive: i64,
    mtu: usize,
    window: i64,
    congestion: bool,
    handshake_retry: i64,
    accept_timeout: i64,
    close_linger: i64,
    socket_buffer: usize,
    max_connections: usize,
    on_connect: Option<ConnectionFactory>,
    /// Listening: the port is multiplexed ( [`host::Builder::mux`] ).
    mux: Option<crate::lib::network::mux::MuxPick>,
    /// Where the failures of the host and of its connections go.
    on_failure: super::failure::OnFailure,
    #[cfg(test)]
    wire_hook: Option<WireHook>,
    token_secret: [u8; 32],
    live: AtomicI64,
    /// Per loop: the free slots of this host, a list through `Peer::next_free`; the socket
    /// connections are made through.
    free: Box<[UnsafeCell<*mut Peer>]>,
    clients: Box<[UnsafeCell<*mut Endpoint>]>,
    bound: Mutex<Vec<SocketAddr>>,
}

// SAFETY: `free[i]` and `clients[i]` are touched by the thread of loop `i` only.
unsafe impl Sync for HostInner {}
unsafe impl Send for HostInner {}

/// Serializes an address for the server-side token preimage. The client treats it as opaque.
fn address_bytes(from: &SocketAddr, out: &mut [u8; 19]) -> usize {
    match from {
        SocketAddr::V4(a) => {
            out[0..4].copy_from_slice(&a.ip().octets());
            out[4..6].copy_from_slice(&a.port().to_le_bytes());
            6
        }
        SocketAddr::V6(a) => {
            out[0..16].copy_from_slice(&a.ip().octets());
            out[16..18].copy_from_slice(&a.port().to_le_bytes());
            18
        }
    }
}

/// The clock of the retry tokens: seconds of the monotonic clock of the process - a step of the
/// wall clock voids no token; a token is read by the server that made it alone.
fn token_seconds() -> u32 {
    (super::engine::clock() / 1_000_000_000) as u32
}

impl HostInner {
    /// 4 bytes of time and 12 of HMAC-SHA256(secret, time, scid, address): proof that the address
    /// answered.
    fn token_at(&self, dst: &mut [u8], from: &SocketAddr, scid: u32, seconds: u32) {
        let mut ab = [0u8; 19];
        let n = address_bytes(from, &mut ab);
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.token_secret).unwrap();
        mac.update(&seconds.to_le_bytes());
        mac.update(&scid.to_le_bytes());
        mac.update(&ab[..n]);
        let tag: [u8; 32] = mac.finalize().into_bytes().into();
        dst[..4].copy_from_slice(&seconds.to_le_bytes());
        dst[4..TOKEN].copy_from_slice(&tag[..TOKEN - 4]);
    }

    fn token_make(&self, dst: &mut [u8], from: &SocketAddr, scid: u32) {
        self.token_at(dst, from, scid, token_seconds());
    }

    fn token_valid(&self, token: &[u8], from: &SocketAddr, scid: u32) -> bool {
        let seconds = le32(token, 0);
        if token_seconds().wrapping_sub(seconds) > TOKEN_SECONDS {
            return false;
        }
        let mut expected = [0u8; TOKEN];
        self.token_at(&mut expected, from, scid, seconds);
        expected.iter().zip(&token[..TOKEN]).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
    }

    fn socket(&self, addr: SocketAddr, reuse: bool) -> io::Result<UdpSocket> {
        let s = socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
        if 0 < self.socket_buffer {
            s.set_recv_buffer_size(self.socket_buffer)?; // a socket without the buffers asked for is refused
            s.set_send_buffer_size(self.socket_buffer)?;
        }
        #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
        if reuse {
            s.set_reuse_port(true)?;
        }
        let _ = reuse;
        if addr.is_ipv6() {
            let _ = s.set_only_v6(false); // [::] serves IPv4 too
        }
        s.set_nonblocking(true)?;
        s.bind(&addr.into())?;
        Ok(UdpSocket::from_std(s.into()))
    }

    /// The socket of loop `lp` connections are made through: made on the first connect there.
    unsafe fn client(self: &Arc<Self>, lp: &Loop) -> io::Result<*mut Endpoint> {
        let c = self.clients[lp.index].get();
        if (*c).is_null() {
            // one dual-stack socket for both families; IPv4 alone where the system has no IPv6
            let socket = match self.socket(([0u16; 8], 0).into(), false) {
                Ok(s) => s,
                Err(_) => self.socket(([0, 0, 0, 0], 0).into(), false)?,
            };
            let mut ep = Endpoint::new(socket, self.clone(), false);
            let p: *mut Endpoint = &mut *ep;
            lp.add(ep)?;
            *c = p;
        }
        Ok(*c)
    }
}

/// An IPv4 peer named by a dual-stack socket (`::ffff:a.b.c.d`) as the IPv4 address it is: one
/// peer, one address.
#[inline]
fn canonical(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => a,
        },
        _ => a,
    }
}

/// Where a datagram to `to` goes on the socket of `ep`: an IPv4 address mapped when the socket is a
/// dual-stack IPv6 one.
#[inline]
unsafe fn wire_to(ep: *mut Endpoint, to: SocketAddr) -> SocketAddr {
    match to {
        SocketAddr::V4(v4) if (*ep).v6 => SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port()),
        _ => to,
    }
}

/// The server and the client side at once: [`bind`](Self::bind) takes connections in,
/// [`connect`](Self::connect) makes them.
#[derive(Clone)]
pub struct Host {
    inner: Arc<HostInner>,
}

/// A look at a connection's counters, taken on its loop.
#[derive(Debug, Clone, Copy)]
pub struct PeerStats {
    pub state: u8,
    pub forged: u64,
    pub replayed: u64,
    pub gaps: u64,
    pub refused: u64,
    pub lost_datagrams: u64,
    pub cwnd: i64,
    /// The smoothed round trip, in milliseconds; -1 while unmeasured.
    pub rtt_ms: f64,
    pub attempts: u32,
    /// The journal: what the peer reported, one past its last byte; -1 without one.
    pub journal_acked: i64,
    pub journal_next: i64,
}

const REUSE: bool = cfg!(target_os = "linux");

/// Formats the datagram host name.
impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UDP {}", self.name())
    }
}

impl Host {
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Takes connections in on `addr` (a port 0 - one the system chooses): the address bound. On
    /// Linux every loop gets a socket of its own on it (`SO_REUSEPORT`); elsewhere one loop reads
    /// the one socket.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<SocketAddr> {
        if self.inner.on_connect.is_none() && self.inner.mux.is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a host that binds seats its connections: on_connect or mux"));
        }
        let engine = &self.inner.engine;
        let loops: Vec<&'static Loop> = if REUSE { engine.loops.to_vec() } else { vec![engine.pick()] };
        let mut ip = addr;
        for lp in loops {
            let socket = self.inner.socket(ip, REUSE)?;
            if ip.port() == 0 {
                ip = socket.local_addr()?; // the other loops take the same port
            }
            let ep = Endpoint::new(socket, self.inner.clone(), true);
            // SAFETY: on the thread of the loop
            lp.call(move |lp| unsafe { lp.add(ep) })?;
        }
        self.inner.bound.lock().push(ip);
        Ok(ip)
    }

    /// The addresses this host listens on, as bound.
    pub fn bound(&self) -> Vec<SocketAddr> {
        self.inner.bound.lock().clone()
    }

    pub fn connections_live(&self) -> i64 {
        self.inner.live.load(Ordering::Relaxed)
    }

    /// Connects to `server`, with the protocol layer `seat` gives seated on the connection's state
    /// first - on the thread of the connection's loop. `on_connected` hears there: the state once
    /// the server accepted, or what went wrong - `TimedOut` when no answer came in `timeout`. The
    /// Noise the protocol layer declares as it is seated ( [`ExternalConnection::secure`] ) encrypts
    /// the connection: only a server whose layer declares the matching keys accepts it; none - only
    /// a server whose layer declares none.
    pub fn connect(
        &self,
        server: SocketAddr,
        timeout: Duration,
        seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
        on_connected: impl FnOnce(io::Result<Arc<ExternalConnection>>) + Send + 'static,
    ) {
        self.connect_named(server, timeout, None, seat, on_connected)
    }

    /// As [`connect`](Self::connect), to a multiplexed port of the server: `named` - the uid the
    /// INITIAL names the connection with ( the `MUX_ID` of the connection ), `None` - an ordinary port.
    pub fn connect_named(
        &self,
        server: SocketAddr,
        timeout: Duration,
        named: Option<u8>,
        seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
        on_connected: impl FnOnce(io::Result<Arc<ExternalConnection>>) + Send + 'static,
    ) {
        let host = self.inner.clone();
        let lp = host.engine.pick();
        lp.submit(Box::new(move |lp| unsafe {
            // SAFETY: on the thread of the loop
            let ep = match host.client(lp) {
                Ok(ep) => ep,
                Err(e) => return on_connected(Err(e)),
            };
            let Some(p) = lp.allocate(&host, ep) else {
                return on_connected(Err(io::Error::other("A loop holds 1 048 576 UDP connections at most.")));
            };
            (*p).address = Some(canonical(server)); // as the replies are named
            (*p).named = named;
            (*p).ext.set_addresses((*ep).socket.local_addr().ok(), (*p).address); // the region of the keys sees where it calls
            let (sink, source) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| seat(&(*p).ext))) {
                Ok(s) => s,
                Err(why) => {
                    // no protocol layer for it: the slot goes back, whoever waits hears it
                    let e = super::failure::panicked(&*why);
                    (*p).ext.failure(&e);
                    free(p);
                    return on_connected(Err(e));
                }
            };
            (*p).sink = Some(sink);
            (*p).source = Some(source);
            match (*p).ext.take_secured() {
                Some(None) => {
                    // no keys for it: no INITIAL goes
                    let e = super::TCP::refused();
                    (*p).ext.failure(&e);
                    free(p);
                    return on_connected(Err(e));
                }
                Some(Some(c)) if 0 <= c.pattern => (*p).secured = Some(c), // every handshake of the slot, a reconnect's too
                _ => {}
            }
            (*p).on_connected = Some(Box::new(on_connected));
            connect(p, lp.now() + timeout.as_nanos().min(i64::MAX as u128 / 2) as i64);
        }));
    }

    /// Continues the parked session of `state` over a new handshake - to `server`, or to where it
    /// was (`None`). The slot, its protocol layer and its journal stay what they were. A failed
    /// attempt parks the session again, for what is left of its wait.
    pub fn reconnect(&self, state: &Arc<ExternalConnection>, timeout: Duration, server: Option<SocketAddr>, on_connected: impl FnOnce(io::Result<Arc<ExternalConnection>>) + Send + 'static) {
        let Some(link) = state.udp.get() else {
            return on_connected(Err(io::Error::new(io::ErrorKind::InvalidInput, "not a UDP connection")));
        };
        let p = link.0 as usize;
        let host = self.inner.clone();
        // SAFETY: a slot lives as long as its loop: forever
        let lp = unsafe { (*link.0).lp };
        lp.submit(Box::new(move |lp| unsafe {
            let p = p as *mut Peer;
            if !Arc::ptr_eq(&(*p).host, &host) || (*p).state != PARKED {
                return on_connected(Err(io::Error::other("the connection holds no parked session")));
            }
            let to = canonical(server.or((*p).address).unwrap());
            let ep = match host.client(lp) {
                Ok(ep) => ep,
                Err(e) => return on_connected(Err(e)), // the session stays parked
            };
            if !(*p).claimable.swap(false, Ordering::AcqRel) {
                return on_connected(Err(io::Error::other("the connection holds no parked session")));
                // a reclaim took it meanwhile
            }
            lp.disarm(p, PARK);
            (*p).ep = ep;
            (*p).ext.dialing.store(true, Ordering::Release); // a reconnect: this side makes the connection
            (*p).cid = cid((*p).slot); // the old one's late packets name nobody
            (*p).address = Some(to);
            // the handshake is of the Noise the protocol layer declared at the connect ( `secured` ): the layer is not seated
            // again - its seat hook would give a new source, and that takes the journal of the session over from the slot's own
            (*p).on_connected = Some(Box::new(on_connected));
            connect(p, lp.now() + timeout.as_nanos().min(i64::MAX as u128 / 2) as i64);
        }));
    }

    /// The connection slots of this host on every loop, whatever their state: for a look from
    /// outside, as a test takes it.
    pub fn peers(&self) -> Vec<Arc<ExternalConnection>> {
        let mut all = Vec::new();
        for lp in self.inner.engine.loops.iter() {
            for &p in lp.shared.read().iter() {
                let p = p as *mut Peer;
                // SAFETY: a slot lives as long as its loop; its host and its connection state are
                // its own for life
                unsafe {
                    if Arc::ptr_eq(&(*p).host, &self.inner) {
                        all.push((*p).ext.clone());
                    }
                }
            }
        }
        all
    }

    /// Gives up the parked session of the slot of `state` now, as its wait running out does: its
    /// close is heard, the slot goes back to the host. Waits for the loop; nothing when it is not
    /// parked.
    pub fn reap_now(&self, state: &Arc<ExternalConnection>) {
        let Some(l) = state.udp.get() else { return };
        let p = l.0 as usize;
        // SAFETY: a slot lives as long as its loop
        let lp = unsafe { (*(p as *mut Peer)).lp };
        lp.call(move |_| unsafe {
            let p = p as *mut Peer;
            if (*p).state == PARKED {
                reap(p);
            }
        });
    }

    /// Formats a connection as `[index] host state`. Waits for its loop to read the state. Returns
    /// `None` if `state` does not belong to this host.
    pub fn describe(&self, state: &Arc<ExternalConnection>) -> Option<String> {
        let p = state.udp.get()?.0 as usize;
        let index = state.index.0;
        // SAFETY: a slot lives as long as its loop
        let lp = unsafe { (*(p as *mut Peer)).lp };
        Some(lp.call(move |_| unsafe {
            let p = p as *mut Peer;
            let what = match (*p).state {
                ACTIVE => "active",
                CONNECTING if (*p).accepting => "accepting",
                CONNECTING => "connecting",
                CLOSING => "closing",
                PARKED => "parked",
                _ => "free",
            };
            format!("[{index}] {} {what}", (*p).host.name)
        }))
    }

    /// The counters of the connection of `state`, taken on its loop; `None` - not a connection of
    /// this host.
    pub fn stats(&self, state: &Arc<ExternalConnection>) -> Option<PeerStats> {
        let p = state.udp.get()?.0 as usize;
        // SAFETY: a slot lives as long as its loop
        let lp = unsafe { (*(p as *mut Peer)).lp };
        Some(lp.call(move |_| unsafe {
            let p = p as *mut Peer;
            PeerStats {
                state: (*p).state,
                forged: (*p).forged,
                replayed: (*p).replayed,
                gaps: (*p).gaps,
                refused: (*p).refused,
                lost_datagrams: (*p).lost_datagrams,
                cwnd: (*p).cwnd,
                rtt_ms: if (*p).has_rtt { (*p).srtt as f64 / TICKS_MS as f64 } else { -1. },
                attempts: (*p).attempts,
                journal_acked: journal(p).map_or(-1, |j| j.acked()),
                journal_next: journal(p).map_or(-1, |j| j.next()),
            }
        }))
    }

    /// The stream state of the connection of `state`, for a log line. Waits for the loop; `None` -
    /// not a connection of this host.
    pub fn stream_text(&self, state: &Arc<ExternalConnection>) -> Option<String> {
        let head = self.describe(state)?;
        let p = state.udp.get()?.0 as usize;
        // SAFETY: a slot lives as long as its loop
        let lp = unsafe { (*(p as *mut Peer)).lp };
        let id = state.session() as u32;
        Some(lp.call(move |lp| unsafe {
            let p = p as *mut Peer;
            let ms = |ticks: i64| ticks as f64 / TICKS_MS as f64;
            let probe = if (*p).dl[PROBE].armed {
                format!("in {:.0}ms", ms((*p).dl[PROBE].due - lp.now()))
            } else {
                "-".into()
            };
            format!(
                "{head} id={id} tx={}.{} rx={}.{}{}{} lost={} cwnd={}KB tokens={:.0} srtt={:.1}ms pace={} drained={} gaps={} refused={}{} backoff={} pto={:.1}ms probe={probe} received={} {}",
                (*p).tx_series,
                (*p).tx_number,
                (*p).rx_series,
                (*p).rx_number,
                if (*p).wanted != (*p).rx_series { format!(" wanted={}", (*p).wanted) } else { String::new() },
                if (*p).failed { " failed" } else { "" },
                (*p).lost_datagrams,
                (*p).cwnd / 1024,
                (*p).tokens,
                ms((*p).srtt),
                if (*p).dl[PACE].armed { "armed" } else { "-" },
                (*p).drained,
                (*p).gaps,
                (*p).refused,
                if (*p).asked { " asked" } else { "" },
                (*p).backoff,
                ms(pto(p)),
                position(p),
                journal(p).map_or(String::from("null"), |j| j.to_string()),
            )
        }))
    }

    /// The state of the connection, as text: for the tests.
    #[cfg(test)]
    pub(crate) fn dump(&self, state: &Arc<ExternalConnection>) -> String {
        let Some(l) = state.udp.get() else { return "no peer".into() };
        let p = l.0 as usize;
        // SAFETY: a slot lives as long as its loop
        let lp = unsafe { (*(p as *mut Peer)).lp };
        lp.call(move |lp| unsafe {
            let p = p as *mut Peer;
            let armed: String = (0..KINDS).map(|k| if (*p).dl[k].armed { char::from(b'0' + k as u8) } else { '-' }).collect();
            format!(
                "state {} tx {}/{} lost_at {} said {:x} starting {} | rx {}/{} wanted {} failed {} received {} told {} requested {} asks {} stale {} | asking {} asked {} again {} held_turn {:?} guarded {} drained {} blocked {} backoff {} tokens {:.0} cwnd {} armed {armed} now {} | journal {:?}",
                (*p).state, (*p).tx_series, (*p).tx_number, (*p).lost_at, (*p).said, (*p).starting,
                (*p).rx_series, (*p).rx_number, (*p).wanted, (*p).failed, position(p), (*p).told, (*p).requested, (*p).asks, (*p).stale,
                (*p).asking, (*p).asked, (*p).asked_again, (*p).held_turn, journaled(p), (*p).drained, (*p).tx_blocked, (*p).backoff, (*p).tokens, (*p).cwnd, lp.now(),
                journal(p).map(|j| (j.base(), j.acked(), j.sent(), j.next())),
            )
        })
    }

    /// Drops every connection of this host - each peer gets a CLOSE - and closes its sockets.
    pub fn shutdown(&self) {
        for &lp in self.inner.engine.loops.iter() {
            let host = self.inner.clone();
            lp.call(move |lp| unsafe {
                // SAFETY: on the thread of the loop
                let st = lp.st();
                let n = (*st).slots.len();
                for i in 0..n {
                    let p = (&(*st).slots)[i];
                    if Arc::ptr_eq(&(*p).host, &host) && (*p).state != FREE {
                        (*p).ext.claim_close(Event::THIS_CLOSE_ABRUPTLY);
                        end(p, Event::THIS_CLOSE_ABRUPTLY, 1);
                        if (*p).state == PARKED {
                            reap(p); // the host goes: no reaper is left to end it later - its close is heard now
                        } else if (*p).state != FREE {
                            free(p); // a linger has no socket to linger on
                        }
                    }
                }
                lp.remove_of(&host);
                *host.clients[lp.index].get() = null_mut();
            });
        }
        self.inner.bound.lock().clear();
    }
}

// Peer

/// The place of a connection in one deadline list.
#[derive(Clone, Copy)]
struct Dl {
    next: *mut Peer,
    at: i64,
    due: i64,
    armed: bool,
    linked: bool,
}

/// The state of the slot, and its mirror in the state of the connection the application holds: read
/// from any thread, the slot's own only on its loop.
#[inline]
unsafe fn set_state(p: *mut Peer, s: u8) {
    (*p).state = s;
    let mirror = match s {
        ACTIVE => super::TCP::pool::ACTIVE,
        PARKED => super::TCP::pool::PARKED,
        CLOSING => super::TCP::pool::CLEANING,
        _ => super::TCP::pool::FREE, // free, or connecting: none of the three
    };
    (*p).ext.conn_state.store(mirror, Ordering::Release);
}

/// Where a [`ExternalConnection`] of a UDP connection reaches its slot: requests, from any thread.
pub(crate) struct Link(*mut Peer);

// SAFETY: through a link only `requests` and the stack of the loop are touched - atomics
unsafe impl Send for Link {}
unsafe impl Sync for Link {}

impl Link {
    /// This thread runs the loop of the connection.
    #[inline]
    pub(crate) fn on_loop_thread(&self) -> bool {
        // SAFETY: a slot lives as long as its loop: forever; `lp` is set once, when the slot is made
        unsafe { (*self.0).lp.in_loop() }
    }

    #[inline]
    pub(crate) fn request(&self, what: u32) {
        // SAFETY: a slot lives as long as its loop: forever
        unsafe { request(self.0, what) }
    }

    /// The peer hands back the token of a former connection of this host: the parked session it
    /// names moves onto this connection, which takes its identity. On the loop of this connection;
    /// waits for it.
    pub(crate) fn reclaim(&self, token: u64) {
        let p = self.0 as usize;
        // SAFETY: a slot lives as long as its loop: forever
        let lp = unsafe { (*self.0).lp };
        lp.call(move |_| unsafe { reclaim(p as *mut Peer, token) });
    }

    /// The wait of the stage ran out ([`ExternalConnection::receive_timed_out`]): closed as the
    /// receive timeout of the link closes it - the event `RECEIVE_TIMEOUT`, an identified session
    /// parks. On the loop of the connection.
    pub(crate) fn receive_timed_out(&self) {
        let p = self.0;
        // SAFETY: on the thread of its loop - the caller's check
        unsafe {
            if (*p).state == ACTIVE && !(*p).closing_gracefully {
                end(p, Event::RECEIVE_TIMEOUT, 1);
            }
        }
    }
}

/// One connection. It lives on its loop: every field but `requests` and `next_request` is the
/// loop's, and a call from another thread is a request the loop serves on its next pass.
pub(crate) struct Peer {
    lp: &'static Loop,
    host: Arc<HostInner>,
    slot: u32,
    /// The state of the slot, for its life: what the application holds.
    ext: Arc<ExternalConnection>,
    requests: AtomicU32,
    next_request: UnsafeCell<*mut Peer>,

    state: u8,
    ep: *mut Endpoint,
    address: Option<SocketAddr>,
    /// This side's connection id - what the peer's packets carry - and the peer's.
    cid: u32,
    rcid: u32,
    dl: [Dl; KINDS],
    next_free: *mut Peer,

    sink: Option<Box<dyn BytesDst>>,
    source: Option<Box<dyn BytesSrc>>,

    // the handshake
    on_connected: Option<OnConnected>,
    connect_until: i64,
    attempts: u32,
    token: [u8; TOKEN],
    tokened: bool,
    initial_at: i64,
    accepting: bool,
    confirming: bool,
    /// Calling: the Noise the protocol layer declared, of every handshake of the slot.
    secured: Option<Arc<Chain>>,
    /// Calling a multiplexed port: the uid the INITIAL names the connection with.
    named: Option<u8>,
    noise: Option<NoiseNK>,
    hs: [u8; HS_MAX],
    hs_len: usize,
    tx_key: Option<Aead>,
    rx_key: Option<Aead>,
    forged: u64,
    replayed: u64,

    // the stream this side sends: whole numbers, the wire carries the low bits
    tx_series: i64,
    tx_number: i64,
    tx_service: i64,
    lost_at: i64,
    starting: bool,
    said: u8,

    // the stream this side receives
    rx_series: i64,
    rx_number: i64,
    wanted: i64,
    rx_service: i64,
    failed: bool,
    requested: i64,
    requested_at: i64,
    requested_once: bool,
    owed: bool,
    told: i64,
    asking: bool,
    asked: bool,
    asked_again: bool,
    asked_at: i64,
    asked_from: i64,
    /// The peer asked for a turn of the stream this side sends (its position, the series) before a
    /// session was named here: a journal belongs to a session, so the turn waits for one - the
    /// identity given, or a session moved here by a reclaim. Answered with the journal of a new
    /// connection, the peer would jump past what the parked session still owes it.
    held_turn: Option<(i64, i64)>,
    close_pending: bool,
    closing_gracefully: bool,
    drained: bool,
    backoff: u32,
    asks: u32,
    stale: u32,
    last_acked: i64,
    linger_until: i64,
    close_repeats: u32,

    // the round trip, RFC 6298, in clock ticks
    srtt: i64,
    rttvar: i64,
    has_rtt: bool,

    gaps: u64,
    refused: u64,
    lost_datagrams: u64,

    // the window
    cwnd: i64,
    ssthresh: i64,
    reduced_at: i64,
    tokens: f64,
    tokens_at: i64,

    received_at: i64,
    /// How long this side may send nothing, in ticks: the host's keepalive, until the generated code
    /// sets a third of the `[ReceiveTimeout]` of the connection ([`ExternalConnection::pulse`]).
    keepalive: i64,

    /// The connection waits for its socket to send the datagram it holds back (on
    /// `Endpoint::blocked`).
    tx_blocked: bool,
    /// What the sink did not take, in order: `tail[0].0[tail_at..tail[0].1]`, then the bytes of
    /// each next buffer up to its end. The first is the buffer the datagram came in, kept as it is;
    /// what comes meanwhile goes behind. Empty - none.
    tail: std::collections::VecDeque<(Box<[u8]>, usize)>,
    tail_at: usize,

    /// The parked session may be taken: by a connection that hands back its token (`reclaim`, from
    /// any loop) or by its own loop - its reap, its reconnect. Exactly one wins it; the loser
    /// leaves the slot alone.
    claimable: AtomicBool,
    /// A parked session claimed for this connection, put in place once its sink is out of the call
    /// it is in.
    moved: Option<Box<Moved>>,
    /// The sink is in a call: what it would swap out waits until the call returns.
    in_sink: bool,
}

/// What a parked session takes along onto the connection that claims it.
struct Moved {
    sink: Option<Box<dyn BytesDst>>,
    source: Option<Box<dyn BytesSrc>>,
    /// The connection state the session was seated on: what its protocol layer hooked there goes to
    /// the new one.
    ext: Arc<ExternalConnection>,
    /// The keys of the session: its stages hold them, they go with it.
    keys: Arc<crate::lib::adhoc::connection::keyring::Keyring>,
    id: u32,
}

impl Peer {
    fn new(lp: &'static Loop, host: Arc<HostInner>, slot: u32, ext: Arc<ExternalConnection>) -> Self {
        let off = Dl {
            next: null_mut(),
            at: 0,
            due: 0,
            armed: false,
            linked: false,
        };
        let keepalive = host.keepalive;
        Self {
            lp,
            host,
            slot,
            ext,
            requests: AtomicU32::new(0),
            next_request: UnsafeCell::new(null_mut()),
            state: FREE,
            ep: null_mut(),
            address: None,
            cid: 0,
            rcid: 0,
            dl: [off; KINDS],
            next_free: null_mut(),
            sink: None,
            source: None,
            on_connected: None,
            connect_until: 0,
            attempts: 0,
            token: [0; TOKEN],
            tokened: false,
            initial_at: 0,
            accepting: false,
            confirming: false,
            secured: None,
            named: None,
            noise: None,
            hs: [0; HS_MAX],
            hs_len: 0,
            tx_key: None,
            rx_key: None,
            forged: 0,
            replayed: 0,
            tx_series: 0,
            tx_number: 0,
            tx_service: 0,
            lost_at: -1,
            starting: false,
            said: 0,
            rx_series: 0,
            rx_number: 0,
            wanted: 0,
            rx_service: -1,
            failed: false,
            requested: 0,
            requested_at: 0,
            requested_once: false,
            owed: false,
            told: -1,
            asking: false,
            asked: false,
            asked_again: false,
            asked_at: 0,
            asked_from: 0,
            held_turn: None,
            close_pending: false,
            closing_gracefully: false,
            drained: false,
            backoff: 0,
            asks: 0,
            stale: 0,
            last_acked: -1,
            linger_until: 0,
            close_repeats: 0,
            srtt: 0,
            rttvar: 0,
            has_rtt: false,
            gaps: 0,
            refused: 0,
            lost_datagrams: 0,
            cwnd: 0,
            ssthresh: i64::MAX,
            reduced_at: 0,
            tokens: 0.,
            tokens_at: 0,
            received_at: 0,
            keepalive,
            tx_blocked: false,
            tail: std::collections::VecDeque::new(),
            tail_at: 0,
            claimable: AtomicBool::new(false),
            moved: None,
            in_sink: false,
        }
    }
}

// the session: its journal, its decoder

/// The journal of the stream this side sends: the session's, which its source keeps. `None` - the
/// stream guarantees nothing: a datagram lost is lost. Its positions as the source published them;
/// the source changes it ([`BytesSrc::with_journal`]).
#[inline]
unsafe fn journal<'a>(p: *mut Peer) -> Option<&'a Journal> {
    let src: &'a Box<dyn BytesSrc> = (*p).source.as_ref()?;
    src.journal()
}

/// The source keeps a journal.
#[inline]
unsafe fn journaled(p: *mut Peer) -> bool {
    (*p).source.as_ref().is_some_and(|s| s.journal().is_some())
}

/// The position in the peer's journal the decoder has got to: the bytes of the guaranteed packs,
/// the one in progress counted; -1 - none.
#[inline]
unsafe fn position(p: *mut Peer) -> i64 {
    (*p).sink.as_ref().map_or(-1, |s| s.received())
}

// requests

/// From any thread: the loop serves `what` on its next pass. No allocation, no lock.
#[inline]
unsafe fn request(p: *mut Peer, what: u32) {
    if (*p).requests.fetch_or(what, Ordering::AcqRel) == 0 {
        (*p).lp.post(p);
    }
}

unsafe fn serve(p: *mut Peer, what: u32) {
    if what & ABORT_ != 0 {
        let ev = if (*p).ext.is_open.load(Ordering::Acquire) {
            Event::THIS_CLOSE_ABRUPTLY
        } else {
            (*p).ext.close_event()
        };
        // a loss (`lost`: REMOTE) goes as a broken link does - the peer is told nothing, so both
        // sides may park and go on, as over TCP; an abort tells it with a CLOSE of its own
        end(p, ev, if ev.is_remote() { -1 } else { 1 });
        return;
    }
    if (*p).state != ACTIVE {
        return;
    }
    if (*p).held_turn.is_some() && (*p).ext.session() as u32 != 0 {
        // the session is named here now: the turn the peer asked for before it
        let (position, series) = (*p).held_turn.take().unwrap();
        turn(p, position, series, (*p).lp.now());
        if (*p).state != ACTIVE {
            return;
        }
    }
    if what & TIMEOUT_ != 0 {
        let ms = (*p).ext.receive_timeout_ms.load(Ordering::Relaxed);
        if ms < i32::MAX as i64 {
            (*p).lp.arm(p, RECEIVE, (*p).received_at + ms * TICKS_MS); // 0: at once
        } else {
            (*p).lp.disarm(p, RECEIVE);
        }
    }
    if what & PULSE_ != 0 {
        let mut ticks = (*p).ext.pulse_ms.load(Ordering::Acquire).saturating_mul(TICKS_MS);
        let host = (*p).host.keepalive;
        if 0 < host && host < ticks {
            ticks = host; // never past the host's: it keeps the binding of the NAT too
        }
        if 0 < ticks && ticks != (*p).keepalive {
            (*p).keepalive = ticks;
            sent_at(p, (*p).lp.now()); // the keepalive armed by the handshake may be later than the peer waits
        }
    }
    if what & CLOSE_ != 0 {
        (*p).close_pending = true; // the close goes out once everything sent is reported: see transmit()
        (*p).closing_gracefully = true;
    }
    if what & CONTINUE_ != 0 {
        ask(p);
    }
    if what & DEADLINE_ != 0 {
        arm_deadline(p);
    }
    if what & RX_ != 0 {
        drain(p);
        if (*p).state != ACTIVE {
            return;
        }
    }
    if what & (TX_ | CLOSE_) != 0 {
        transmit(p);
    }
}

/// A graceful close is asked for: a negative timeout, or [`ExternalConnection::close_gracefully`] -
/// both set the flag.
#[inline]
fn closing(ext: &ExternalConnection) -> bool {
    ext.closing_gracefully.load(Ordering::Relaxed)
}

// timeouts

unsafe fn timed_out(p: *mut Peer, kind: usize, now: i64) {
    let lp = (*p).lp;
    match kind {
        RECEIVE => {
            if (*p).state != ACTIVE {
                return;
            }
            let ms = (*p).ext.receive_timeout_ms.load(Ordering::Relaxed);
            if i32::MAX as i64 <= ms {
                return;
            }
            if now - (*p).received_at < ms * TICKS_MS {
                lp.arm(p, RECEIVE, (*p).received_at + ms * TICKS_MS); // the timeout grew meanwhile
                return;
            }
            match (*p).ext.timeout_handler() {
                Some(h) => match h(TimeoutKind::Receive) {
                    TimeoutAnswer::Extend(ms) if 0 < ms => {
                        if (*p).state == ACTIVE && ms < i32::MAX as i64 {
                            lp.arm(p, RECEIVE, now + ms * TICKS_MS);
                        }
                    }
                    TimeoutAnswer::Disarm => {}
                    _ => {
                        if (*p).state == ACTIVE {
                            end(p, Event::RECEIVE_TIMEOUT, 1);
                        }
                    }
                },
                None => end(p, Event::RECEIVE_TIMEOUT, 1),
            }
        }
        KEEPALIVE => {
            if (*p).state == ACTIVE {
                question(p);
            }
        }
        PROBE => {
            if (*p).state != ACTIVE || !journal(p).is_some_and(|j| j.acked() < j.next()) {
                return;
            }
            // Nothing reported for a probe timeout: the question, or its answer, is lost - or the
            // last datagrams are, and nothing behind them shows it. Asked again: the question tells
            // the peer the number it should have got to.
            (*p).backoff = ((*p).backoff + 1).min(6);
            lp.arm(p, PROBE, now + (pto(p) << (*p).backoff));
            question(p);
            transmit(p);
        }
        REQUEST => {
            if (*p).state != ACTIVE || (*p).wanted == (*p).rx_series {
                return;
            }
            (*p).asks += 1;
            speak(p, -1); // the series asked for has not started: asked for again
        }
        HANDSHAKE => {
            let retry = |attempts: u32| ((*p).host.handshake_retry << attempts.min(16)).min(1000 * TICKS_MS);
            if (*p).state == ACTIVE && (*p).confirming {
                // the server has not heard this client since its ACCEPT: say so again
                question(p);
                let d = retry((*p).attempts);
                (*p).attempts += 1;
                lp.arm(p, HANDSHAKE, now + d);
                return;
            }
            if (*p).state != CONNECTING {
                return;
            }
            if (*p).accepting {
                // the client never showed that it got the ACCEPT
                (*p).accepting = false;
                free(p);
                return;
            }
            if (*p).connect_until - now <= 0 {
                failed(
                    p,
                    io::Error::new(io::ErrorKind::TimedOut, format!("No answer from {} in time.", (*p).address.map_or(String::new(), |a| a.to_string()))),
                );
                return;
            }
            send_initial(p);
            // doubling, but not past a second: a server hears the repeats as the client being alive
            let d = retry((*p).attempts).min((*p).connect_until - now);
            (*p).attempts += 1;
            lp.arm(p, HANDSHAKE, now + d);
        }
        PARK => {
            if (*p).state == PARKED {
                reap(p);
            }
        }
        PACE => {
            if (*p).state == ACTIVE {
                transmit(p);
            }
        }
        LINGER => {
            if (*p).state != CLOSING {
                return;
            }
            if (*p).linger_until - now <= 0 || 3 <= (*p).close_repeats {
                free(p);
                return;
            }
            // A lost close would leave the peer waiting for its idle timeout: said again, a few
            // times, within the linger.
            (*p).close_repeats += 1;
            speak(p, 0);
            lp.arm(p, LINGER, (*p).linger_until.min(now + (pto(p) << (*p).close_repeats)));
        }
        DEADLINE => {
            if (*p).state == ACTIVE {
                (*p).ext.fire_deadline(now_ms().max((*p).ext.deadline_at.load(Ordering::Acquire)));
                arm_deadline(p);
            }
        }
        _ => {}
    }
}

/// The deadline of the actor instances the state holds, when it is nearer than the one the
/// connection waits for.
unsafe fn arm_deadline(p: *mut Peer) {
    if (*p).state != ACTIVE {
        return;
    }
    let at = (*p).ext.deadline_at.load(Ordering::Acquire);
    if at == 0 {
        return;
    }
    let lp = (*p).lp;
    let due = lp.now() + (at - now_ms()).max(0) * TICKS_MS;
    if !(*p).dl[DEADLINE].armed || due < (*p).dl[DEADLINE].due {
        lp.arm(p, DEADLINE, due);
    }
}

// the handshake

unsafe fn connect(p: *mut Peer, until: i64) {
    let lp = (*p).lp;
    (*p).connect_until = until;
    (*p).attempts = 0;
    (*p).tokened = false;
    drop_keys(p); // a reconnect: a new handshake, new keys
    set_state(p, CONNECTING);
    (*p).initial_at = lp.now();
    send_initial(p);
    lp.arm(p, HANDSHAKE, lp.now() + (*p).host.handshake_retry); // the deadline is looked at when it comes
}

unsafe fn send_initial(p: *mut Peer) {
    let lp = (*p).lp;
    if (*(*p).ep).held != 0 {
        return; // the socket holds a datagram back: the INITIAL is sent again at the next HANDSHAKE
    }
    let tx = &mut (*(*p).ep).tx;
    tx[0] = LONG | INITIAL;
    tx[1] = VERSION;
    put32(tx, 2, (*p).cid);
    let mut pos = 7;
    if (*p).tokened {
        tx[6] = TOKEN as u8;
        tx[7..7 + TOKEN].copy_from_slice(&(*p).token);
        pos += TOKEN;
    } else {
        tx[6] = 0;
    }
    if let Some(c) = &(*p).secured {
        if (*p).noise.is_none() {
            // the first message of the handshake, written once: every repeat of the INITIAL sends
            // the same bytes
            let mut n = c.handshake(true, &prologue_named((*p).cid, (*p).named));
            (*p).hs_len = n.write_message(&mut (*p).hs, &[]).unwrap_or(0);
            (*p).noise = Some(n);
        }
        tx[pos] = (*p).hs_len as u8;
        pos += 1;
        tx[pos..pos + (*p).hs_len].copy_from_slice(&(*p).hs[..(*p).hs_len]);
        pos += (*p).hs_len;
    } else {
        tx[pos] = 0;
        pos += 1;
    }
    match (*p).named {
        None => {
            tx[pos] = 0;
            pos += 1;
        }
        Some(uid) => {
            tx[pos] = 1;
            tx[pos + 1] = uid;
            pos += 2;
        }
    }
    tx[pos..MIN_INITIAL].fill(0); // the padding
    lp.send((*p).ep, (*p).address.unwrap(), MIN_INITIAL);
}

unsafe fn retry(p: *mut Peer) {
    if (*p).state != CONNECTING {
        return;
    }
    let rx = &(*(*p).lp.st()).buf;
    (*p).token.copy_from_slice(&rx[6..6 + TOKEN]);
    (*p).tokened = true;
    send_initial(p);
}

/// The server side of the handshake: the ACCEPT is out, but the connection opens only when the
/// client shows it got it - with its first short packet. A lost ACCEPT is sent again for every
/// repeated INITIAL.
unsafe fn accept(p: *mut Peer) {
    (*p).accepting = true;
    set_state(p, CONNECTING);
    send_accept(p);
    let lp = (*p).lp;
    lp.arm(p, HANDSHAKE, lp.now() + (*p).host.accept_timeout);
}

unsafe fn confirmed(p: *mut Peer) {
    (*p).accepting = false;
    (*p).lp.disarm(p, HANDSHAKE);
    open(p, Event::REMOTE_CONNECT);
}

unsafe fn accepted(p: *mut Peer, server: u32, n: usize) {
    if (*p).state != CONNECTING || (*p).accepting {
        return;
    }
    let lp = (*p).lp;
    let rx = &(*lp.st()).buf;
    let len = rx[10] as usize;
    if let Some(c) = (*p).secured.clone() {
        // the second message: only the server holding the keys the Noise names can write it; it
        // names the server's connection id
        if len == 0 || n < 11 + len {
            return;
        }
        let mut payload = [0u8; 4];
        let ok = (*p).noise.as_mut().map(|h| h.read_message(&rx[11..11 + len], &mut payload)) == Some(Ok(4));
        if !ok || le32(&payload, 0) != server {
            return; // forged: the real ACCEPT may still come
        }
        let h = (*p).noise.take().unwrap();
        if !keyed(p, h, &c) {
            return failed(p, io::Error::new(io::ErrorKind::PermissionDenied, "the key the server proved is refused"));
        }
    } else if len != 0 {
        return; // a server that encrypts, and this client does not know its key
    }
    (*p).rcid = server;
    lp.disarm(p, HANDSHAKE);
    let done = (*p).on_connected.take();
    let a = (*p).attempts;
    open(p, Event::THIS_CONNECT);
    if a == 0 && (*p).state == ACTIVE {
        sample(p, lp.now() - (*p).initial_at); // no INITIAL repeated: the round trip is the one measured
    }
    if (*p).state == ACTIVE {
        // the server opens the connection on the first datagram of it: that is this question,
        // repeated until the server is heard
        (*p).confirming = true;
        (*p).attempts = 0;
        question(p);
        lp.arm(p, HANDSHAKE, lp.now() + (*p).host.handshake_retry);
    }
    if let Some(f) = done {
        if (*p).state == ACTIVE {
            f(Ok((*p).ext.clone()))
        } else {
            f(Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the connection was closed while it was being opened")))
        }
    }
}

unsafe fn send_accept(p: *mut Peer) {
    let lp = (*p).lp;
    if (*(*p).ep).held != 0 {
        return; // the socket holds a datagram back: the ACCEPT goes for the repeated INITIAL
    }
    let tx = &mut (*(*p).ep).tx;
    tx[0] = LONG | ACCEPT;
    tx[1] = VERSION;
    put32(tx, 2, (*p).rcid);
    put32(tx, 6, (*p).cid);
    tx[10] = (*p).hs_len as u8; // the second message of the handshake, written once, sent again for a repeated INITIAL
    tx[11..11 + (*p).hs_len].copy_from_slice(&(*p).hs[..(*p).hs_len]);
    lp.send((*p).ep, (*p).address.unwrap(), 11 + (*p).hs_len);
}

/// The handshake is done: the keys of the two directions. The server writes its reply here, the
/// second message. false - not done, or the key the peer proved is not let in ( `on_peer` of the
/// chain ).
unsafe fn keyed(p: *mut Peer, mut h: NoiseNK, chain: &Chain) -> bool {
    if !h.done() {
        // the server: its reply names its connection id, authenticated
        let payload = (*p).cid.to_le_bytes();
        (*p).hs_len = h.write_message(&mut (*p).hs, &payload).unwrap_or(0);
    }
    if !h.done() || !chain.accepts(&h) {
        return false; // its secrets go with it
    }
    let (send, receive) = h.split();
    h.wipe();
    (*p).tx_key = Some(send);
    (*p).rx_key = Some(receive);
    true
}

unsafe fn drop_keys(p: *mut Peer) {
    if let Some(k) = &mut (*p).tx_key {
        k.wipe();
    }
    if let Some(k) = &mut (*p).rx_key {
        k.wipe();
    }
    (*p).tx_key = None;
    (*p).rx_key = None;
    if let Some(n) = &mut (*p).noise {
        n.wipe();
    }
    (*p).noise = None;
    (*p).hs_len = 0;
}

/// The handshake never completed. A reconnect of a parked session waits on, for what is left of its
/// time.
unsafe fn failed(p: *mut Peer, e: io::Error) {
    // the caller hears it, the host does not: not a failure of the host
    let done = (*p).on_connected.take();
    let resumable = (*p).ext.resumable.load(Ordering::Relaxed);
    if resumable != 0 && now_sec() < resumable {
        park(p);
    } else if resumable != 0 {
        reap(p);
    } else {
        free(p);
    }
    if let Some(f) = done {
        f(Err(e));
    }
}

// the session

unsafe fn open(p: *mut Peer, event: Event) {
    let lp = (*p).lp;
    let ext = &(*p).ext;
    set_state(p, ACTIVE);
    (*p).closing_gracefully = false;
    ext.closing_gracefully.store(false, Ordering::Relaxed);
    ext.receive_timeout_ms
        .store(if (*p).host.idle_ms <= 0 { i32::MAX as i64 } else { (*p).host.idle_ms }, Ordering::Relaxed);
    ext.transmit_timeout_ms.store(i32::MAX as i64, Ordering::Relaxed);
    ext.resumable.store(0, Ordering::Relaxed); // live again, if it was parked
    ext.set_addresses((*(*p).ep).socket.local_addr().ok(), (*p).address); // Publish local and peer socket addresses.
    ext.is_open.store(true, Ordering::Release);
    let t = lp.now();
    (*p).received_at = t;
    (*p).keepalive = (*p).host.keepalive; // the session's own, if it has one, comes with its OnOpen
    heard(p, t);
    sent_at(p, t);
    reset(p, t);
    (*p).accepting = false;
    if let Some(src) = (*p).source.as_ref() {
        src.online();
    }
    (*p).host.live.fetch_add(1, Ordering::Relaxed);
    ext.fire(event); // the session opens with it
    if (*p).state != ACTIVE {
        return;
    }
    arm_deadline(p);
    // An identity kept across the handshakes - a reconnect of a parked session - resumes: the peer
    // is asked to go on from where the sink of this side is. A session identified later asks then
    // (`ExternalConnection::identify`).
    request(p, if (*p).ext.session() as u32 != 0 { CONTINUE_ | TX_ } else { TX_ });
}

/// Something came from the peer: the idle wait starts over. On a busy connection: two field writes
/// and a compare.
#[inline]
unsafe fn heard(p: *mut Peer, now: i64) {
    if (*p).confirming {
        (*p).confirming = false;
        (*p).lp.disarm(p, HANDSHAKE);
    }
    (*p).received_at = now;
    let ms = (*p).ext.receive_timeout_ms.load(Ordering::Relaxed);
    if ms < i32::MAX as i64 {
        (*p).lp.arm(p, RECEIVE, now + ms * TICKS_MS);
    }
}

#[inline]
unsafe fn sent_at(p: *mut Peer, t: i64) {
    (*p).lp.arm(p, KEEPALIVE, t + (*p).keepalive);
}

/// A connection starts: both streams from series 0, number 0. The receive position is the sink's:
/// it survives a reconnect.
unsafe fn reset(p: *mut Peer, now: i64) {
    (*p).tx_series = 0;
    (*p).tx_number = 0;
    (*p).tx_service = 0;
    (*p).rx_series = 0;
    (*p).rx_number = 0;
    (*p).wanted = 0;
    (*p).requested = 0;
    (*p).requested_once = false;
    (*p).rx_service = -1;
    (*p).lost_at = -1;
    (*p).told = -1;
    (*p).held_turn = None;
    (*p).failed = false;
    (*p).owed = false;
    (*p).asking = false;
    (*p).asked = false;
    (*p).asked_again = false;
    (*p).starting = false;
    (*p).close_pending = false;
    (*p).drained = false;
    (*p).backoff = 0;
    (*p).asks = 0;
    (*p).stale = 0;
    (*p).last_acked = -1;
    (*p).has_rtt = false;
    (*p).cwnd = 10 * (*p).host.mtu as i64;
    (*p).ssthresh = i64::MAX;
    (*p).reduced_at = now;
    (*p).tokens = (*p).cwnd as f64;
    (*p).tokens_at = now;
}

/// The session is over: the event to the application, the sink and the source dropped. `close` is
/// the code of the close the peer is sent, -1 - none; a graceful close lingers to repeat it,
/// anything else frees the slot now.
unsafe fn end(p: *mut Peer, event: Event, close: i64) {
    match (*p).state {
        FREE => return,
        CONNECTING => return failed(p, io::Error::new(io::ErrorKind::ConnectionAborted, "the connection was closed while connecting")),
        CLOSING => return free(p),
        PARKED => return reap(p),
        _ => {}
    }
    let lp = (*p).lp;
    let ext = (*p).ext.clone();
    ext.claim_close(event);
    let event = ext.close_event(); // whoever closed first names the event
    if 0 <= close {
        speak(p, close as i32);
    }
    // A loss that is not our own doing - the peer closing, or its silence - parks an identified
    // session for the seconds the protocol layer names, as over TCP; the close event is withheld
    // until the wait runs out (reap).
    let loss = !(*p).closing_gracefully && !ext.closing_gracefully.load(Ordering::Relaxed) && (event.is_remote() || event.is_timeout());
    let seconds = if loss && ext.session() as u32 != 0 { ext.resumable_seconds(event) } else { 0 };
    let resumable = if seconds <= 0 { 0 } else { now_sec() + seconds };
    ext.resumable.store(resumable, Ordering::Release);
    set_state(p, CLOSING); // before the handlers: a call from them finds the connection closed
    ext.is_open.store(false, Ordering::Release); // as its state says: parked, its sends go to the journal
    (*p).host.live.fetch_sub(1, Ordering::Relaxed);
    if resumable == 0 {
        quiet(p, || ext.fire(event));
    }
    quiet(p, || ext.session_closed());
    if resumable != 0 {
        if let Some(src) = (*p).source.as_mut() {
            // parked: the journal stays, and so does the receive position
            if !src.with_journal(&mut |j| {
                let _ = j.close(true);
            }) {
                if let Some(j) = src.journal() {
                    j.park(); // the offline pull holds it: parked as it lets it go
                }
            }
        }
        park(p);
        return;
    }
    (*p).sink = None;
    (*p).source = None;
    if close == 0 && (*p).state == CLOSING {
        lp.disarm(p, RECEIVE);
        lp.disarm(p, KEEPALIVE);
        let t = lp.now();
        (*p).linger_until = t + (*p).host.close_linger;
        (*p).close_repeats = 0;
        lp.arm(p, LINGER, (*p).linger_until.min(t + pto(p)));
    } else if (*p).state == CLOSING {
        free(p);
    }
}

/// The slot goes back to its host's free list; the stale entries in the deadline lists are dropped
/// when they come up.
unsafe fn free(p: *mut Peer) {
    let lp = (*p).lp;
    for k in 0..KINDS {
        lp.disarm(p, k);
    }
    forget_accepted(p);
    (*p).ext.reset(); // the session is over: its keys, its identity, its hooks, its deadline go - the next one finds the
                      // slot new
    set_state(p, FREE);
    (*p).address = None;
    (*p).ep = null_mut();
    drop_keys(p);
    (*p).secured = None;
    (*p).named = None;
    (*p).accepting = false;
    (*p).confirming = false;
    (*p).sink = None;
    (*p).source = None;
    (*p).tx_blocked = false;
    drop_tail(p);
    (*p).moved = None;
    (*p).claimable.store(false, Ordering::Relaxed);
    if let Some(f) = (*p).on_connected.take() {
        f(Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the connection is closed")));
    }
    lp.release(p);
}

/// A listening socket forgets the connection: a repeated INITIAL of the old handshake starts a new
/// one.
unsafe fn forget_accepted(p: *mut Peer) {
    let ep = (*p).ep;
    if !ep.is_null() && (*ep).listening {
        if let Some(a) = (*p).address {
            if (*ep).accepted.get(&(a, (*p).rcid)) == Some(&p) {
                (*ep).accepted.remove(&(a, (*p).rcid));
            }
        }
    }
}

/// The slot keeps its protocol layer, its journal and its receive position; nothing is sent or
/// received for it.
unsafe fn park(p: *mut Peer) {
    let lp = (*p).lp;
    set_state(p, PARKED);
    for k in 0..KINDS {
        lp.disarm(p, k);
    }
    forget_accepted(p);
    (*p).tx_blocked = false;
    drop_tail(p);
    let left = ((*p).ext.resumable.load(Ordering::Relaxed) * 1000 - now_ms()).max(0);
    lp.arm(p, PARK, lp.now() + left * TICKS_MS);
    (*p).claimable.store(true, Ordering::Release); // last: what a claim takes is written
}

/// The wait is over and the peer did not come back: the close event withheld at the loss, the
/// journal dropped, the slot freed.
unsafe fn reap(p: *mut Peer) {
    if (*p).state == PARKED && !(*p).claimable.swap(false, Ordering::AcqRel) {
        return; // claimed by a connection that reclaimed the session: the slot is freed when it moved
                // (`moved_out`)
    }
    (*p).ext.resumable.store(0, Ordering::Relaxed);
    set_state(p, CLOSING);
    quiet(p, || (*p).ext.fire(Event::REMOTE_CLOSE_ABRUPTLY));
    free(p);
}

/// Runs the code of the connection `p` - its sink and source, the handlers of the application under
/// them. A panic out of it goes to the handler of its host and the connection lives on.
#[inline]
unsafe fn guarded(p: *mut Peer, f: impl FnOnce()) {
    if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        panicked(p, &*why);
    }
}

#[cold]
#[inline(never)]
unsafe fn panicked(p: *mut Peer, why: &(dyn std::any::Any + Send)) {
    // told, and the connection lives on: what to do of it is the application's (the owner, 14.10)
    (*p).ext.failure(&super::failure::panicked(why));
}

/// Runs code of the application in the end of `p`: a panic out of it goes to the handler of its
/// host and the end goes on.
#[inline]
unsafe fn quiet(p: *mut Peer, f: impl FnOnce()) {
    if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        (*p).ext.failure(&super::failure::panicked(&*why));
    }
}

// the stream
//
// The bytes of the connection are the ones its source gives and its sink takes, as over TCP. What
// TCP gave for free - that a lost byte is sent again - comes from the journal. What is left here is
// to keep the two sides in step: the number the datagrams of a stream are numbered, so the receiver
// hands the sink only bytes that continue what it has; the series a datagram ahead of its number
// ends the series: the bytes are dropped, the side of the sink asks for the
//                next series from the byte it has got to, and the sender starts it there with number 0;
// the report how many bytes of the journal arrived; the question asks for a report at once.

/// The one-time number of a signature: a datagram with data has its series and number; one without
/// has a counter of its own, the top bit telling the two kinds apart.
const NO_DATA: u64 = 1 << 63;

#[inline]
fn once(series: i64, number: i64) -> u64 {
    (series as u64) << 40 | number as u64
}

unsafe fn sample(p: *mut Peer, ticks: i64) {
    if ticks <= 0 || 60_000 * TICKS_MS < ticks {
        return; // an answer of an earlier session, or a clock step
    }
    if !(*p).has_rtt {
        (*p).srtt = ticks;
        (*p).rttvar = ticks / 2;
        (*p).has_rtt = true;
        return;
    }
    (*p).rttvar = (3 * (*p).rttvar + ((*p).srtt - ticks).abs()) / 4;
    (*p).srtt = (7 * (*p).srtt + ticks) / 8;
}

/// The probe timeout, RFC 9002 without the ack delay: a question is answered at once.
#[inline]
unsafe fn pto(p: *mut Peer) -> i64 {
    (if (*p).has_rtt { (*p).srtt + (4 * (*p).rttvar).max(TICKS_MS) } else { 250 * TICKS_MS }).max(2 * TICKS_MS)
}

/// The whole series of an 8-bit wire value, nearest to `near`.
#[inline]
fn whole_series(near: i64, low8: u32) -> i64 {
    let mut s = near & !0xFF | (low8 & 0xFF) as i64;
    if s + 0x80 <= near {
        s += 0x100;
    } else if near + 0x80 < s && 0x100 <= s {
        s -= 0x100;
    }
    s
}

/// The whole number of a 24-bit wire value, nearest to `near`.
#[inline]
fn whole_number(near: i64, low24: u32) -> i64 {
    let mut n = near & !0xFF_FFFF | (low24 & 0xFF_FFFF) as i64;
    if n + 0x80_0000 <= near {
        n += 0x100_0000;
    } else if near + 0x80_0000 < n && 0x100_0000 <= n {
        n -= 0x100_0000;
    }
    n
}

/// The header of a datagram into `tx`; its first byte, the flags, is put last, when they are known.
#[inline]
fn put_header(tx: &mut [u8], rcid: u32, series: i64, number: i64, lost_at: i64) -> usize {
    put32(tx, 1, rcid);
    put32(tx, 5, series as u32 & 0xFF | (number as u32) << 8); // the series, the number: one word
    if lost_at != -1 && number == 0 {
        put64(tx, HEADER, lost_at);
        return HEADER + 8;
    }
    HEADER
}

#[inline]
unsafe fn header(p: *mut Peer) -> usize {
    put_header(&mut (*(*p).ep).tx, (*p).rcid, (*p).tx_series, (*p).tx_number, (*p).lost_at)
}

/// What this side has to say of the stream it receives, at `pos`: the series it asks for and the
/// position it asks it from
/// - said in every datagram until the series starts; or the position its sink has got to, when the
///   peer asked or the
/// position moved. And the question, when one is to go out.
unsafe fn say(p: *mut Peer, mut pos: usize) -> usize {
    let lp = (*p).lp;
    let tx = &mut (*(*p).ep).tx;
    (*p).said = if (*p).lost_at != -1 && (*p).tx_number == 0 { LOST } else { 0 };
    let at = position(p);
    if (*p).wanted != (*p).rx_series {
        put64(tx, pos, at);
        tx[pos + 8] = (*p).wanted as u8;
        pos += 9;
        (*p).said |= REPORT | CONTINUE;
        (*p).told = at;
        (*p).requested_once = (*p).requested != (*p).wanted;
        if (*p).requested_once {
            (*p).requested = (*p).wanted;
            (*p).requested_at = lp.now();
        }
        if !(*p).dl[REQUEST].armed {
            lp.arm(p, REQUEST, lp.now() + (pto(p) << (*p).asks.min(6))); // said again until the series starts
        }
    } else if at != -1 && ((*p).owed || at != (*p).told) {
        put32(tx, pos, at as u32);
        pos += 4;
        (*p).said |= REPORT;
        (*p).told = at;
    }
    if (*p).owed {
        (*p).said |= ANSWER;
        (*p).owed = false;
    }
    if (*p).asking {
        (*p).said |= QUESTION;
        (*p).asked_again = (*p).asked; // the round trip of a question said again is nobody's
        (*p).asked = true;
        (*p).asking = false;
        (*p).asked_at = lp.now();
        (*p).asked_from = journal(p).map_or(0, |j| j.sent());
    }
    pos
}

/// The datagram being made, `pos` bytes of header and nothing behind them, goes out.
unsafe fn bare(p: *mut Peer, pos: usize) {
    let lp = (*p).lp;
    let tx = &mut (*(*p).ep).tx;
    tx[0] = (*p).said;
    let s = (*p).tx_service;
    (*p).tx_service += 1;
    send(p, pos, NO_DATA | s as u64);
    if (*p).state == ACTIVE {
        sent_at(p, lp.now());
    }
}

/// A datagram without data: what [`say`] has to say, and a close when `close` is its code. Not
/// while the socket holds a datagram back: as one lost on the way - what it would say is said by
/// the next one.
unsafe fn speak(p: *mut Peer, close: i32) {
    if (*(*p).ep).held != 0 {
        return;
    }
    let mut pos = say(p, header(p));
    if 0 <= close {
        (*p).said |= CLOSE;
        (&mut (*(*p).ep).tx)[pos] = close as u8;
        pos += 1;
    }
    bare(p, pos);
}

/// A question on its own: keeps the path, measures the round trip, shows the peer the number it
/// should have got to.
unsafe fn question(p: *mut Peer) {
    (*p).asking = true;
    speak(p, -1);
}

/// The datagram of `len` bytes being made goes out, sealed with `once` when the connection has a
/// key: the header clear, the tag of it and of the rest, the rest encrypted in place, the tag behind
/// it; one without data has the low 24 bits of its counter behind the tag. False - the socket has no
/// room for it now: it holds it, and makes nothing more until it is out.
#[inline]
unsafe fn send(p: *mut Peer, mut len: usize, once: u64) -> bool {
    let lp = (*p).lp;
    if let Some(k) = &mut (*p).tx_key {
        let tx = &mut (*(*p).ep).tx;
        let (head, rest) = tx.split_at_mut(len);
        let (ad, text) = head.split_at_mut(HEADER);
        k.seal(once, ad, text, (&mut rest[..TAG]).try_into().unwrap()); // the tag right behind the text: no copy
        len += TAG;
        if once & NO_DATA != 0 {
            tx[len] = once as u8;
            tx[len + 1] = (once >> 8) as u8;
            tx[len + 2] = (once >> 16) as u8;
            len += 3;
        }
    }
    lp.send((*p).ep, (*p).address.unwrap(), len)
}

#[inline]
unsafe fn payload_max(p: *mut Peer) -> usize {
    if (*p).tx_key.is_none() {
        (*p).host.mtu
    } else {
        (*p).host.mtu - TAG
    }
}

/// The journal moved up: the silence is over.
unsafe fn progress(p: *mut Peer, now: i64) {
    let Some((acked, next)) = journal(p).map(|j| (j.acked(), j.next())) else { return };
    if acked == (*p).last_acked {
        return;
    }
    if 0 <= (*p).last_acked && (*p).last_acked < acked {
        grown(p, acked - (*p).last_acked);
    }
    (*p).last_acked = acked;
    (*p).backoff = 0;
    let lp = (*p).lp;
    if next <= acked {
        lp.disarm(p, PROBE);
    } else {
        lp.arm(p, PROBE, now + pto(p));
    }
}

/// Fills datagrams from the source until it is dry or the window is full, a bounded burst per call.
/// What this side has to say of the stream it receives rides along.
unsafe fn transmit(p: *mut Peer) {
    if (*p).state != ACTIVE {
        return;
    }
    let lp = (*p).lp;
    let now = lp.now();
    if !(*p).close_pending && closing(&(*p).ext) {
        (*p).close_pending = true;
        (*p).closing_gracefully = true;
    }
    (*p).drained = (*p).source.is_none(); // nothing to send, ever
    let mut burst = 0;
    while !(*p).drained {
        if burst == 64 {
            request(p, TX_); // the other connections of the loop first
            break;
        }
        burst += 1;
        if (*(*p).ep).held != 0 {
            wait(p); // the socket holds a datagram back: this one goes on once it is out
            break;
        }
        if let Some(out) = journal(p).map(|j| j.sent() - j.acked()) {
            if window(p) <= out {
                if !(*p).asked {
                    question(p); // the window is full and nobody was asked: the report that opens it is asked for
                }
                break;
            }
            if 0 < out && !(*p).asked {
                (*p).asking = true; // a question is out all the time bytes are: a report a round trip
            }
        }
        if !may_transmit(p, now, (*p).host.mtu) {
            break; // the pacing: the PACE deadline goes on
        }
        let pos = say(p, header(p));
        let max = payload_max(p);
        // the replay the journal owes first, then new packs - the guaranteed ones journalled as
        // they go: the source's own
        let n = match (*p).source.as_mut() {
            Some(src) => src.read(&mut (&mut (*(*p).ep).tx)[pos..max]),
            None => -1,
        };
        if (*p).state != ACTIVE {
            return;
        }
        if n <= 0 {
            (*p).drained = true;
            if (*p).said & !LOST != 0 || (*p).starting {
                bare(p, pos); // said already: it goes out without data
            }
            (*p).starting = false;
            break;
        }
        (*p).starting = false;
        let len = pos + n as usize;
        (&mut (*(*p).ep).tx)[0] = (*p).said | DATA;
        let o = once((*p).tx_series, (*p).tx_number);
        (*p).tx_number += 1;
        send(p, len, o); // sent, or held by the socket and sent as it is: its number is spent either way
        (*p).tokens -= len as f64;
        sent_at(p, now);
        if !(*p).dl[PROBE].armed && journal(p).is_some_and(|j| j.acked() < j.next()) {
            lp.arm(p, PROBE, now + (pto(p) << (*p).backoff));
        }
    }
    if (*p).starting {
        // the series asked for has nothing to start with yet: the peer is told it started, or it
        // would ask again
        (*p).starting = false;
        speak(p, -1);
    }
    if !(*p).dl[PROBE].armed && journal(p).is_some_and(|j| j.acked() < j.next()) {
        lp.arm(p, PROBE, now + (pto(p) << (*p).backoff)); // also a journal waiting for the peer to name its position
    }
    if (*p).close_pending && (*p).drained {
        if journal(p).is_none_or(|j| j.next() <= j.acked()) {
            end(p, Event::THIS_CLOSE_GRACEFUL, 0);
        } else if !(*p).asked {
            question(p); // the close waits for the report of what was sent
        }
    }
}

/// The socket holds a datagram back: the connection waits for it, and transmits again once it is
/// out.
#[cold]
unsafe fn wait(p: *mut Peer) {
    if !(*p).tx_blocked {
        (*p).tx_blocked = true;
        (*(*p).ep).blocked.push(p);
    }
}

/// The peer asks for `series` of the stream this side sends, from `position` of the journal. A
/// series not above the one that runs was asked for before, and runs.
unsafe fn turn(p: *mut Peer, position: i64, series: i64, now: i64) {
    if series <= (*p).tx_series {
        return;
    }
    if journaled(p) && (*p).ext.session() as u32 == 0 {
        (*p).held_turn = Some((position, series)); // no session here yet: whose journal answers is not known
        return;
    }
    (*p).lost_at = -1;
    if let Some(src) = (*p).source.as_mut() {
        if src.journal().is_some() {
            if !src.holds_journal() {
                (*p).held_turn = Some((position, series)); // the offline pull holds the journal: asked again as it lets it go
                return;
            }
            match src.turn(position) {
                Ok(at) => (*p).lost_at = at, // -1: the journal holds it; otherwise where it goes on from, told with the start
                Err(e) => {
                    (*p).ext.failure(&e);
                    end(p, Event::THIS_CLOSE_ABRUPTLY, 1);
                    return;
                }
            }
        } else {
            src.abandon(); // no journal: the pack cut is lost, the stream goes on with the next one
        }
    }
    if 0 < (*p).tx_series {
        // the first series a connection is asked for is where its session goes on; every next one
        // is a loss
        (*p).lost_datagrams += 1;
        reduced(p, now);
    }
    (*p).tx_series = series;
    (*p).tx_number = 0;
    (*p).starting = true;
    (*p).last_acked = journal(p).map_or(-1, |j| j.acked());
    (*p).backoff = 0;
    (*p).asked = false; // the question that is out was of the series that is over
    (*p).asked_again = false;
}

/// A datagram of this connection came, `n` bytes in the receive buffer. Its numbers are made whole.
/// One of a series that is over, or with a number that was heard, is late or came again: dropped
/// whole, unread. The others are opened in place: a forged or damaged one is dropped.
unsafe fn incoming(p: *mut Peer, now: i64, mut n: usize) {
    let rx = &mut (*(*p).lp.st()).buf;
    let flags = rx[0];
    let data = flags & DATA != 0;
    let word = le32(rx, 5); // the series, the number
    let series = whole_series((*p).wanted, word & 0xFF);
    let near = if series == (*p).rx_series { (*p).rx_number } else { 0 };
    let number = whole_number(near, word >> 8);
    if (*p).state == ACTIVE && (series < (*p).rx_series || series == (*p).rx_series && data && number < (*p).rx_number) {
        (*p).replayed += 1;
        return;
    }
    if let Some(key) = &mut (*p).rx_key {
        if n < TAG + HEADER + if data { 0 } else { 3 } {
            (*p).forged += 1;
            return;
        }
        let mut o = once(series, number);
        let mut service = 0;
        if !data {
            n -= 3; // the counter, behind the tag
            let next = (*p).rx_service + 1;
            service = whole_number(next, rx[n] as u32 | (rx[n + 1] as u32) << 8 | (rx[n + 2] as u32) << 16);
            if service <= (*p).rx_service {
                (*p).replayed += 1;
                return;
            }
            o = NO_DATA | service as u64;
        }
        n -= TAG;
        let (ad, rest) = rx.split_at_mut(HEADER);
        let (text, rest) = rest.split_at_mut(n - HEADER);
        if !key.open(o, ad, text, (&rest[..TAG]).try_into().unwrap()) {
            (*p).forged += 1;
            return;
        }
        if !data {
            (*p).rx_service = service;
        }
    }
    if (*p).state == CONNECTING {
        if !(*p).accepting {
            return;
        }
        confirmed(p); // the ACCEPT arrived: the handshake is complete, as a TCP accept is
        if (*p).state != ACTIVE {
            return;
        }
    } else if (*p).state < ACTIVE {
        return;
    }
    received(p, now, flags, n, series, number);
}

/// A datagram of this connection, `end` bytes of it, opened - its tag and counter taken off. The
/// header is read whatever the state of the sink is: what it says of the stream this side sends
/// does not depend on the stream this side receives. The bytes go to the sink only when they
/// continue what it has.
unsafe fn received(p: *mut Peer, now: i64, flags: u8, end: usize, series: i64, number: i64) {
    if (*p).state == CLOSING {
        speak(p, 0); // it has not heard our close
        return;
    }
    heard(p, now);
    let lp = (*p).lp;
    let rx = &mut (*lp.st()).buf;
    let data = flags & DATA != 0;
    let mut pos = HEADER;

    // the stream this side receives
    let mut lost = -1;
    let mut accept = false;
    let mut again = false;
    if flags & LOST != 0 {
        if end < pos + 8 {
            return;
        }
        lost = le64(rx, pos);
        pos += 8;
    }
    if series == (*p).rx_series {
        if number == (*p).rx_number && !(*p).failed {
            accept = data;
        } else if (*p).rx_number < number {
            // The series goes on behind its failure: the peer has not heard the request. Said again
            // at the 1st, the 2nd, the 4th, the 8th... datagram of the kind: a window in flight
            // costs a few requests, not one each.
            if (*p).failed && data {
                (*p).stale += 1;
                again = (*p).stale & ((*p).stale - 1) == 0;
            }
            fail(p); // a datagram ahead of its number: something was lost
        }
    } else if series == (*p).wanted {
        if number == 0 {
            // the series asked for starts
            if (*p).requested_once && (*p).requested == series {
                sample(p, now - (*p).requested_at); // the request, and the start it brought
            }
            (*p).rx_series = series;
            (*p).rx_number = 0;
            (*p).failed = false;
            (*p).asks = 0;
            (*p).stale = 0;
            lp.disarm(p, REQUEST);
            if let Some(s) = &mut (*p).sink {
                if lost == -1 {
                    s.continue_(); // the bytes go on from the position the decoder named
                } else {
                    // the peer's journal does not reach the position asked for: the packs between
                    // are lost
                    s.lost(lost);
                    (*p).told = -1;
                }
            }
            if (*p).state != ACTIVE {
                return;
            }
            accept = data;
        } else {
            // its start was lost: the next one is asked for
            (*p).failed = true;
            (*p).asks = 0;
            (*p).stale = 0;
            (*p).wanted += 1;
            (*p).gaps += 1;
            lp.disarm(p, REQUEST);
        }
    } else if data {
        // a series between the two: its start was lost, the next was asked for - and the peer has
        // not heard that yet
        (*p).stale += 1;
        again = (*p).stale & ((*p).stale - 1) == 0;
    }

    // the stream this side sends
    let mut reported = i64::MAX; // none: nothing to tell a late answer by
    if flags & REPORT != 0 {
        if flags & CONTINUE != 0 {
            if end < pos + 9 {
                return;
            }
            let position = le64(rx, pos);
            reported = i64::MIN; // a request answers no question
            let s = whole_series((*p).tx_series, rx[pos + 8] as u32);
            pos += 9;
            turn(p, position, s, now);
            if (*p).state != ACTIVE {
                return;
            }
        } else {
            if end < pos + 4 {
                return;
            }
            if 0 < (*p).tx_series {
                if let Some(src) = (*p).source.as_mut() {
                    // before the peer asked for a series its position is not of this journal
                    let r = le32(rx, pos) as i32;
                    src.with_journal(&mut |j| {
                        reported = j.next() + r.wrapping_sub(j.next() as i32) as i64;
                        j.reported(r);
                    });
                }
            }
            pos += 4;
        }
    }
    // An answer reports what the sink had when the question came. One that reports less answers an
    // earlier question, or came again: it is not the answer waited for.
    if flags & ANSWER != 0 && (*p).asked && (*p).asked_from <= reported {
        if !(*p).asked_again {
            sample(p, now - (*p).asked_at);
        }
        (*p).asked = false;
        (*p).asked_again = false;
    }
    if flags & CLOSE != 0 {
        let ev = if pos < end && rx[pos] == 0 { Event::REMOTE_CLOSE_GRACEFUL } else { Event::REMOTE_CLOSE_ABRUPTLY };
        self::end(p, ev, -1);
        return;
    }
    if flags & QUESTION != 0 {
        (*p).owed = true;
    }

    if accept {
        (*p).rx_number += 1;
        if pos < end {
            if !(*p).tail.is_empty() {
                behind(p, pos, end); // the sink has what it did not take yet: these go behind, in order
            } else if let Some(s) = &mut (*p).sink {
                (*p).in_sink = true;
                // straight from the datagram in hand; a panic of the sink goes on up past the flag put down
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.on_slice(&mut rx[pos..end])));
                (*p).in_sink = false;
                let r = r.unwrap_or_else(|why| std::panic::resume_unwind(why));
                if (*p).state != ACTIVE {
                    return;
                }
                match r {
                    Some(took) => {
                        if took < end - pos {
                            keep(p, pos + took, end); // no room in the sink now: offered again once it has room
                        }
                    }
                    None => return broken(p),
                }
                if (*p).moved.is_some() {
                    install(p); // a handler of the pack reclaimed a parked session
                }
            }
        }
    }

    progress(p, now);
    if (*p).owed || flags & (REPORT | ANSWER) != 0 || (*p).starting {
        transmit(p); // the answer rides with the data, if there is any; what the peer said may have opened the window
        if (*p).state != ACTIVE {
            return;
        }
    }
    if (*p).owed || (*p).wanted != (*p).rx_series && (again || !(*p).dl[REQUEST].armed) {
        speak(p, -1); // the side of the sink has to be heard now: the answer to a question, or the series it asks for
    }
}

/// The stream received is broken: the sink gets nothing until a series asked for starts. The next
/// one is asked for, unless one is asked for already.
unsafe fn fail(p: *mut Peer) {
    drop_tail(p); // the next series starts at what the sink took: what it did not would come twice
    if (*p).failed {
        return;
    }
    (*p).failed = true;
    (*p).gaps += 1;
    if (*p).wanted != (*p).rx_series {
        return;
    }
    (*p).wanted = (*p).rx_series + 1;
    (*p).asks = 0;
    (*p).stale = 0;
    (*p).lp.disarm(p, REQUEST);
}

/// The sink took the datagram in hand up to `at` of `end`: the datagram stays with the connection -
/// the buffer it came in, as it is - and the loop receives into another from its pool. A relay
/// before its outbound is there takes nothing for a few passes; the socket is read on meanwhile,
/// the other connections on it do not wait.
#[cold]
unsafe fn keep(p: *mut Peer, at: usize, end: usize) {
    let st = (*p).lp.st();
    let b = std::mem::replace(&mut (*st).buf, (*st).pool.pop().unwrap_or_else(|| vec![0u8; RX_BUF].into_boxed_slice()));
    (*p).tail.push_back((b, end));
    (*p).tail_at = at;
}

/// The bytes `[pos, end)` of the datagram in hand go behind what the sink has not taken. No more
/// than the sender may have out unreported - the window - comes so; beyond it (a sender without a
/// journal has no window) the stream is broken, as by a loss.
#[cold]
unsafe fn behind(p: *mut Peer, pos: usize, end: usize) {
    let st = (*p).lp.st();
    let n = end - pos;
    let (last, fill) = (*p).tail.back_mut().unwrap();
    if n <= last.len() - *fill {
        last[*fill..*fill + n].copy_from_slice(&(&(*st).buf)[pos..end]);
        *fill += n;
        return;
    }
    if (*p).host.window.max(0) as usize / RX_BUF + 2 <= (*p).tail.len() {
        (*p).refused += 1;
        fail(p);
        return;
    }
    let mut b = (*st).pool.pop().unwrap_or_else(|| vec![0u8; RX_BUF].into_boxed_slice());
    b[..n].copy_from_slice(&(&(*st).buf)[pos..end]);
    (*p).tail.push_back((b, n));
}

/// The sink could not parse the stream: the connection ends. Asking the series again would not mend
/// it - the same bytes would come, and the sink keeps its error.
#[cold]
unsafe fn broken(p: *mut Peer) {
    (*p).refused += 1;
    fail(p); // Request the next series only if the decoder has not already aborted the connection.
    end(p, Event::THIS_CLOSE_ABRUPTLY, 1);
}

/// The sink has room again (`wake_receive`): what it did not take is offered to it, in order. What
/// it takes now is reported at once - the window of the sender opens.
unsafe fn drain(p: *mut Peer) {
    if (*p).tail.is_empty() {
        return;
    }
    let before = position(p);
    while !(*p).tail.is_empty() {
        let Some(s) = &mut (*p).sink else { break };
        let at = (*p).tail_at;
        let (b, fill) = &mut (*p).tail[0];
        let len = *fill - at;
        (*p).in_sink = true;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.on_slice(&mut b[at..*fill])));
        (*p).in_sink = false; // a panic of the sink goes on up past the flag put down
        let r = r.unwrap_or_else(|why| std::panic::resume_unwind(why));
        if (*p).moved.is_some() {
            install(p); // a handler of the pack reclaimed a parked session: what was kept is not its own
            return;
        }
        if (*p).state != ACTIVE {
            return;
        }
        let Some(took) = r else { return broken(p) };
        if took < len {
            (*p).tail_at = at + took; // no room again: the next wake_receive
            break;
        }
        let (b, _) = (*p).tail.pop_front().unwrap(); // a ring: no shift of those behind
        give(p, b);
        (*p).tail_at = 0;
    }
    if position(p) != before {
        speak(p, -1);
    }
}

/// What the sink did not take is dropped: its buffers back to the pool of the loop.
unsafe fn drop_tail(p: *mut Peer) {
    while let Some((b, _)) = (*p).tail.pop_back() {
        give(p, b);
    }
    (*p).tail_at = 0;
}

#[inline]
unsafe fn give(p: *mut Peer, b: Box<[u8]>) {
    let pool = &mut (*(*p).lp.st()).pool;
    if pool.len() < POOL_KEEP {
        pool.push(b);
    }
}

/// The session the token names, parked on a slot of this host on any loop, moves onto `p`: claimed
/// against its loop's reaper, its sink, source, journal and receive position taken - put in place
/// once the sink of `p` is out of its call -, its slot freed on its own loop. Either way `p` takes
/// the identity of the token.
unsafe fn reclaim(p: *mut Peer, token: u64) {
    let id = token as u32;
    let at = (token >> 32) as u32;
    // also before the client confirmed the handshake: once it opens, the session it took asks to go
    // on
    if ((*p).state == ACTIVE || (*p).state == CONNECTING) && at != (*p).ext.index.0 {
        if let Some(other) = (*p).host.engine.loops.get((at >> SLOT_BITS) as usize) {
            let q = other.shared.read().get((at & SLOT_MASK) as usize).copied().unwrap_or(0) as *mut Peer;
            // SAFETY: `host` and `ext` of a slot change only on its own loop after its session is
            // claimed; the rest of a parked slot is its loop's no more once the claim is won here -
            // the loser of the claim leaves the slot alone
            if !q.is_null() && Arc::ptr_eq(&(*q).host, &(*p).host) && (*q).ext.session() as u32 == id && (*q).claimable.compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                (*p).moved = Some(Box::new(Moved {
                    sink: (*q).sink.take(),
                    source: (*q).source.take(),
                    ext: (*q).ext.clone(),
                    keys: (*q).ext.keys(),
                    id,
                }));
                let q = q as usize;
                other.submit(Box::new(move |_| moved_out(q as *mut Peer)));
                if !(*p).in_sink {
                    install(p);
                }
                return;
            }
        }
    }
    (*p).ext.identify(id);
}

/// The claimed session takes this connection over: its protocol layer is seated here (`OnClose` for
/// the one seated here before, `OnOpen` for it), its sink, source and journal drive this
/// connection, its keys get the key of this connection, and the peer is asked to go on from its
/// receive position - what is left of the series that runs is not its own: dropped.
unsafe fn install(p: *mut Peer) {
    let Some(m) = (*p).moved.take() else { return };
    let ext = (*p).ext.clone();
    let open = ext.is_session_open(); // Only an already-open session receives the swap event; a new one receives the connect event.
    ext.session_closed();
    m.ext.hand_over(&ext);
    ext.set_keys(m.keys); // the keys of the session that came: a key added from now on is its own
    (*p).sink = m.sink;
    (*p).source = m.source;
    drop_tail(p); // what the decoder that left did not take is its own
    if let Some(src) = (*p).source.as_ref() {
        src.online();
    }
    if open {
        ext.session_opened();
    }
    ext.unidentify();
    ext.identify(m.id); // the peer is asked to go on from the receive position of the session
                        // the deadlines of the session - of its instances, of its stage - were the timer of the
                        // connection it left: filed here; a connection still connecting files them as it opens
    ext.refile_deadline();
    arm_deadline(p);
}

/// Releases slot `q` after its parked session moves to another connection. Cleanup runs on the slot
/// loop. The session takes its keys; the slot resets keys and hooks so a later session cannot
/// inherit them.
unsafe fn moved_out(q: *mut Peer) {
    (*q).ext.renew_keys();
    (*q).ext.reset();
    (*q).state = CLOSING;
    free(q);
}

/// The peer is asked to go on from the position of this side's sink: the session goes on on this
/// connection. Called once the session is identified ( the generated resume(connection) asks ): the
/// agreement starts here - see the order in the description of the module resumable.
///
/// The first series of the connection is what the side that made it sent before it heard the
/// position: its login and the packs of no guarantee behind it - a guaranteed pack is only
/// journalled until then, and a pack of no guarantee the former socket cut is dropped there. It
/// starts with a pack, so a sink between two packs reads it on until the series asked for starts: no
/// guaranteed byte of it comes twice. A sink that holds a pack in progress holds it from the socket
/// the session was parked from: it would take that series for the rest of the pack, and the stream
/// would be broken from there on. Then, and once a series was asked for on this connection, the
/// datagrams of the series that runs are not read from now on - nothing is read until the series
/// asked for starts: it goes on from the position, the pack in progress gets its own rest.
unsafe fn ask(p: *mut Peer) {
    if (*p).state != ACTIVE {
        return;
    }
    if (*p).wanted != 0 || (*p).sink.as_ref().is_some_and(|s| !s.idle()) {
        drop_tail(p); // the peer goes on from what the sink took
        (*p).failed = true; // the series that runs is not from the position asked
    }
    if (*p).wanted == (*p).rx_series {
        (*p).wanted = (*p).rx_series + 1;
        (*p).asks = 0;
        (*p).stale = 0;
        (*p).lp.disarm(p, REQUEST);
    }
    speak(p, -1);
}

// the window: NewReno on what the reports tell. A report moves it up, a series the peer asks for -
// a loss - halves it, once a round trip. The pacing spreads what the window lets through.

#[inline]
unsafe fn window(p: *mut Peer) -> i64 {
    if (*p).host.congestion {
        (*p).cwnd.min((*p).host.window)
    } else {
        (*p).host.window
    }
}

unsafe fn grown(p: *mut Peer, bytes: i64) {
    if (*p).cwnd < (*p).ssthresh {
        (*p).cwnd += bytes; // slow start
    } else {
        (*p).cwnd += (*p).host.mtu as i64 * bytes / (*p).cwnd; // congestion avoidance
    }
    if (*p).host.window < (*p).cwnd {
        (*p).cwnd = (*p).host.window;
    }
}

unsafe fn reduced(p: *mut Peer, now: i64) {
    if now - (*p).reduced_at < if (*p).has_rtt { (*p).srtt } else { 0 } {
        return; // one reduction for a whole round of losses
    }
    (*p).reduced_at = now;
    (*p).cwnd = ((*p).cwnd / 2).max(2 * (*p).host.mtu as i64);
    (*p).ssthresh = (*p).cwnd;
}

/// Room for `size` more bytes in the pacing budget.
#[inline]
unsafe fn may_transmit(p: *mut Peer, now: i64, size: usize) -> bool {
    if !(*p).host.congestion || !(*p).has_rtt {
        return true;
    }
    let rate = 1.25 * (*p).cwnd as f64 / (*p).srtt as f64; // bytes a tick
    (*p).tokens = ((*p).tokens + (now - (*p).tokens_at) as f64 * rate).min((10.0 * (*p).host.mtu as f64).max(rate * 2. * TICKS_MS as f64));
    (*p).tokens_at = now;
    if size as f64 <= (*p).tokens {
        return true;
    }
    (*p).lp.arm(p, PACE, now + ((size as f64 - (*p).tokens) / rate) as i64 + 1);
    false
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn wire_constants_match_csharp() {
        assert_eq!(LONG, 0x80);
        assert_eq!(VERSION, 2);
        assert_eq!((INITIAL, RETRY, ACCEPT), (0, 1, 2));
        assert_eq!((DATA, QUESTION, REPORT, CONTINUE, LOST, CLOSE, ANSWER), (0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40));
        assert_eq!((MIN_INITIAL, TOKEN, TOKEN_SECONDS, HEADER, TAG), (1200, 16, 10, 9, 16));
        assert_eq!(SLOT_MASK, (1 << 20) - 1);
        assert_eq!(BEAT, 1 << 19, "the largest power of two of nanoseconds not over a millisecond");
    }

    #[test]
    fn prologue_layout() {
        let p = prologue(0xA1B2C3D4);
        assert_eq!(&p[..9], b"AdHoc UDP");
        assert_eq!(p[9], VERSION);
        assert_eq!(le32(&p, 10), 0xA1B2C3D4);
    }

    #[test]
    fn series_number_word_layout() {
        let mut tx = [0u8; 32];
        assert_eq!(put_header(&mut tx, 0xDEADBEEF, 0x12, 0x345678, -1), HEADER);
        assert_eq!(le32(&tx, 1), 0xDEADBEEF);
        assert_eq!(tx[5], 0x12, "series: the low byte");
        assert_eq!(&tx[6..9], &[0x78, 0x56, 0x34], "number: 24 bits, little-endian");
        assert_eq!(put_header(&mut tx, 1, 3, 0, 77), HEADER + 8, "the lost position rides with the start of a series");
        assert_eq!(le64(&tx, HEADER), 77);
    }

    #[test]
    fn whole_series_wraps_near_wanted() {
        assert_eq!(whole_series(0, 0), 0);
        assert_eq!(whole_series(0, 1), 1);
        assert_eq!(whole_series(1, 0xFF), 0xFF);
        assert_eq!(whole_series(0x100, 0xFF), 0xFF);
        assert_eq!(whole_series(0x100, 0x01), 0x101);
        assert_eq!(whole_series(0x181, 0x00), 0x200);
        assert_eq!(whole_series(0x181, 0xFF), 0x1FF);
    }

    #[test]
    fn whole_number_wraps_near() {
        assert_eq!(whole_number(0, 5), 5);
        assert_eq!(whole_number(0x100_0000, 0xFF_FFFF), 0xFF_FFFF);
        assert_eq!(whole_number(0x100_0005, 1), 0x100_0001);
        assert_eq!(whole_number(0, 0xFF_FFFF), 0xFF_FFFF);
        assert_eq!(whole_number(0x180_0001, 0xFF_FFFF), 0x1FF_FFFF);
    }

    #[test]
    fn token_roundtrip_and_forgery() {
        let host = host::Builder::new("t").build();
        let from = SocketAddr::new(Ipv4Addr::new(1, 2, 3, 4).into(), 5000);
        let mut tok = [0u8; TOKEN];
        host.inner.token_make(&mut tok, &from, 42);
        assert!(host.inner.token_valid(&tok, &from, 42));
        let other = SocketAddr::new(Ipv4Addr::new(1, 2, 3, 5).into(), 5000);
        assert!(!host.inner.token_valid(&tok, &other, 42));
        assert!(!host.inner.token_valid(&tok, &from, 43));
        tok[8] ^= 1;
        assert!(!host.inner.token_valid(&tok, &from, 42));
    }

    #[test]
    fn token_of_another_secret_is_not_valid() {
        let a = host::Builder::new("a").build();
        let b = host::Builder::new("b").build();
        let from = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 9000);
        let mut tok = [0u8; TOKEN];
        a.inner.token_make(&mut tok, &from, 7);
        assert!(a.inner.token_valid(&tok, &from, 7));
        assert!(!b.inner.token_valid(&tok, &from, 7));
    }

    /// The bytes received, collected.
    struct Collect(Arc<std::sync::Mutex<Vec<u8>>>);

    impl BytesDst for Collect {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.0.lock().unwrap().extend_from_slice(bytes);
            true
        }
    }

    /// The bytes to send: what the test put there.
    struct Out(Arc<std::sync::Mutex<(Vec<u8>, usize)>>);

    impl BytesSrc for Out {
        fn read(&mut self, dst: &mut [u8]) -> isize {
            let mut b = self.0.lock().unwrap();
            let (bytes, at) = &mut *b;
            if *at == bytes.len() {
                return -1;
            }
            let n = (bytes.len() - *at).min(dst.len());
            dst[..n].copy_from_slice(&bytes[*at..*at + n]);
            *at += n;
            n as isize
        }
    }

    /// The bytes to send, journalled as the source of a session journals its guaranteed packs: what
    /// a loss cuts the transport turns the stream back to.
    struct Journaled {
        out: Outbox,
        j: Journal,
    }

    impl Journaled {
        fn new(out: Outbox) -> Self {
            Self {
                out,
                j: Journal::new(Resumable::new(1 << 20)),
            }
        }
    }

    impl Journaled {
        fn fill(&mut self, dst: &mut [u8]) -> isize {
            // SAFETY: the only holder
            let j = unsafe { self.j.get() };
            match j.replay_into(dst) {
                Ok(0) => {}
                Ok(n) => return n as isize,
                Err(_) => return -1,
            }
            loop {
                if !j.has_room() {
                    return 0;
                }
                let n = Out(self.out.clone()).read(dst);
                if n <= 0 {
                    return n;
                }
                match j.journal(&dst[..n as usize]) {
                    Ok(true) => return n,
                    Ok(false) => {} // held: the peer has not named its position yet
                    Err(_) => return -1,
                }
            }
        }
    }

    impl BytesSrc for Journaled {
        fn read(&mut self, dst: &mut [u8]) -> isize {
            let r = self.fill(dst);
            self.j.publish();
            r
        }

        fn journal(&self) -> Option<&Journal> {
            Some(&self.j)
        }

        fn with_journal(&mut self, f: &mut dyn FnMut(&mut Resumable)) -> bool {
            // SAFETY: the only holder
            f(unsafe { self.j.get() });
            self.j.publish();
            true
        }

        fn turn(&mut self, position: i64) -> io::Result<i64> {
            // SAFETY: the only holder
            let at = unsafe { self.j.get() }.turn(position);
            self.j.publish();
            Ok(at)
        }
    }

    /// The bytes received, collected, counted as a decoder counts the bytes of guaranteed packs:
    /// from the position the peer's journal names.
    struct Counting(Inbox, i64);

    impl Counting {
        fn new(got: Inbox) -> Self {
            Self(got, -1)
        }
    }

    impl BytesDst for Counting {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.0.lock().unwrap().extend_from_slice(bytes);
            if 0 <= self.1 {
                self.1 += bytes.len() as i64;
            }
            true
        }

        fn lost(&mut self, position: i64) {
            self.1 = position;
        }

        fn received(&self) -> i64 {
            self.1
        }
    }

    /// The bytes received, collected - no more than the room it has: a relay before its outbound is
    /// there.
    struct Slow(Inbox, Arc<AtomicUsize>);

    impl BytesDst for Slow {
        fn on_bytes(&mut self, _: &[u8]) -> bool {
            unreachable!()
        }

        fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
            let n = self.1.load(Ordering::Acquire).min(bytes.len());
            self.1.fetch_sub(n, Ordering::AcqRel);
            self.0.lock().unwrap().extend_from_slice(&bytes[..n]);
            Some(n)
        }
    }

    type Outbox = Arc<std::sync::Mutex<(Vec<u8>, usize)>>;
    type Inbox = Arc<std::sync::Mutex<Vec<u8>>>;

    /// As [`wait_for`], and what `dump` tells is in the message when it stalls.
    fn wait_for_dump(what: &str, got: &Inbox, want: &[u8], secs: u64, dump: impl Fn() -> String) {
        let start = std::time::Instant::now();
        while got.lock().unwrap().len() < want.len() {
            assert!(start.elapsed() < Duration::from_secs(secs), "{what}: {} of {}\n{}", got.lock().unwrap().len(), want.len(), dump());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(*got.lock().unwrap() == want, "{what}: other bytes");
    }

    fn wait_for(what: &str, got: &Inbox, want: &[u8], secs: u64) {
        let start = std::time::Instant::now();
        while got.lock().unwrap().len() < want.len() {
            assert!(start.elapsed() < Duration::from_secs(secs), "{what}: {} of {}", got.lock().unwrap().len(), want.len());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(*got.lock().unwrap() == want, "{what}: other bytes");
    }

    /// A client and a server on one machine, the session identified on both sides: what each sends
    /// arrives whole and in order, both ways - with every `drop`-th datagram of data lost on the
    /// way (0 - none): the journal sends it again. `pattern`: the Noise both protocol layers declare,
    /// each with its own secret and the other's public key, as the generated code does.
    fn round_trip(drop: usize, pattern: Option<i32>) {
        let engine = Engine::new("t", 2).unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let s2 = seen.clone();
        let hook: WireHook = Arc::new(move |dir, pkt| {
            if drop == 0 || dir != WireDir::Tx || pkt[0] & LONG != 0 || pkt[0] & DATA == 0 {
                return WireAction::Pass;
            }
            if s2.fetch_add(1, Ordering::Relaxed) % drop == drop - 1 {
                WireAction::Drop
            } else {
                WireAction::Pass
            }
        });
        let server_got: Inbox = Default::default();
        let server_out: Outbox = Default::default();
        let server_state: Arc<std::sync::Mutex<Option<Arc<ExternalConnection>>>> = Default::default();
        let (sg, so, ss) = (server_got.clone(), server_out.clone(), server_state.clone());
        let made = Arc::new(AtomicUsize::new(0));
        let m2 = made.clone();
        let (secret, public) = crate::lib::x25519::keypair();
        let (client_secret, client_public) = crate::lib::x25519::keypair();
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .wire_hook(hook.clone())
            .on_connect(Arc::new(move |state| {
                m2.fetch_add(1, Ordering::Relaxed);
                if let Some(p) = pattern {
                    state.secure(Some(Chain::default().noise_keys(p, &secret, &client_public)));
                }
                state.identify(7); // before it opens: the open asks the peer to go on from here
                *ss.lock().unwrap() = Some(state.clone());
                (Box::new(Counting::new(sg.clone())) as Box<dyn BytesDst>, Box::new(Journaled::new(so.clone())) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client").engine(engine.clone()).wire_hook(hook).build();
        let client_got: Inbox = Default::default();
        let client_out: Outbox = Default::default();
        let (cg, co) = (client_got.clone(), client_out.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        client.connect(
            addr,
            Duration::from_secs(3),
            move |st| {
                if let Some(p) = pattern {
                    st.secure(Some(Chain::default().noise_keys(p, &client_secret, &public)));
                }
                (Box::new(Counting::new(cg)) as Box<dyn BytesDst>, Box::new(Journaled::new(co)) as Box<dyn BytesSrc>)
            },
            move |r| {
                let _ = tx.send(r);
            },
        );
        let state = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        state.identify(7); // open already: the peer is asked to go on from here now
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
        client_out.lock().unwrap().0.extend_from_slice(&payload);
        state.wake_transmit();
        {
            let ss = server_state.clone();
            let co = client_out.clone();
            let dump = || {
                let (len, at) = {
                    let o = co.lock().unwrap();
                    (o.0.len(), o.1)
                };
                format!(
                    "client source gave {at} of {len}; live: client {} server {}; server sinks made {}\nclient: {}\nserver: {}",
                    client.connections_live(),
                    server.connections_live(),
                    made.load(Ordering::Relaxed),
                    client.dump(&state),
                    ss.lock().unwrap().as_ref().map_or("none".into(), |b| server.dump(b))
                )
            };
            wait_for_dump(&format!("drop {drop}: the server"), &server_got, &payload, 30, dump);
        }
        let back = server_state.lock().unwrap().clone().unwrap();
        server_out.lock().unwrap().0.extend_from_slice(&payload);
        back.wake_transmit();
        wait_for_dump(&format!("drop {drop}: the client"), &client_got, &payload, 30, || {
            format!("client: {}\nserver: {}", client.dump(&state), server.dump(&back))
        });
        if 0 < drop {
            assert!(0 < client.stats(&state).unwrap().lost_datagrams + server.stats(&back).unwrap().lost_datagrams);
        }
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    #[test]
    fn bytes_go_both_ways_whole() {
        round_trip(0, None);
    }

    #[test]
    fn bytes_go_both_ways_whole_through_losses() {
        round_trip(9, None);
    }

    /// A datagram lost before the peer asked for the stream is not lost for the session: once named
    /// here, what goes is journalled and held until the peer names its position. The server says
    /// nothing for a while, so its request comes late; the client loses a datagram of its first
    /// series meanwhile - everything arrives, whole and in order.
    #[test]
    fn a_loss_before_the_peer_asks_loses_nothing() {
        let engine = Engine::new("t", 2).unwrap();
        let quiet_from: Arc<std::sync::Mutex<Option<std::time::Instant>>> = Default::default();
        let q = quiet_from.clone();
        let server_hook: WireHook = Arc::new(move |dir, pkt| {
            if dir != WireDir::Tx || pkt[0] & LONG != 0 {
                return WireAction::Pass;
            }
            let at = *q.lock().unwrap().get_or_insert_with(std::time::Instant::now);
            if at.elapsed() < Duration::from_millis(200) {
                WireAction::Drop
            } else {
                WireAction::Pass
            } // the request waits
        });
        let d = Arc::new(AtomicUsize::new(0));
        let client_hook: WireHook = Arc::new(move |dir, pkt| {
            if dir != WireDir::Tx || pkt[0] & LONG != 0 || pkt[0] & DATA == 0 || pkt.len() < HEADER {
                return WireAction::Pass;
            }
            let word = u32::from_le_bytes([pkt[5], pkt[6], pkt[7], pkt[8]]);
            if word & 0xFF == 0 && word >> 8 == 2 && d.fetch_add(1, Ordering::Relaxed) == 0 {
                WireAction::Drop
            } else {
                WireAction::Pass
            }
        });
        let server_got: Inbox = Default::default();
        let sg = server_got.clone();
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .wire_hook(server_hook)
            .on_connect(Arc::new(move |state| {
                state.identify(7);
                (
                    Box::new(Counting::new(sg.clone())) as Box<dyn BytesDst>,
                    Box::new(Journaled::new(Default::default())) as Box<dyn BytesSrc>,
                )
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client").engine(engine.clone()).wire_hook(client_hook).build();
        let out: Outbox = Default::default();
        let o = out.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        client.connect(
            addr,
            Duration::from_secs(3),
            move |_| (Box::new(Counting::new(Default::default())) as Box<dyn BytesDst>, Box::new(Journaled::new(o)) as Box<dyn BytesSrc>),
            move |r| {
                let _ = tx.send(r);
            },
        );
        let state = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        state.identify(7);
        let payload: Vec<u8> = (0..30_000u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
        out.lock().unwrap().0.extend_from_slice(&payload);
        state.wake_transmit();
        wait_for("a loss before the peer asked", &server_got, &payload, 10);
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    /// After a reclaim the server's stream goes on too: what the source of the moved session has
    /// goes to the client.
    #[test]
    fn a_reclaimed_session_sends_on_the_new_connection() {
        let engine = Engine::new("t", 1).unwrap();
        let dark = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = dark.clone();
        let hook: WireHook = Arc::new(move |_, _| if d.load(Ordering::Relaxed) { WireAction::Drop } else { WireAction::Pass });
        let outs: Arc<std::sync::Mutex<Vec<Outbox>>> = Default::default();
        let seated: Arc<std::sync::Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let (os, s) = (outs.clone(), seated.clone());
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .wire_hook(hook.clone())
            .idle_timeout(Duration::from_millis(300))
            .keepalive(Duration::from_millis(100))
            .on_connect(Arc::new(move |state| {
                state.set_resumable_seconds_hook(Arc::new(|_| 60));
                let mut seated = s.lock().unwrap();
                if seated.is_empty() {
                    state.identify(7);
                }
                seated.push(state.clone());
                let out: Outbox = Default::default();
                os.lock().unwrap().push(out.clone());
                (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(out)) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client")
            .engine(engine.clone())
            .wire_hook(hook)
            .idle_timeout(Duration::from_millis(300))
            .keepalive(Duration::from_millis(100))
            .build();
        let got: Inbox = Default::default();
        let g = got.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        client.connect(
            addr,
            Duration::from_secs(3),
            move |_| (Box::new(Collect(g)) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>),
            move |r| {
                let _ = tx.send(r);
            },
        );
        let cs = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        cs.set_resumable_seconds_hook(Arc::new(|_| 60));
        cs.identify(7);
        let a: Vec<u8> = (0..50_000u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
        let b: Vec<u8> = (0..50_000u32).map(|i| (i.wrapping_mul(40503) >> 7) as u8).collect();
        let lost = seated.lock().unwrap()[0].clone();
        let first_out = outs.lock().unwrap()[0].clone();
        // the packs go once the server heard the position of the client: what this source, of no
        // journal, sent before would not be read, and could not be sent again
        let p = lost.udp.get().unwrap().0 as usize;
        let until = std::time::Instant::now() + Duration::from_secs(10);
        // SAFETY: a slot lives as long as its loop
        while unsafe { (*(p as *mut Peer)).lp }.call(move |_| unsafe { (*(p as *mut Peer)).tx_series }) == 0 {
            assert!(std::time::Instant::now() < until, "the server did not hear the position of the client");
            std::thread::sleep(Duration::from_millis(5));
        }
        first_out.lock().unwrap().0.extend_from_slice(&a);
        lost.wake_transmit();
        wait_for("before the loss", &got, &a, 10);
        let token = lost.session();
        dark.store(true, Ordering::Relaxed);
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while client.stats(&cs).unwrap().state != PARKED || server.stats(&lost).unwrap().state != PARKED {
            assert!(std::time::Instant::now() < until, "not parked");
            std::thread::sleep(Duration::from_millis(10));
        }
        dark.store(false, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        client.reconnect(&cs, Duration::from_secs(3), None, move |r| {
            let _ = tx.send(r.map(|_| ()));
        });
        rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while seated.lock().unwrap().len() < 2 {
            assert!(std::time::Instant::now() < until, "no new connection on the server");
            std::thread::sleep(Duration::from_millis(5));
        }
        let taker = seated.lock().unwrap()[1].clone();
        use crate::lib::adhoc::connection::external::External;
        let foreign = (token as u32).wrapping_add(1); // a token someone guessed: the login of this connection proved another identity
        External::reclaim_id(&*taker, token, foreign);
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while taker.session() as u32 != foreign {
            assert!(std::time::Instant::now() < until, "the proved identity was not applied");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.stats(&lost).unwrap().state, PARKED, "a token of another identity took the session");
        taker.identify(token as u32); // the login identified this connection before the token came: the move below applies it again
        External::reclaim_id(&*taker, token, token as u32);
        std::thread::sleep(Duration::from_millis(50));
        first_out.lock().unwrap().0.extend_from_slice(&b);
        taker.wake_transmit();
        let mut whole = a.clone();
        whole.extend_from_slice(&b);
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while got.lock().unwrap().len() < whole.len() {
            assert!(
                std::time::Instant::now() < until,
                "after the reclaim: {} of {}; server new {:?}; client {:?}",
                got.lock().unwrap().len(),
                whole.len(),
                server.stats(&taker),
                client.stats(&cs)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(*got.lock().unwrap() == whole, "after the reclaim: other bytes");
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    /// A reconnect that fails after the wait of its session ran out ends the session: its close is
    /// heard and the slot is free - not left connecting for good.
    #[test]
    fn a_reconnect_failing_past_the_wait_ends_the_session() {
        let engine = Engine::new("t", 1).unwrap();
        let dark = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = dark.clone();
        let hook: WireHook = Arc::new(move |_, _| if d.load(Ordering::Relaxed) { WireAction::Drop } else { WireAction::Pass });
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .wire_hook(hook.clone())
            .idle_timeout(Duration::from_millis(300))
            .keepalive(Duration::from_millis(100))
            .on_connect(Arc::new(|_| {
                (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client")
            .engine(engine.clone())
            .wire_hook(hook)
            .idle_timeout(Duration::from_millis(300))
            .keepalive(Duration::from_millis(100))
            .build();
        let (tx, rx) = std::sync::mpsc::channel();
        client.connect(
            addr,
            Duration::from_secs(3),
            |_| (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>),
            move |r| {
                let _ = tx.send(r);
            },
        );
        let cs = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        cs.set_resumable_seconds_hook(Arc::new(|_| 3)); // wide enough to be seen parked on a busy machine
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = closes.clone();
        cs.set_event_sink(Arc::new(move |_, e| {
            if e.is_close() {
                c.fetch_add(1, Ordering::Relaxed);
            }
        }));
        cs.identify(7);
        dark.store(true, Ordering::Relaxed); // the server is gone for good
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while client.stats(&cs).unwrap().state != PARKED {
            assert!(std::time::Instant::now() < until, "not parked");
            std::thread::sleep(Duration::from_millis(10));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        client.reconnect(&cs, Duration::from_secs(4), None, move |r| {
            // past the wait of the session
            let _ = tx.send(r.map(|_| ()));
        });
        assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap().is_err(), "nobody answers");
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while client.stats(&cs).unwrap().state != FREE || closes.load(Ordering::Relaxed) == 0 {
            assert!(
                std::time::Instant::now() < until,
                "left {:?}, closes {}",
                client.stats(&cs).map(|s| s.state),
                closes.load(Ordering::Relaxed)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(closes.load(Ordering::Relaxed), 1, "its close is heard once");
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    /// Collects what it gets - and panics on a byte 0xFF: code of the application with a bug in it.
    struct Touchy(Inbox);
    impl BytesDst for Touchy {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            assert!(!bytes.contains(&0xFF), "a byte the sink cannot take");
            self.0.lock().unwrap().extend_from_slice(bytes);
            true
        }
    }

    /// The code of one connection panics: its failure goes to the handler of the host, and the
    /// connection lives on - what to do of it is the application's -, and so does the loop with the
    /// other connection.
    #[test]
    fn a_panic_in_a_connection_is_told_and_the_connection_lives_on() {
        let engine = Engine::new("t", 1).unwrap();
        let got: Inbox = Default::default();
        let failures = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (g, fl) = (got.clone(), failures.clone());
        let seated: Arc<std::sync::Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let sd = seated.clone();
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .on_failure(Arc::new(move |_, e| fl.lock().unwrap().push(e.to_string())))
            .on_connect(Arc::new(move |st| {
                sd.lock().unwrap().push(st.clone());
                (Box::new(Touchy(g.clone())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client").engine(engine.clone()).build();
        let connect = |out: Outbox| {
            let (tx, rx) = std::sync::mpsc::channel();
            client.connect(
                addr,
                Duration::from_secs(3),
                move |_| (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(out)) as Box<dyn BytesSrc>),
                move |r| {
                    let _ = tx.send(r);
                },
            );
            rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap()
        };
        let (oa, ob): (Outbox, Outbox) = (Default::default(), Default::default());
        let a = connect(oa.clone());
        let b = connect(ob.clone());
        oa.lock().unwrap().0.extend_from_slice(&[1, 0xFF]);
        a.wake_transmit();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while failures.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < until, "the panic is not told");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(client.stats(&a).is_some_and(|s| s.state == ACTIVE), "the connection whose code panicked lives on");
        // the sink is not in a call any more: a reclaim installs the session at once, not at the next call of the sink
        let p = seated.lock().unwrap()[0].udp.get().unwrap().0 as usize;
        // SAFETY: a slot lives as long as its loop
        let in_sink = unsafe { (*(p as *mut Peer)).lp }.call(move |_| unsafe { (*(p as *mut Peer)).in_sink });
        assert!(!in_sink, "the flag of the sink is down after its panic");
        oa.lock().unwrap().0.extend_from_slice(b"alive");
        a.wake_transmit();
        wait_for("the connection whose code panicked goes on", &got, b"alive", 10);
        ob.lock().unwrap().0.extend_from_slice(b"after");
        b.wake_transmit();
        wait_for("the other connection goes on", &got, b"aliveafter", 10);
        assert_eq!(*failures.lock().unwrap(), ["panicked: a byte the sink cannot take"], "one failure, to the handler of the host");
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    /// A handler of the failures of the engine that panics does not take the loop with it, as on TCP: its own failure is
    /// printed, the loop serves the next task.
    #[test]
    fn a_panicking_failure_handler_does_not_take_the_loop_with_it() {
        let told = Arc::new(AtomicUsize::new(0));
        let t = told.clone();
        let engine = Engine::with_failure(
            "t",
            1,
            Arc::new(move |_, _| {
                t.fetch_add(1, Ordering::Relaxed);
                panic!("the handler of the engine");
            }),
        )
        .unwrap();
        let lp = engine.loops[0];
        lp.submit(Box::new(|_| panic!("a task that panics")));
        assert_eq!(lp.call(|_| 7), 7, "the loop goes on");
        assert_eq!(told.load(Ordering::Relaxed), 1, "the failure went to the handler");
        engine.close();
    }

    /// Collects what it gets until a byte 0xFE, then refuses everything: a stream it could not
    /// parse (`Receiver` keeps its error).
    struct Sticky(Inbox, Arc<AtomicUsize>);
    impl BytesDst for Sticky {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.1.fetch_add(1, Ordering::Relaxed);
            if bytes.contains(&0xFE) || self.0.lock().unwrap().contains(&0xFE) {
                self.0.lock().unwrap().push(0xFE);
                return false;
            }
            self.0.lock().unwrap().extend_from_slice(bytes);
            true
        }
    }

    /// A permanently invalid stream must close instead of requesting identical bytes again. The
    /// peer must receive the close notification.
    #[test]
    fn a_stream_the_sink_broke_ends_the_connection() {
        let engine = Engine::new("t", 1).unwrap();
        let got: Inbox = Default::default();
        let offers = Arc::new(AtomicUsize::new(0));
        let (g, o) = (got.clone(), offers.clone());
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .on_connect(Arc::new(move |_| {
                (Box::new(Sticky(g.clone(), o.clone())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client").engine(engine.clone()).build();
        let (tx, rx) = std::sync::mpsc::channel();
        let out: Outbox = Default::default();
        let o2 = out.clone();
        client.connect(
            addr,
            Duration::from_secs(3),
            move |_| (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(o2)) as Box<dyn BytesSrc>),
            move |r| {
                let _ = tx.send(r);
            },
        );
        let a = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        out.lock().unwrap().0.extend_from_slice(&[1, 2, 0xFE, 3]);
        a.wake_transmit();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while client.stats(&a).map_or(false, |s| s.state == ACTIVE) {
            assert!(
                std::time::Instant::now() < until,
                "the connection whose stream broke goes on: the sink was offered it {} times",
                offers.load(Ordering::Relaxed)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(offers.load(Ordering::Relaxed) <= 3, "offered {} times", offers.load(Ordering::Relaxed));
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    #[test]
    fn bytes_go_both_ways_whole_encrypted_through_losses() {
        round_trip(7, Some(crate::lib::noise::PATTERN_NK));
    }

    /// IK: the client proves its key in the INITIAL - its first message is the longest of all.
    #[test]
    fn bytes_go_both_ways_whole_encrypted_ik() {
        round_trip(0, Some(crate::lib::noise::PATTERN_IK));
    }

    /// KK: both sides know each other beforehand.
    #[test]
    fn bytes_go_both_ways_whole_encrypted_kk() {
        round_trip(0, Some(crate::lib::noise::PATTERN_KK));
    }

    /// How many datagrams of the connection, both ways, show the marker the bytes sent are full of -
    /// the protocol layers declaring `pattern` of Noise, or none. The layers see where the connection
    /// is as they are seated: both addresses on the server, at least the remote one on the client.
    fn marker_on_the_wire(pattern: Option<i32>) -> usize {
        const MARKER: &[u8] = b"marker of the pack";
        let engine = Engine::new("t", 1).unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let s2 = seen.clone();
        let hook: WireHook = Arc::new(move |_, pkt| {
            if pkt[0] & LONG == 0 && pkt.windows(MARKER.len()).any(|w| w == MARKER) {
                s2.fetch_add(1, Ordering::Relaxed);
            }
            WireAction::Pass
        });
        let (secret, public) = crate::lib::x25519::keypair();
        let (client_secret, client_public) = crate::lib::x25519::keypair();
        let server_got: Inbox = Default::default();
        let sg = server_got.clone();
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .wire_hook(hook.clone())
            .on_connect(Arc::new(move |state| {
                assert!(state.local().is_some() && state.remote().is_some(), "the server's layer sees both addresses");
                if let Some(p) = pattern {
                    state.secure(Some(Chain::default().noise_keys(p, &secret, &client_public)));
                }
                (Box::new(Collect(sg.clone())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client").engine(engine.clone()).wire_hook(hook).build();
        let out: Outbox = Default::default();
        let payload: Vec<u8> = MARKER.repeat(2000);
        out.lock().unwrap().0.extend_from_slice(&payload);
        let o2 = out.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        client.connect(
            addr,
            Duration::from_secs(3),
            move |st| {
                assert_eq!(st.remote(), Some(canonical(addr)), "the client's layer sees where it calls");
                if let Some(p) = pattern {
                    st.secure(Some(Chain::default().noise_keys(p, &client_secret, &public)));
                }
                (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(o2)) as Box<dyn BytesSrc>)
            },
            move |r| {
                let _ = tx.send(r);
            },
        );
        let state = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        state.wake_transmit();
        wait_for("the server", &server_got, &payload, 20);
        client.shutdown();
        server.shutdown();
        engine.close();
        seen.load(Ordering::Relaxed)
    }

    /// With Noise every datagram is encrypted: the bytes it carries are not on the wire; without,
    /// they are.
    #[test]
    fn noise_hides_the_bytes_on_the_wire() {
        for p in [crate::lib::noise::PATTERN_NK, crate::lib::noise::PATTERN_IK, crate::lib::noise::PATTERN_KK] {
            assert_eq!(marker_on_the_wire(Some(p)), 0, "pattern {p}: the marker went in the clear");
        }
        assert!(0 < marker_on_the_wire(None), "no Noise: the bytes go as they are");
    }

    /// The Noise of the layers on UDP: a server whose layer refuses the key the client proved (IK)
    /// does not accept the connection, nor does one whose layer has no keys for it - its failure
    /// goes to the host; a client whose layer has none does not dial.
    #[test]
    fn a_refused_noise_opens_no_connection() {
        use crate::lib::noise::PATTERN_IK;
        let engine = Engine::new("t", 1).unwrap();
        let (secret, public) = crate::lib::x25519::keypair();
        let (client_secret, client_public) = crate::lib::x25519::keypair();
        let failures = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let refuse_keys = Arc::new(AtomicBool::new(false));
        let (f, rk) = (failures.clone(), refuse_keys.clone());
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .on_failure(Arc::new(move |_, e| f.lock().unwrap().push(e.to_string())))
            .on_connect(Arc::new(move |state| {
                state.secure(if rk.load(Ordering::Relaxed) {
                    None
                } else {
                    Some(Chain::default().noise_keys(PATTERN_IK, &secret, &client_public).on_peer(|_| false))
                });
                (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client").engine(engine.clone()).build();
        let dial = |declare: Option<Chain>| {
            let (tx, rx) = std::sync::mpsc::channel();
            client.connect(
                addr,
                Duration::from_millis(600),
                move |st| {
                    st.secure(declare);
                    (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
                },
                move |r| {
                    let _ = tx.send(r.map(|_| ()));
                },
            );
            rx.recv_timeout(Duration::from_secs(10)).unwrap()
        };
        let keys = || Some(Chain::default().noise_keys(PATTERN_IK, &client_secret, &public));
        assert!(dial(keys()).is_err(), "the server's layer refuses the key of the client");
        assert_eq!(server.connections_live(), 0);
        refuse_keys.store(true, Ordering::Relaxed);
        assert!(dial(keys()).is_err(), "the server's layer has no keys");
        assert!(
            failures.lock().unwrap().iter().any(|e| e.contains("[Noise]")),
            "the refusal goes to the host: {:?}",
            failures.lock().unwrap()
        );
        assert_eq!(dial(None).unwrap_err().kind(), io::ErrorKind::PermissionDenied, "the client's layer has no keys: no INITIAL");
        assert_eq!(server.connections_live(), 0);
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    /// A sink with no room at first, then a little at a time, each time with `wake_receive`: what
    /// it did not take is kept by the connection and offered again - everything arrives whole and
    /// in order, and the stream is never broken for it.
    #[test]
    fn a_sink_without_room_gets_everything_later() {
        let engine = Engine::new("t", 1).unwrap();
        let got: Inbox = Default::default();
        let room = Arc::new(AtomicUsize::new(0));
        let server_state: Arc<std::sync::Mutex<Option<Arc<ExternalConnection>>>> = Default::default();
        let (g, r, ss) = (got.clone(), room.clone(), server_state.clone());
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .on_connect(Arc::new(move |state| {
                state.identify(7);
                *ss.lock().unwrap() = Some(state.clone());
                (Box::new(Slow(g.clone(), r.clone())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client").engine(engine.clone()).build();
        let out: Outbox = Default::default();
        let o = out.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        client.connect(
            addr,
            Duration::from_secs(3),
            move |_| (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(o)) as Box<dyn BytesSrc>),
            move |r| {
                let _ = tx.send(r);
            },
        );
        let state = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        state.identify(7);
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
        // the packs go once the client heard the position of the server: what this source, of no journal, sent before
        // would not be read, and could not be sent again
        let p = state.udp.get().unwrap().0 as usize;
        let until = std::time::Instant::now() + Duration::from_secs(10);
        // SAFETY: a slot lives as long as its loop
        while unsafe { (*(p as *mut Peer)).lp }.call(move |_| unsafe { (*(p as *mut Peer)).tx_series }) == 0 {
            assert!(std::time::Instant::now() < until, "the client did not hear the position of the server");
            std::thread::sleep(Duration::from_millis(5));
        }
        out.lock().unwrap().0.extend_from_slice(&payload);
        state.wake_transmit();
        std::thread::sleep(Duration::from_millis(100)); // the sink takes nothing meanwhile: the window is out, kept
        let back = server_state.lock().unwrap().clone().unwrap();
        let start = std::time::Instant::now();
        while got.lock().unwrap().len() < payload.len() {
            assert!(start.elapsed() < Duration::from_secs(30), "{} of {}", got.lock().unwrap().len(), payload.len());
            room.fetch_add(20_000, Ordering::AcqRel);
            back.wake_receive();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(*got.lock().unwrap() == payload, "other bytes");
        let s = server.stats(&back).unwrap();
        assert_eq!(s.gaps, 0, "the stream was broken: {s:?}");
        assert_eq!(s.refused, 0, "{s:?}");
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    /// Both sides lose each other and park the session; the client comes back over a new handshake,
    /// and the new connection on the server reclaims the parked session by its token: what the
    /// client sends from then on goes to the sink of the session, in order after what it had - the
    /// sink of the new connection gets nothing.
    #[test]
    fn a_reclaimed_session_goes_on_on_the_new_connection() {
        let engine = Engine::new("t", 1).unwrap();
        let dark = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = dark.clone();
        let hook: WireHook = Arc::new(move |_, _| if d.load(Ordering::Relaxed) { WireAction::Drop } else { WireAction::Pass });
        let first: Inbox = Default::default();
        let later: Inbox = Default::default();
        let seated: Arc<std::sync::Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let (f, l, s) = (first.clone(), later.clone(), seated.clone());
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .wire_hook(hook.clone())
            .idle_timeout(Duration::from_millis(300))
            .keepalive(Duration::from_millis(100))
            .on_connect(Arc::new(move |state| {
                state.set_resumable_seconds_hook(Arc::new(|_| 60));
                let mut seated = s.lock().unwrap();
                let inbox = if seated.is_empty() { f.clone() } else { l.clone() };
                if seated.is_empty() {
                    state.identify(7);
                }
                seated.push(state.clone());
                (Box::new(Collect(inbox)) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
            }))
            .build();
        let addr = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
        let client = host::Builder::new("client")
            .engine(engine.clone())
            .wire_hook(hook)
            .idle_timeout(Duration::from_millis(300))
            .keepalive(Duration::from_millis(100))
            .build();
        let out: Outbox = Default::default();
        let o = out.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        client.connect(
            addr,
            Duration::from_secs(3),
            move |_| (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(o)) as Box<dyn BytesSrc>),
            move |r| {
                let _ = tx.send(r);
            },
        );
        let cs = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        cs.set_resumable_seconds_hook(Arc::new(|_| 60));
        cs.identify(7);
        let a: Vec<u8> = (0..50_000u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
        let b: Vec<u8> = (0..50_000u32).map(|i| (i.wrapping_mul(40503) >> 7) as u8).collect();
        // as after the reclaim below: the packs go once the client heard the position of the server -
        // what this source, of no journal, sent before would not be read, and could not be sent again
        let p = cs.udp.get().unwrap().0 as usize;
        let until = std::time::Instant::now() + Duration::from_secs(10);
        // SAFETY: a slot lives as long as its loop
        while unsafe { (*(p as *mut Peer)).lp }.call(move |_| unsafe { (*(p as *mut Peer)).tx_series }) == 0 {
            assert!(std::time::Instant::now() < until, "the client did not hear the position of the server");
            std::thread::sleep(Duration::from_millis(5));
        }
        out.lock().unwrap().0.extend_from_slice(&a);
        cs.wake_transmit();
        wait_for("before the loss", &first, &a, 10);
        let lost = seated.lock().unwrap()[0].clone();
        let token = lost.session();
        dark.store(true, Ordering::Relaxed); // nothing goes through: both sides time out and park
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while client.stats(&cs).unwrap().state != PARKED || server.stats(&lost).unwrap().state != PARKED {
            assert!(
                std::time::Instant::now() < until,
                "not parked: client {}, server {}",
                client.stats(&cs).unwrap().state,
                server.stats(&lost).unwrap().state
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // what the application reads of the connection says so too
        assert!(cs.is_parked() && lost.is_parked() && !cs.is_active() && !lost.is_cleaning());
        dark.store(false, Ordering::Relaxed);
        assert!(server.describe(&lost).unwrap().ends_with("parked"), "{:?}", server.describe(&lost));
        assert!(server.peers().iter().any(|c| Arc::ptr_eq(c, &lost)), "the slots of the host");
        let (tx, rx) = std::sync::mpsc::channel();
        client.reconnect(&cs, Duration::from_secs(3), None, move |r| {
            let _ = tx.send(r.map(|_| ()));
        });
        rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while seated.lock().unwrap().len() < 2 {
            assert!(std::time::Instant::now() < until, "no new connection on the server");
            std::thread::sleep(Duration::from_millis(5));
        }
        let taker = seated.lock().unwrap()[1].clone();
        assert!(taker.reclaim_session(token).is_none());
        assert_eq!(taker.session() as u32, 7, "the new connection takes the identity of the session");
        // the packs go after the agreement ( the order in the description of the module resumable ): the client goes on
        // once it heard the position of the server - its series turned. What this source, of no journal, sent before
        // would not be read, and could not be sent again
        let p = cs.udp.get().unwrap().0 as usize;
        let until = std::time::Instant::now() + Duration::from_secs(10);
        // SAFETY: a slot lives as long as its loop
        while unsafe { (*(p as *mut Peer)).lp }.call(move |_| unsafe { (*(p as *mut Peer)).tx_series }) == 0 {
            assert!(std::time::Instant::now() < until, "the client did not hear the position of the server");
            std::thread::sleep(Duration::from_millis(5));
        }
        out.lock().unwrap().0.extend_from_slice(&b);
        cs.wake_transmit();
        let mut whole = a.clone();
        whole.extend_from_slice(&b);
        let until = std::time::Instant::now() + Duration::from_secs(20);
        while first.lock().unwrap().len() < whole.len() {
            assert!(
                std::time::Instant::now() < until,
                "after the reclaim: {} of {}; the new sink got {}; server new {:?}; server old {:?}; client {:?}",
                first.lock().unwrap().len(),
                whole.len(),
                later.lock().unwrap().len(),
                server.stats(&taker),
                server.stats(&lost),
                client.stats(&cs)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(*first.lock().unwrap() == whole, "after the reclaim: other bytes");
        assert!(later.lock().unwrap().is_empty(), "the sink of the new connection got {} bytes", later.lock().unwrap().len());
        assert_eq!(server.stats(&lost).unwrap().state, FREE, "the slot of the parked session is freed");
        assert!(
            lost.session() as u32 == 0 && !lost.is_open(),
            "the slot stays itself, cleared for its next session ( Java: a Peer for life )"
        );
        assert!(!Arc::ptr_eq(&lost.keys(), &taker.keys()), "the keys went with the session, the slot has its own");
        client.shutdown();
        server.shutdown();
        engine.close();
    }

    /// One socket for both families: a server bound on `[::]` takes IPv4 and IPv6 peers, a client
    /// loop connects to either through its one socket.
    #[test]
    fn one_socket_serves_both_families() {
        let engine = Engine::new("t", 1).unwrap();
        let server = host::Builder::new("server")
            .engine(engine.clone())
            .on_connect(Arc::new(move |_| {
                (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>)
            }))
            .build();
        let port = match server.bind(SocketAddr::new(std::net::Ipv6Addr::UNSPECIFIED.into(), 0)) {
            Ok(a) => a.port(),
            Err(_) => return, // no IPv6 on this machine
        };
        let client = host::Builder::new("client").engine(engine.clone()).build();
        // an IPv4 peer named as IPv6-mapped (::ffff:127.0.0.1) too: it is the IPv4 one
        for to in [
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port),
            SocketAddr::new(Ipv4Addr::LOCALHOST.to_ipv6_mapped().into(), port),
        ] {
            let (tx, rx) = std::sync::mpsc::channel();
            client.connect(
                to,
                Duration::from_secs(3),
                |_| (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>),
                move |r| {
                    let _ = tx.send(r.map(|_| ()));
                },
            );
            assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap().is_ok(), "connected to {to}");
        }
        engine.close();
    }

    #[test]
    fn a_connect_nobody_answers_times_out() {
        let engine = Engine::new("t", 1).unwrap();
        let client = host::Builder::new("client").engine(engine.clone()).build();
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let start = std::time::Instant::now();
        client.connect(
            silent.local_addr().unwrap(),
            Duration::from_millis(400),
            |_| (Box::new(Collect(Default::default())) as Box<dyn BytesDst>, Box::new(Out(Default::default())) as Box<dyn BytesSrc>),
            move |r| {
                let _ = tx.send(r.map(|_| ()));
            },
        );
        let r = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2), "the loop times the connect: {:?}", start.elapsed());
        engine.close();
    }
}
