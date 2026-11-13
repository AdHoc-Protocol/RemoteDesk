// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! TCP clients and servers driven by [`Engine`] I/O loops.
//!
//! A server accepts on one loop and distributes sockets among its engine's loops. Each connection
//! retains its assigned loop. The connection factory attaches a [`BytesDst`] / [`BytesSrc`] pair to
//! an [`ExternalConnection`] before I/O starts.
//!
//! Construction starts accepting connections. Client connection results are delivered on the
//! assigned loop thread; no separate async runtime is required.

pub mod client;
pub mod external_connection;
pub mod pool;
pub mod server;
pub mod web_socket;

pub use client::{Client, Redial};
pub use external_connection::ExternalConnection;
pub use pool::Pool;
pub use server::Server;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use self::external_connection::ConnId;
use self::external_connection::Event;
use self::web_socket::Codec;
use super::admission::{Admission, Admit, Census, SocketOptions};
use super::engine::{self, Accept, Engine, Listening, Loop};
use super::noise_adapter;
use crate::lib::adhoc::{BytesDst, BytesSrc};
use crate::lib::chain::Chain;

/// Seats the protocol layer on a new connection: its sink, its source. The connection wakes its
/// transmit through [`ExternalConnection::wake_transmit`].
pub type ConnectionFactory = Arc<dyn Fn(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + Sync>;

/// Chooses the chain of an incoming connection by where it comes from: TLS is decided on every
/// connection.
pub type ChooseChain = Arc<dyn Fn(SocketAddr) -> Option<Arc<Chain>> + Send + Sync>;

/// What a client hears of its connect, on the thread of the connection's loop: the state once the
/// session started, or what went wrong.
pub type OnConnected = Box<dyn FnOnce(std::io::Result<Arc<ExternalConnection>>) + Send>;

/// The lifecycle events of the connections of a host.
pub type EventCallback = Arc<dyn Fn(ConnId, Event) + Send + Sync>;

// The listening host, shared by the TCP and the WebSocket server

/// What a listening server gives every socket accepted for it.
pub(crate) struct Host {
    pub name: String,
    pub ws: bool,
    /// Where the failures of the host and of its connections go.
    pub on_failure: crate::lib::network::failure::OnFailure,
    pub on_connect: ConnectionFactory,
    pub on_event: Option<EventCallback>,
    /// What its connections go through: TLS ([`Chain`]); the Noise inside it is of the protocol layer seated.
    pub chain: Option<Arc<Chain>>,
    /// Chooses the chain of every incoming connection by where it comes from, in place of `chain`.
    /// `None` it gives: the socket is used as it is.
    pub choose_chain: Option<ChooseChain>,
    /// The slots of its connections: the free ones, the live ones, the parked sessions.
    pub pool: Arc<Pool>,
    pub admission: Arc<Admission>,
    pub socket_opts: SocketOptions,
    /// How long the listener stays deaf after accept failed.
    pub accept_retry: Duration,
    /// How long an accepted connection may take over its handshakes, in milliseconds; non-positive:
    /// no limit.
    pub handshake_timeout_ms: i64,
}

impl Accept for Host {
    fn admit(&self, peer: SocketAddr) -> Option<bool> {
        let (a, counted) = self.admission.admit_counted(peer, self.pool.connections_live());
        (a == Admit::Allowed).then_some(counted)
    }

    /// On the loop the connection will live on: a state, the protocol layer seated on it, and both
    /// directions start. A WebSocket's session starts with the upgrade, not with the socket: its
    /// connect event comes then.
    fn failure(&self, e: &(dyn std::error::Error + 'static)) {
        (self.on_failure)(&self.name, e);
    }

    fn accept_retry(&self) -> Duration {
        self.accept_retry
    }

    fn accepted(self: Arc<Self>, lp: &Loop, stream: mio::net::TcpStream, peer: SocketAddr, counted: bool) {
        let state = self.pool.allocate(lp.number); // the slot lives on the loop the socket was handed to
        self.socket_opts.apply_mio(&stream, |e| state.failure(e));
        if let Some(ev) = &self.on_event {
            state.set_event_sink(ev.clone());
        }
        state.set_addresses(stream.local_addr().ok(), Some(peer)); // the region of the keys sees where the connection is
        let seated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (sink, source) = (self.on_connect)(&state);
            noised(&state, false, sink, source) // the Noise the protocol layer declares: this end answers
        }));
        let (sink, source) = match seated {
            Ok(Some(s)) => s,
            Ok(None) => {
                state.failure(&refused()); // the protocol layer has no keys for it
                if counted {
                    self.admission.connection_closed(peer);
                }
                state.abandon();
                return;
            }
            Err(why) => {
                state.failure(&crate::lib::network::failure::panicked(&*why)); // the connection is refused: no protocol layer for it
                if counted {
                    self.admission.connection_closed(peer);
                }
                state.abandon();
                return; // the socket closes with the stream dropped here
            }
        };
        let chain = match &self.choose_chain {
            Some(choose) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| choose(peer).inspect(|c| c.tls_only()))) {
                Ok(c) => c,
                Err(why) => {
                    state.failure(&crate::lib::network::failure::panicked(&*why));
                    if counted {
                        self.admission.connection_closed(peer);
                    }
                    state.abandon();
                    return;
                }
            },
            None => self.chain.clone(),
        };
        state.set_chain(chain);
        unsafe {
            match engine::adopt(lp, stream, state.clone(), sink, source, if self.ws { Some(Codec::server()) } else { None }) {
                Ok(p) => {
                    if counted {
                        // its address is given back when the slot goes back to the pool
                        let me = self.clone();
                        (*p).on_gone = Some(Box::new(move || me.admission.connection_closed(peer)));
                    }
                    engine::limit_handshake(p, self.handshake_timeout_ms);
                    // the session can be used: its addresses are known, and a handler that aborts
                    // it here closes the slot the way any close does
                    engine::guarded(p, || {
                        state.fire(Event::REMOTE_CONNECT); // a WebSocket too: the socket is there, its actors start; the 101 says WEBSOCKET_REMOTE_CONNECT
                        if engine::active(p) {
                            engine::start(p);
                        }
                    });
                }
                Err(_) => {
                    state.disconnect(Event::THIS_CLOSE_ABRUPTLY);
                    if counted {
                        self.admission.connection_closed(peer);
                    }
                }
            }
        }
    }
}

/// What a dial seats on the slot it connects, on the slot's loop: the sink and the source of the
/// protocol layer the transport drives - `None`, there is none to seat.
pub(crate) type Seat = Box<dyn FnOnce(&Arc<ExternalConnection>) -> Option<(Box<dyn BytesDst>, Box<dyn BytesSrc>)> + Send>;

/// Who seats the protocol layer on the slot of a connect: a client of its own, the layer a parked
/// session had (its seat hook), or a closure of the caller. No closure of ours per connect.
pub(crate) enum SeatBy {
    Fn(Seat),
    Client(Arc<client::Core>),
    Hook,
}

impl SeatBy {
    fn seat(self, s: &Arc<ExternalConnection>) -> Option<(Box<dyn BytesDst>, Box<dyn BytesSrc>)> {
        match self {
            SeatBy::Fn(f) => f(s),
            SeatBy::Client(c) => c.seat_on(s),
            SeatBy::Hook => s.seat_on(s),
        }
    }
}

/// What the protocol layer just seated on `state` declared of Noise ([`ExternalConnection::secure`]), applied around its sink and
/// source: nothing declared - they stay as they are; a refusal - `None`; a chain - its Noise stream, `initiator` writing the first
/// message, the connection's own ([`ExternalConnection::noise`]).
pub(crate) fn noised(state: &Arc<ExternalConnection>, initiator: bool, sink: Box<dyn BytesDst>, source: Box<dyn BytesSrc>) -> Option<(Box<dyn BytesDst>, Box<dyn BytesSrc>)> {
    match state.take_secured() {
        None => Some((sink, source)),
        Some(None) => None,
        Some(Some(c)) if c.pattern < 0 => Some((sink, source)),
        Some(Some(c)) => Some(noise(state, &c, initiator, sink, source)),
    }
}

/// The Noise stream of `chain` around the sink and the source of the protocol layer on `state`.
pub(crate) fn noise(state: &Arc<ExternalConnection>, chain: &Chain, initiator: bool, sink: Box<dyn BytesDst>, source: Box<dyn BytesSrc>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) {
    let st = Arc::downgrade(state); // weak: the connection holds its Noise stream, the stream this hook
    let (sink, source, h) = noise_adapter::wrap_chain(
        chain,
        initiator,
        sink,
        source,
        Box::new(move || {
            if let Some(st) = st.upgrade() {
                st.wake_transmit_deferred()
            }
        }),
    );
    state.set_noise(h);
    (sink, source)
}

/// The protocol layer seated has no keys for the connection ([`ExternalConnection::secure`] with `None`).
pub(crate) fn refused() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "The protocol layer refused the connection: [Noise] of its description has no keys for it.",
    )
}

/// Who hears how a connect went - a client of its own, or a closure of the caller -, and, for a
/// reconnect, whether its session goes on and under which identity it parked.
pub(crate) struct Report {
    to: ReportTo,
    redial: Option<(bool, u64)>,
}

enum ReportTo {
    Fn(OnConnected),
    Client(Arc<client::Core>),
}

impl Report {
    pub(crate) fn of(f: OnConnected) -> Self {
        Self { to: ReportTo::Fn(f), redial: None }
    }

    pub(crate) fn client(c: Arc<client::Core>) -> Self {
        Self {
            to: ReportTo::Client(c),
            redial: None,
        }
    }

    /// Once: the connection whose session started, or what went wrong.
    pub(crate) fn call(self, r: std::io::Result<Arc<ExternalConnection>>) {
        if let (Ok(s), Some((resume, before))) = (&r, self.redial) {
            s.redialed();
            if resume && s.session() == before {
                s.resume(); // an identity kept across sockets that the connect handler did not re-identify
            }
        }
        match self.to {
            ReportTo::Fn(f) => f(r),
            ReportTo::Client(c) => c.connected(r),
        }
    }
}

/// A connect the connection carries to its loop ([`ExternalConnection::dial`], served at
/// `engine::CONNECT`): no closure for it.
pub(crate) enum Pending {
    /// A client's connect, after the teardown it came behind.
    Start(Arc<client::Core>),
    Dial(Dial),
    /// A reconnect of a parked session, after the teardown it came behind.
    Redial {
        engine: Arc<Engine>,
        addr: SocketAddr,
        timeout: Option<Duration>,
        ws: Option<Box<Codec>>,
        opts: SocketOptions,
        report: Report,
    },
}

/// What a connect does on its loop ([`dial`]).
pub(crate) struct Dial {
    engine: Arc<Engine>,
    addr: SocketAddr,
    until: Option<i64>,
    ws: Option<Box<Codec>>,
    opts: SocketOptions,
    from: SlotOf,
    resume: bool,
    seat: SeatBy,
    report: Report,
}

/// On the loop: what the connection carried.
pub(crate) fn pending(lp: &Loop, ext: &Arc<ExternalConnection>, p: Pending) {
    match p {
        Pending::Start(c) => c.start(),
        Pending::Dial(d) => dial_on(lp, d),
        Pending::Redial {
            engine,
            addr,
            timeout,
            ws,
            opts,
            report,
        } => redial(ext, &engine, addr, timeout, ws, opts, report),
    }
}

/// Where the slot of a dial comes from.
pub(crate) enum SlotOf {
    /// A slot of the clients of the engine, on the loop the engine picks.
    Clients,
    /// The slot of a client of its own, on its loop `home`: the one slot its pool has, taken off
    /// its free list again for every connect.
    Pool(Arc<Pool>, u8),
    /// The slot a session is parked on, on its own loop: a reconnect.
    Parked(Arc<ExternalConnection>),
}

/// Dials `addr` on a loop of `engine`, in the slot of the connection itself - the slot `from` names
/// -, the protocol layer `seat` gives seated on it first, then the socket connects there.
/// `on_connected` hears on that loop's thread: the state once the session has started - at once for
/// raw TCP, with the server's 101 for a WebSocket (`ws`) -, or what went wrong; `TimedOut` when
/// `timeout` ran out first (the loop times it as a wait of the slot). A reconnect that fails leaves
/// the session parked, for what is left of its wait - when `resume`: the session goes on; otherwise
/// the slot starts a session anew with the protocol layer it has. A connection that exists already
/// (`carrier`) carries the connect to its loop itself - no allocation for it -; otherwise it is a
/// task of the loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dial(
    engine: &Arc<Engine>,
    addr: SocketAddr,
    timeout: Option<Duration>,
    ws: Option<Box<Codec>>,
    opts: SocketOptions,
    from: SlotOf,
    resume: bool,
    seat: SeatBy,
    report: Report,
    carrier: Option<&Arc<ExternalConnection>>,
) {
    let until = timeout.map(Loop::deadline_of);
    let lp = match &from {
        SlotOf::Parked(s) => engine.loops[s.home as usize - 1].clone(),
        SlotOf::Pool(_, home) => engine.loops[*home as usize - 1].clone(),
        SlotOf::Clients => engine.pick().clone(),
    };
    let d = Dial {
        engine: engine.clone(),
        addr,
        until,
        ws,
        opts,
        from,
        resume,
        seat,
        report,
    };
    match carrier {
        Some(c) => {
            *c.dial.lock() = Some(Pending::Dial(d));
            c.request(&lp, engine::CONNECT);
        }
        None => lp.submit_fn(Box::new(move |lp: &Loop| dial_on(lp, d))),
    }
}

/// The connect, on its loop: the slot, the protocol layer seated on it, the socket connecting
/// there.
fn dial_on(lp: &Loop, d: Dial) {
    let Dial {
        engine: eng,
        addr,
        until,
        ws,
        opts,
        from,
        resume,
        seat,
        report,
    } = d;
    {
        let st = match from {
            SlotOf::Parked(s) => {
                s.redialing.store(resume, std::sync::atomic::Ordering::Release); // a close before the session starts parks it again
                let _ = s.activate();
                s
            }
            SlotOf::Pool(pool, _) => {
                let s = pool.allocate(lp.number);
                s.set_on_failure(eng.on_failure()); // once: the slot keeps the one it was first given
                s
            }
            SlotOf::Clients => eng.client_state(lp),
        };
        st.dialing.store(true, std::sync::atomic::Ordering::Release); // a slot that dials: its side names itself first
        st.forget_noise(); // the Noise of this socket is what the protocol layer seated now declares
        st.set_addresses(None, Some(addr)); // the region of the keys sees where it calls: the socket is not there yet
                                            // the factory is code of the application: its panic fails this connect - whoever waits for it hears it
        let seated = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| seat.seat(&st).map(|(sink, source)| noised(&st, true, sink, source)))) {
            Ok(seated) => seated,
            Err(why) => {
                let e = super::failure::panicked(&*why);
                st.failure(&e);
                st.disconnect(Event::THIS_CLOSE_ABRUPTLY);
                return report.call(Err(e));
            }
        };
        let Some(seated) = seated else {
            let e = std::io::Error::other("the session has no protocol layer to go on with");
            st.failure(&e);
            st.disconnect(Event::THIS_CLOSE_ABRUPTLY);
            return report.call(Err(e));
        };
        let Some((sink, source)) = seated else {
            let e = refused();
            st.failure(&e);
            st.disconnect(Event::THIS_CLOSE_ABRUPTLY);
            return report.call(Err(e));
        };
        let stream = match opts.connect_mio(addr, |e| st.failure(e)) {
            Ok(s) => s,
            Err(e) => {
                st.failure(&e);
                st.disconnect(Event::THIS_CLOSE_ABRUPTLY);
                return report.call(Err(e));
            }
        };
        // the slot hears the connect and reports it: no closure of ours (`engine::connected`)
        unsafe { engine::connect(lp, stream, st, sink, source, ws, until, report) };
    }
}

/// Requests graceful shutdown of every live connection and waits up to `grace`. Aborts remaining
/// connections afterward and stops a host-owned engine. A shared process engine remains available
/// to other hosts. Returns `true` when all sessions ended within the grace period. A nonzero grace
/// period must not be used from an engine loop thread.
pub(crate) fn stop(pool: &Pool, engine: &Arc<Engine>, grace: Duration) -> bool {
    assert!(
        grace.is_zero() || !engine::on_a_loop(),
        "A graceful shutdown waits for the loops: not from a thread of one ( the handler of a connection )."
    );
    let each = |politely: bool| {
        pool.each(|c| {
            if c.is_open() {
                if politely {
                    c.close_gracefully();
                } else {
                    c.abort();
                }
            }
        })
    };
    let mut quiet = true;
    if !grace.is_zero() {
        each(true);
        quiet = false;
        let until = std::time::Instant::now() + grace;
        while std::time::Instant::now() < until {
            if pool.connections_live() == 0 {
                quiet = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5)); // the caller waits for the sessions
        }
    }
    each(false);
    if !engine.is_shared() {
        engine.close();
    }
    quiet
}

pub(crate) fn redial(state: &Arc<ExternalConnection>, engine: &Arc<Engine>, addr: SocketAddr, timeout: Option<Duration>, ws: Option<Box<Codec>>, opts: SocketOptions, report: Report) {
    use std::sync::atomic::Ordering::Acquire;
    // the teardown this reconnect came behind (called from a close handler), or the reaper that
    // took the session as it came: the reconnect waits for it on the slot's loop, as a connect
    // during any teardown - carried by the connection itself, served on a later pass
    let behind = |state: &Arc<ExternalConnection>, engine: &Arc<Engine>, ws: Option<Box<Codec>>, opts: SocketOptions, report: Report| {
        *state.dial.lock() = Some(Pending::Redial {
            engine: engine.clone(),
            addr,
            timeout,
            ws,
            opts,
            report,
        });
        state.request(&engine.loops[state.home as usize - 1], engine::CONNECT);
    };
    match state.conn_state.load(Acquire) {
        pool::CLEANING => return behind(state, engine, ws, opts, report),
        pool::PARKED => {}
        // NotFound: the session is gone (the reaper took it) - a client starts anew on the free
        // slot
        _ => return report.call(Err(std::io::Error::new(std::io::ErrorKind::NotFound, "the connection holds no parked session"))),
    }
    let Some(host) = state.host() else {
        return report.call(Err(std::io::Error::other("the host of the connection is gone")));
    };
    let Some(parked) = host.parked(state.session()) else {
        return behind(state, engine, ws, opts, report);
    };
    // the wait ran out while the peer was away: the stale session ends - its close is heard - and
    // the slot connects anew with the protocol layer it has
    let resume = now() < parked.resumable.load(Acquire);
    if !resume {
        parked.end_stale();
    }
    let before = parked.session(); // the identity the session parked with
    let report = Report {
        redial: Some((resume, before)),
        ..report
    };
    let carrier = parked.clone();
    dial(engine, addr, timeout, ws, opts, SlotOf::Parked(parked), resume, SeatBy::Hook, report, Some(&carrier));
}

/// The clock of the deadlines of the sessions, in seconds ([`ExternalConnection::resumable`]).
#[inline]
fn now() -> i64 {
    external_connection::now_sec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Mutex;

    /// What a generated `seat_on` declares for `[Noise]` of `pattern`: this side's secret, the other
    /// side's public key.
    fn noise(pattern: i32, secret: &[u8; 32], peer: &[u8; 32]) -> Option<Chain> {
        Some(Chain::default().noise_keys(pattern, secret, peer))
    }

    /// Connects and waits for the report: a test waits for it, the loop is what gives it.
    fn connected(start: impl FnOnce(OnConnected)) -> std::io::Result<Arc<ExternalConnection>> {
        let (tx, rx) = std::sync::mpsc::channel();
        start(Box::new(move |r| {
            let _ = tx.send(r);
        }));
        rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| Err(std::io::Error::other("no report of the connect")))
    }

    /// Echoes what it gets through the source of the same connection.
    struct EchoSink {
        out: Arc<Mutex<Vec<u8>>>,
        echo: Arc<Mutex<Vec<u8>>>,
        state: Option<Arc<ExternalConnection>>,
    }
    impl BytesDst for EchoSink {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.out.lock().unwrap().extend_from_slice(bytes);
            self.echo.lock().unwrap().extend_from_slice(bytes);
            if let Some(s) = &self.state {
                s.wake_transmit();
            }
            true
        }
    }

    struct EchoSource(Arc<Mutex<Vec<u8>>>);
    impl BytesSrc for EchoSource {
        fn read(&mut self, dst: &mut [u8]) -> isize {
            let mut b = self.0.lock().unwrap();
            if b.is_empty() {
                return -1;
            }
            let n = b.len().min(dst.len());
            dst[..n].copy_from_slice(&b[..n]);
            b.drain(..n);
            n as isize
        }
    }

    /// Takes what comes, counted; with `take` it takes the storage of the receive buffer over
    /// whenever what came runs to the end of the bytes received, and keeps it, as a relay does.
    struct Taker {
        got: Arc<Mutex<usize>>,
        take: bool,
        kept: Arc<Mutex<Vec<crate::lib::network::buf::Buf>>>,
    }
    impl BytesDst for Taker {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            *self.got.lock().unwrap() += bytes.len();
            true
        }
        fn on_block(&mut self, block: &crate::lib::network::buf::RxBuf, at: usize, end: usize) -> Option<usize> {
            if self.take && block.ends_at(block.as_ptr().wrapping_add(end)) {
                let storage = block.take(vec![0u8; block.len()].into_boxed_slice());
                self.kept.lock().unwrap().push(storage);
            }
            *self.got.lock().unwrap() += end - at;
            Some(end - at)
        }
    }

    /// A connection has one receive buffer: one whose storage a relay takes over reads on into the
    /// one put in its place
    ///  - every byte arrives, and there is still the one buffer.
    #[test]
    fn a_receive_buffer_taken_over_is_replaced() {
        let lend = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(0usize));
        let loans = Arc::new(Mutex::new(Vec::new()));
        let states = Arc::new(Mutex::new(Vec::<Arc<ExternalConnection>>::new()));
        let (l2, g2, k2, s2) = (lend.clone(), got.clone(), loans.clone(), states.clone());
        let factory: ConnectionFactory = Arc::new(move |state| {
            s2.lock().unwrap().push(state.clone());
            (
                Box::new(Taker {
                    got: g2.clone(),
                    take: l2.load(Ordering::Relaxed),
                    kept: k2.clone(),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("test", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let addr = server.local_addr().unwrap();
        for (i, lends) in [false, true].into_iter().enumerate() {
            lend.store(lends, Ordering::Relaxed);
            *got.lock().unwrap() = 0;
            let payload: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();
            let outbox = Arc::new(Mutex::new(payload.clone()));
            let client = connected(|done| {
                client::connect(
                    addr,
                    Some(Duration::from_secs(2)),
                    move |_| {
                        (
                            Box::new(EchoSink {
                                out: Arc::new(Mutex::new(Vec::new())),
                                echo: Arc::new(Mutex::new(Vec::new())),
                                state: None,
                            }) as Box<dyn BytesDst>,
                            Box::new(EchoSource(outbox)) as Box<dyn BytesSrc>,
                        )
                    },
                    done,
                )
            })
            .unwrap();
            client.wake_transmit();
            for _ in 0..500 {
                if *got.lock().unwrap() == payload.len() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(*got.lock().unwrap(), payload.len(), "connection {i}");
            let s = states.lock().unwrap()[i].clone();
            assert_eq!(engine::receive_buffers(&s), 1, "takes {lends}");
            assert_eq!(!loans.lock().unwrap().is_empty(), lends, "takes {lends}: a storage taken over");
            loans.lock().unwrap().clear();
            client.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        }
        server.shutdown();
    }

    #[test]
    fn server_and_client_round_trip() {
        let received_on_server = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rs = received_on_server.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            let echo = Arc::new(Mutex::new(Vec::new()));
            (
                Box::new(EchoSink {
                    out: rs.clone(),
                    echo: echo.clone(),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(echo)) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("test", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let addr = server.local_addr().unwrap();
        let received_on_client = Arc::new(Mutex::new(Vec::<u8>::new()));
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i * 31) as u8).collect();
        let outbox = Arc::new(Mutex::new(payload.clone()));
        let rc = received_on_client.clone();
        let state = connected(|done| {
            client::connect(
                addr,
                Some(Duration::from_secs(2)),
                move |_| {
                    (
                        Box::new(EchoSink {
                            out: rc,
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(outbox)) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        state.wake_transmit();
        for _ in 0..200 {
            if received_on_client.lock().unwrap().len() == payload.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(*received_on_server.lock().unwrap() == payload);
        assert!(*received_on_client.lock().unwrap() == payload);
        state.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// A client of one connection: a connect while one is under way hears with it, a connect while
    /// connected is refused, and after a disconnect the next connect takes the same slot on the
    /// same loop.
    #[test]
    fn a_client_keeps_its_one_slot() {
        let engine = Engine::new("client host", 2, crate::lib::network::engine::MIN_BUFFER);
        let factory: ConnectionFactory = Arc::new(move |state| {
            let echo = Arc::new(Mutex::new(Vec::new()));
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: echo.clone(),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(echo)) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("for client", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .on_connect(factory)
            .engine(engine.clone())
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let g = got.clone();
        // what the protocol layer seated sees of the client while its session starts
        let me: Arc<std::sync::OnceLock<std::sync::Weak<Client>>> = Default::default();
        let seen_while_starting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (m, sw) = (me.clone(), seen_while_starting.clone());
        let client = Arc::new(Client::with_engine(
            "c",
            &engine,
            Arc::new(move |st| {
                if let Some(c) = m.get().and_then(|c| c.upgrade()) {
                    sw.store(c.connection().is_some_and(|x| Arc::ptr_eq(&x, st)), Ordering::Relaxed);
                }
                (
                    Box::new(EchoSink {
                        out: g.clone(),
                        echo: Arc::new(Mutex::new(Vec::new())),
                        state: None,
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Arc::new(Mutex::new(b"ping".to_vec())))) as Box<dyn BytesSrc>,
                )
            }),
        ));
        me.set(Arc::downgrade(&client)).ok();
        assert!(!client.is_connected() && client.connection().is_none());
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        client.connect(addr, None, move |r| tx.send(r.map(|s| s.index.0)).unwrap()).unwrap();
        let second = client.connect(addr, None, move |r| tx2.send(r.map(|s| s.index.0)).unwrap()); // under way: hears with the first
        let a = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        match second {
            Ok(()) => assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap(), a),
            // a loopback connect may be over before the second call: that one finds the client
            // connected
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists),
        }
        assert!(seen_while_starting.load(Ordering::Relaxed), "the connection is the client's while its session starts");
        assert!(client.is_connected() && !client.is_connecting());
        assert_eq!(client.to_string(), format!("Client c -> {addr}"));
        let first = client.connection().unwrap();
        first.wake_transmit();
        until("the echo", &|| got.lock().unwrap().as_slice() == b"ping");
        assert_eq!(client.connect(addr, None, |_| {}).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        client.disconnect();
        until("the disconnect", &|| first.conn_state.load(Ordering::Acquire) == pool::FREE);
        let again = connected(|done| client.connect(addr, None, done).unwrap()).unwrap();
        assert!(Arc::ptr_eq(&first, &again), "the same slot");
        assert_eq!(first.home, again.home);
        drop(client); // the slot keeps its client: the connection goes on
        std::thread::sleep(Duration::from_millis(50));
        assert!(again.is_active());
        again.close();
        server.shutdown();
    }

    /// A server and a client on an engine of their own: one loop, buffers of the least size.
    #[test]
    fn round_trip_on_an_engine_of_its_own() {
        let engine = Engine::new("own", 1, crate::lib::network::engine::MIN_BUFFER);
        assert_eq!(engine.buffer_size, crate::lib::network::engine::MIN_BUFFER);
        let received_on_server = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rs = received_on_server.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            let echo = Arc::new(Mutex::new(Vec::new()));
            (
                Box::new(EchoSink {
                    out: rs.clone(),
                    echo: echo.clone(),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(echo)) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("own", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .on_connect(factory)
            .engine(engine.clone())
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let received_on_client = Arc::new(Mutex::new(Vec::<u8>::new()));
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i * 31) as u8).collect();
        let outbox = Arc::new(Mutex::new(payload.clone()));
        let rc = received_on_client.clone();
        let state = connected(|done| {
            client::connect_with(
                addr,
                Some(Duration::from_secs(2)),
                &engine,
                move |_| {
                    (
                        Box::new(EchoSink {
                            out: rc,
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(outbox)) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        assert_eq!(state.buffer_size.load(std::sync::atomic::Ordering::Relaxed), crate::lib::network::engine::MIN_BUFFER);
        state.wake_transmit();
        for _ in 0..500 {
            if received_on_client.lock().unwrap().len() == payload.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(*received_on_server.lock().unwrap() == payload);
        assert!(*received_on_client.lock().unwrap() == payload);
        state.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// An engine closed with encrypted connections on it lets the connections and its loop go: a
    /// slot holds its connection, the connection its loops - the loop frees its slots as it ends; the
    /// Noise stream of a connection wakes it through a weak hold.
    #[test]
    fn a_closed_engine_lets_its_connections_go() {
        let (secret, public) = crate::lib::x25519::keypair();
        let (e, s, loops) = {
            let engine = Engine::new("closed", 1, crate::lib::network::engine::MIN_BUFFER);
            let received = Arc::new(Mutex::new(Vec::<u8>::new()));
            let rs = received.clone();
            let factory: ConnectionFactory = Arc::new(move |state| {
                state.secure(noise(crate::lib::noise::PATTERN_NK, &secret, &public));
                (
                    Box::new(EchoSink {
                        out: rs.clone(),
                        echo: Arc::new(Mutex::new(Vec::new())),
                        state: Some(state.clone()),
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                )
            });
            let server = server::Builder::new("closed", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .engine(engine.clone())
                .on_connect(factory)
                .build()
                .unwrap();
            let addr = server.local_addr().unwrap();
            let client = Client::with_engine(
                "closed",
                &engine,
                Arc::new(move |st| {
                    st.secure(noise(crate::lib::noise::PATTERN_NK, &[0u8; 32], &public));
                    (
                        Box::new(EchoSink {
                            out: Arc::new(Mutex::new(Vec::new())),
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(Arc::new(Mutex::new(b"x".to_vec())))) as Box<dyn BytesSrc>,
                    )
                }),
            );
            let state = connected(|done| client.connect(addr, Some(Duration::from_secs(2)), done).unwrap()).unwrap();
            state.wake_transmit();
            until("the byte", &|| received.lock().unwrap().len() == 1);
            assert!(state.noise().is_some_and(|n| n.ready()), "encrypted");
            let watch = (Arc::downgrade(&engine), Arc::downgrade(&state), engine.loops.iter().map(Arc::downgrade).collect::<Vec<_>>());
            client.shutdown();
            server.shutdown();
            engine.close();
            watch
        };
        until("the engine gone", &|| e.strong_count() == 0 && s.strong_count() == 0 && loops.iter().all(|l| l.strong_count() == 0));
    }

    /// An engine closed under a live encrypted connection - no shutdown tore it down: the Noise stream
    /// the connection keeps wakes it through a weak hold, the two do not keep each other.
    #[test]
    fn a_closed_engine_lets_its_live_encrypted_connection_go() {
        let (secret, _) = crate::lib::x25519::keypair();
        let seated = Arc::new(Mutex::new(None::<std::sync::Weak<ExternalConnection>>));
        let (e, loops, _raw) = {
            let engine = Engine::new("closed-live", 1, crate::lib::network::engine::MIN_BUFFER);
            let sd = seated.clone();
            let factory: ConnectionFactory = Arc::new(move |state| {
                *sd.lock().unwrap() = Some(Arc::downgrade(state));
                state.secure(noise(crate::lib::noise::PATTERN_NK, &secret, &[0u8; 32]));
                (
                    Box::new(EchoSink {
                        out: Arc::new(Mutex::new(Vec::new())),
                        echo: Arc::new(Mutex::new(Vec::new())),
                        state: None,
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                )
            });
            let server = server::Builder::new("closed-live", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .engine(engine.clone())
                .on_connect(factory)
                .build()
                .unwrap();
            let raw = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
            until("seated", &|| seated.lock().unwrap().is_some());
            let watch = (Arc::downgrade(&engine), engine.loops.iter().map(Arc::downgrade).collect::<Vec<_>>());
            engine.close();
            (watch.0, watch.1, raw)
        };
        let s = seated.lock().unwrap().take().unwrap();
        until("the engine gone", &|| e.strong_count() == 0 && s.strong_count() == 0 && loops.iter().all(|l| l.strong_count() == 0));
    }

    /// A client dropped without a shutdown goes once its session is over: its slot keeps it only
    /// while the session lasts.
    #[test]
    fn a_dropped_client_goes_with_its_session() {
        let engine = Engine::new("dropped", 1, crate::lib::network::engine::MIN_BUFFER);
        let factory: ConnectionFactory = Arc::new(|_| {
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("dropped", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory.clone())
            .build()
            .unwrap();
        let s = {
            let client = Client::with_engine("dropped", &engine, factory);
            let state = connected(|done| client.connect(server.local_addr().unwrap(), Some(Duration::from_secs(2)), done).unwrap()).unwrap();
            Arc::downgrade(&state)
        };
        until("the session goes on without the handle", &|| s.upgrade().is_some_and(|c| c.is_open()));
        s.upgrade().unwrap().disconnect(Event::THIS_CLOSE_ABRUPTLY);
        until("the client gone", &|| s.strong_count() == 0);
        server.shutdown();
        engine.close();
    }

    /// Sockets accepted for the loops of an engine go to them through the listener's handoffs,
    /// without an allocation per socket: every one of a burst gets its connection, and every loop
    /// gets some.
    #[test]
    fn accepted_sockets_reach_every_loop() {
        let engine = Engine::new("handoff", 4, crate::lib::network::engine::MIN_BUFFER);
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let s = seated.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            s.lock().unwrap().push(state.clone());
            (
                Box::new(EchoSink {
                    out: Default::default(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("handoff", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .on_connect(factory)
            .engine(engine.clone())
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        const C: usize = 160;
        let clients: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(move || (0..C / 4).map(|_| std::net::TcpStream::connect(addr).unwrap()).collect::<Vec<_>>()))
            .collect();
        let held: Vec<_> = clients.into_iter().flat_map(|t| t.join().unwrap()).collect();
        until("every socket has its connection", &|| server.census().live == C);
        let mut on = [0usize; 4];
        for c in seated.lock().unwrap().iter() {
            on[c.home as usize - 1] += 1;
        }
        assert!(on.iter().all(|&n| 0 < n), "every loop got some: {on:?}");
        drop(held);
        server.shutdown();
        engine.close();
    }

    /// Concurrent wake requests enqueue a connection at most once. Servicing clears requests and
    /// queue accounting so the connection remains usable after the request flood.
    #[test]
    fn a_flood_of_requests_is_one_push_and_leaves_nothing() {
        let engine = Engine::new("flood", 1, crate::lib::network::engine::MIN_BUFFER);
        let received_on_server = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rs = received_on_server.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            (
                Box::new(EchoSink {
                    out: rs.clone(),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("flood", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .on_connect(factory)
            .engine(engine.clone())
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let outbox = Arc::new(Mutex::new(Vec::<u8>::new()));
        let ob = outbox.clone();
        let state = connected(|done| {
            client::connect_with(
                addr,
                Some(Duration::from_secs(2)),
                &engine,
                move |_| {
                    (
                        Box::new(EchoSink {
                            out: Default::default(),
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(ob)) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        let counts = Arc::strong_count(&state);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let st = state.clone();
                std::thread::spawn(move || {
                    for i in 0..50_000 {
                        st.wake_transmit();
                        st.wake_receive();
                        if i % 64 == 0 {
                            st.deadline(60_000);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // not more than before: what was counted then may have held a passing count too (the report
        // of the connect)
        until("the loop served the requests", &|| {
            state.asks.requests.load(Ordering::Acquire) == 0 && Arc::strong_count(&state) <= counts
        });
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i * 13) as u8).collect();
        outbox.lock().unwrap().extend_from_slice(&payload);
        let st = state.clone();
        std::thread::spawn(move || st.wake_transmit()).join().unwrap(); // Wake transmission from another thread through the loop inbox.
        until("the bytes after the flood", &|| received_on_server.lock().unwrap().len() == payload.len());
        assert!(*received_on_server.lock().unwrap() == payload);
        state.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
        engine.close();
    }

    /// Gives its bytes up to `cut`, then says a pack waits (0) until `go`, then the rest, then that
    /// it is empty.
    struct Halting {
        bytes: Vec<u8>,
        at: usize,
        cut: usize,
        go: Arc<std::sync::atomic::AtomicBool>,
    }
    impl BytesSrc for Halting {
        fn read(&mut self, dst: &mut [u8]) -> isize {
            let end = if self.go.load(std::sync::atomic::Ordering::Acquire) { self.bytes.len() } else { self.cut };
            if self.at == end {
                return if end == self.bytes.len() { -1 } else { 0 };
            }
            let n = (end - self.at).min(dst.len());
            dst[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            n as isize
        }
    }

    /// A graceful close asked for while a pack waits half out: the FIN goes after the rest of it,
    /// not before.
    #[test]
    fn graceful_close_waits_for_a_pack_half_out() {
        let received_on_server = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rs = received_on_server.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            (
                Box::new(EchoSink {
                    out: rs.clone(),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("halt", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let addr = server.local_addr().unwrap();
        let payload: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (bytes, g) = (payload.clone(), go.clone());
        let state = connected(|done| {
            client::connect(
                addr,
                Some(Duration::from_secs(2)),
                move |_| {
                    (
                        Box::new(EchoSink {
                            out: Arc::new(Mutex::new(Vec::new())),
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(Halting { bytes, at: 0, cut: 500, go: g }) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        state.wake_transmit();
        for _ in 0..200 {
            if received_on_server.lock().unwrap().len() == 500 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(received_on_server.lock().unwrap().len(), 500);
        state.close_gracefully(); // the pack waits: no FIN yet
        std::thread::sleep(Duration::from_millis(100));
        go.store(true, std::sync::atomic::Ordering::Release);
        state.wake_transmit();
        for _ in 0..200 {
            if received_on_server.lock().unwrap().len() == payload.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(*received_on_server.lock().unwrap() == payload, "the FIN cut the pack: its rest did not get out");
        server.shutdown();
    }

    /// An encrypted connection - the Noise both protocol layers declare: the bytes go through, and a
    /// graceful close of a drained connection ends with the FIN - the server hears it, the close
    /// does not hang behind the record layer.
    #[test]
    fn an_encrypted_connection_closes_gracefully() {
        let (secret, public) = crate::lib::x25519::keypair();
        let received_on_server = Arc::new(Mutex::new(Vec::<u8>::new()));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (rs, cl) = (received_on_server.clone(), closed.clone());
        let factory: ConnectionFactory = Arc::new(move |state| {
            let c = cl.clone();
            state.set_event_sink(Arc::new(move |_, ev| {
                if ev.is_close() {
                    c.store(true, std::sync::atomic::Ordering::Release);
                }
            }));
            state.secure(noise(crate::lib::noise::PATTERN_NK, &secret, &[0u8; 32]));
            (
                Box::new(EchoSink {
                    out: rs.clone(),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("noise", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let addr = server.local_addr().unwrap();
        let payload: Vec<u8> = (0..5000u32).map(|i| (i * 13) as u8).collect();
        let outbox = Arc::new(Mutex::new(payload.clone()));
        let state = connected(|done| {
            client::connect(
                addr,
                Some(Duration::from_secs(2)),
                move |st| {
                    st.secure(noise(crate::lib::noise::PATTERN_NK, &[0u8; 32], &public));
                    (
                        Box::new(EchoSink {
                            out: Arc::new(Mutex::new(Vec::new())),
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(outbox)) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        state.wake_transmit();
        for _ in 0..400 {
            if received_on_server.lock().unwrap().len() == payload.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(*received_on_server.lock().unwrap() == payload);
        assert_eq!(state.keys().count(), 0, "the handshake gives the session no key of packs");
        assert!(state.noise().is_some_and(|n| !n.failed()), "the Noise stream of the session is seen from it ( Java `noise` )");
        state.close_gracefully();
        for _ in 0..400 {
            if closed.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            closed.load(std::sync::atomic::Ordering::Acquire),
            "the graceful close of a drained encrypted connection never sent the FIN"
        );
        server.shutdown();
    }

    /// The Noise of a connection is what its protocol layer declares as it is seated, every time: a
    /// client whose layer declares it is encrypted - the bytes go through -; the next connect of the
    /// same client, its layer declaring none, is not.
    #[test]
    fn the_noise_is_what_each_seat_declares() {
        let (secret, public) = crate::lib::x25519::keypair();
        let engine = Engine::new("noise-per-seat", 1, crate::lib::network::engine::MIN_BUFFER);
        let encrypt = Arc::new(AtomicBool::new(true));
        let received = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (rs, e) = (received.clone(), encrypt.clone());
        let factory: ConnectionFactory = Arc::new(move |state| {
            if e.load(Ordering::Relaxed) {
                state.secure(noise(crate::lib::noise::PATTERN_NK, &secret, &[0u8; 32]));
            }
            (
                Box::new(EchoSink {
                    out: rs.clone(),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("noise-per-seat", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let e = encrypt.clone();
        let client = Client::with_engine(
            "c",
            &engine,
            Arc::new(move |st| {
                if e.load(Ordering::Relaxed) {
                    st.secure(noise(crate::lib::noise::PATTERN_NK, &[0u8; 32], &public));
                }
                (
                    Box::new(EchoSink {
                        out: Arc::new(Mutex::new(Vec::new())),
                        echo: Arc::new(Mutex::new(Vec::new())),
                        state: None,
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Arc::new(Mutex::new(b"secret".to_vec())))) as Box<dyn BytesSrc>,
                )
            }),
        );
        let state = connected(|done| client.connect(addr, Some(Duration::from_secs(2)), done).unwrap()).unwrap();
        state.wake_transmit();
        until("the bytes through the encryption", &|| received.lock().unwrap().as_slice() == b"secret");
        assert!(state.noise().is_some_and(|n| n.ready()), "encrypted");
        client.disconnect();
        until("the disconnect", &|| state.conn_state.load(Ordering::Acquire) == pool::FREE);
        encrypt.store(false, Ordering::Relaxed);
        received.lock().unwrap().clear();
        let plain = connected(|done| client.connect(addr, Some(Duration::from_secs(2)), done).unwrap()).unwrap();
        plain.wake_transmit();
        until("the bytes in the clear", &|| received.lock().unwrap().as_slice() == b"secret");
        assert!(plain.noise().is_none(), "no Noise declared: not encrypted");
        plain.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// Both ends declare `[Noise]` of `pattern` as the generated `seat_on` does - each its own secret
    /// and the public key of the other -, and the bytes go there and back; the server's `on_peer`
    /// hears the key the client proved. `refuse`: it refuses that key - the connection breaks,
    /// nothing gets through.
    fn noise_both_ways(pattern: i32, refuse: bool) {
        let (server_secret, server_public) = crate::lib::x25519::keypair();
        let (client_secret, client_public) = crate::lib::x25519::keypair();
        let received_on_server = Arc::new(Mutex::new(Vec::<u8>::new()));
        let heard = Arc::new(Mutex::new(Vec::<[u8; 32]>::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let (rs, h, cl) = (received_on_server.clone(), heard.clone(), closed.clone());
        let factory: ConnectionFactory = Arc::new(move |state| {
            let (h, c) = (h.clone(), cl.clone());
            state.set_event_sink(Arc::new(move |_, ev| {
                if ev.is_close() {
                    c.store(true, Ordering::Release)
                }
            }));
            state.secure(Some(Chain::default().noise_keys(pattern, &server_secret, &client_public).on_peer(move |k| {
                h.lock().unwrap().push(*k);
                !refuse
            })));
            let echo = Arc::new(Mutex::new(Vec::new()));
            (
                Box::new(EchoSink {
                    out: rs.clone(),
                    echo: echo.clone(),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(echo)) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("noise-both", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let addr = server.local_addr().unwrap();
        let payload: Vec<u8> = (0..40_000u32).map(|i| (i * 29) as u8).collect();
        let (outbox, received_on_client) = (Arc::new(Mutex::new(payload.clone())), Arc::new(Mutex::new(Vec::<u8>::new())));
        let (ob, rc) = (outbox.clone(), received_on_client.clone());
        let state = connected(|done| {
            client::connect(
                addr,
                Some(Duration::from_secs(2)),
                move |st| {
                    st.secure(noise(pattern, &client_secret, &server_public));
                    (
                        Box::new(EchoSink {
                            out: rc,
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(ob)) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        state.wake_transmit();
        if refuse {
            until("the server breaks the connection", &|| closed.load(Ordering::Acquire));
            until("so does the client", &|| !state.is_open());
            assert!(received_on_server.lock().unwrap().is_empty(), "nothing got through");
        } else {
            until("the bytes there and back", &|| received_on_client.lock().unwrap().len() == payload.len());
            assert!(*received_on_server.lock().unwrap() == payload);
            assert!(*received_on_client.lock().unwrap() == payload);
            assert!(state.noise().is_some_and(|n| n.ready() && !n.failed()));
            state.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        }
        if pattern != crate::lib::noise::PATTERN_KK {
            assert_eq!(heard.lock().unwrap().as_slice(), &[client_public], "the key the client proved");
        }
        server.shutdown();
    }

    #[test]
    fn noise_kk_both_ends_declare() {
        noise_both_ways(crate::lib::noise::PATTERN_KK, false);
    }

    #[test]
    fn noise_ik_both_ends_declare() {
        noise_both_ways(crate::lib::noise::PATTERN_IK, false);
    }

    #[test]
    fn noise_xx_key_refused_breaks_the_connection() {
        noise_both_ways(crate::lib::noise::PATTERN_XX, true);
    }

    /// The protocol layer seated on an accepted socket has no keys for it ( `secure(None)` ): the
    /// connection is closed, and its failure goes to the server.
    #[test]
    fn a_refused_seat_closes_the_accepted_socket() {
        use std::io::Read;
        let failures = Arc::new(Mutex::new(Vec::<String>::new()));
        let fl = failures.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            state.secure(None);
            (
                Box::new(EchoSink {
                    out: Default::default(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("refused", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .on_connect(factory)
            .on_failure(Arc::new(move |_, e| fl.lock().unwrap().push(e.to_string())))
            .build()
            .unwrap();
        let mut raw = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 16];
        match raw.read(&mut b) {
            Ok(0) => {}
            Err(e) if e.kind() != std::io::ErrorKind::WouldBlock && e.kind() != std::io::ErrorKind::TimedOut => {}
            r => panic!("the socket is not closed: {r:?}"),
        }
        until("the failure", &|| failures.lock().unwrap().iter().any(|f| f.contains("[Noise]")));
        assert_eq!(server.pool.connections_live(), 0);
        server.shutdown();
    }

    /// A chain of the host is TLS alone: Noise is of the protocol layer.
    #[test]
    #[should_panic(expected = "Noise of a connection is set by [Noise] of its description, not in code")]
    fn a_host_chain_takes_no_noise() {
        let _ = server::Builder::new("tls", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).chain(Chain::default().noise_nk_server(&[1u8; 32]));
    }

    /// A multiplexed port with Noise: the byte naming the connection goes in the clear, the handshake
    /// of the Noise the named connection declares right behind it, then its records - the server
    /// opens them.
    #[test]
    fn a_multiplexed_port_names_the_connection_in_the_clear_and_encrypts_the_rest() {
        use std::io::Read;
        let (secret, public) = crate::lib::x25519::keypair();
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let g = got.clone();
        let named: ConnectionFactory = Arc::new(move |state| {
            state.secure(noise(crate::lib::noise::PATTERN_NK, &secret, &[0u8; 32]));
            (
                Box::new(EchoSink {
                    out: g.clone(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
            )
        });
        let pick: super::super::mux::MuxPick = Arc::new(move |uid| (uid == 7).then(|| named.clone()));
        let server = server::Builder::new("mux", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .on_connect(super::super::mux::factory(pick, Event::REMOTE_CONNECT))
            .build()
            .unwrap();
        let seat = move |st: &Arc<ExternalConnection>| {
            st.secure(noise(crate::lib::noise::PATTERN_NK, &[0u8; 32], &public));
            (
                Box::new(EchoSink {
                    out: Default::default(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(b"plain text of the layer".to_vec())))) as Box<dyn BytesSrc>,
            )
        };
        // what goes on the wire: a listener that only reads
        let wire = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let peek = connected(|done| client::connect(wire.local_addr().unwrap(), Some(Duration::from_secs(2)), super::super::mux::dialing(7, seat.clone()), done)).unwrap();
        peek.wake_transmit();
        let (mut s, _) = wire.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut head = [0u8; 2 + 48];
        s.read_exact(&mut head).unwrap();
        assert_eq!(head[0], 7, "the byte naming the connection, in the clear");
        assert_eq!(head[1], 48, "then the first message of NK: the ephemeral key and the tag");
        peek.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        // and the server opens what comes behind the handshake
        let state = connected(|done| client::connect(server.local_addr().unwrap(), Some(Duration::from_secs(2)), super::super::mux::dialing(7, seat), done)).unwrap();
        state.wake_transmit();
        until("the bytes through the Noise of the named connection", &|| got.lock().unwrap().as_slice() == b"plain text of the layer");
        assert!(state.noise().is_some_and(|n| n.ready()), "encrypted");
        state.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// Bytes that do not open as Noise break the stream for good: the server aborts the connection
    /// - its own verdict, the session does not park.
    #[test]
    fn a_forged_noise_record_aborts_the_connection() {
        let (secret, _) = crate::lib::x25519::keypair();
        let events = Arc::new(Mutex::new(Vec::<Event>::new()));
        let ev = events.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            let e = ev.clone();
            state.set_event_sink(Arc::new(move |_, event| e.lock().unwrap().push(event)));
            state.secure(noise(crate::lib::noise::PATTERN_NK, &secret, &[0u8; 32]));
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("forged", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let mut raw = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        std::io::Write::write_all(&mut raw, &[0x55u8; 4096]).unwrap();
        for _ in 0..400 {
            if events.lock().unwrap().iter().any(|e| e.is_close()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(events.lock().unwrap().iter().copied().filter(|e| e.is_close()).collect::<Vec<_>>(), vec![Event::THIS_CLOSE_ABRUPTLY]);
        server.shutdown();
    }

    /// A sink that panics: the handler of the host hears it and the connection goes on - the bytes
    /// it did not take are offered again with the next ones.
    #[test]
    fn a_panicking_sink_is_reported_and_the_connection_goes_on() {
        struct Flaky {
            got: Arc<Mutex<Vec<u8>>>,
            once: bool,
        }
        impl BytesDst for Flaky {
            fn on_bytes(&mut self, bytes: &[u8]) -> bool {
                if !self.once {
                    self.once = true;
                    panic!("the sink panics once");
                }
                self.got.lock().unwrap().extend_from_slice(bytes);
                true
            }
        }
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (g, f) = (got.clone(), failures.clone());
        let factory: ConnectionFactory = Arc::new(move |_| {
            (
                Box::new(Flaky { got: g.clone(), once: false }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("flaky", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .on_connect(factory)
            .on_failure(Arc::new(move |_: &dyn std::fmt::Display, _: &(dyn std::error::Error + 'static)| {
                f.fetch_add(1, Ordering::Relaxed);
            }))
            .build()
            .unwrap();
        let outbox = Arc::new(Mutex::new(b"abc".to_vec()));
        let ob = outbox.clone();
        let state = connected(|done| {
            client::connect(
                server.local_addr().unwrap(),
                Some(Duration::from_secs(2)),
                move |_| {
                    (
                        Box::new(EchoSink {
                            out: Arc::new(Mutex::new(Vec::new())),
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(ob)) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        state.wake_transmit();
        for _ in 0..400 {
            if failures.load(Ordering::Relaxed) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(failures.load(Ordering::Relaxed), 1, "the panic is the host's failure");
        outbox.lock().unwrap().extend_from_slice(b"def");
        state.wake_transmit();
        for _ in 0..400 {
            if got.lock().unwrap().len() == 6 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(&got.lock().unwrap()[..], b"abcdef", "the connection lives: what was refused comes with what follows");
        assert!(crate::lib::network::TCP::external_connection::ExternalConnection::is_open(&*state));
        server.shutdown();
    }

    /// The host's engine at rest after traffic holds no transmit buffer, a session takes the linger
    /// of its host when it starts, and the shutdown of the host ends the threads of its own engine.
    #[test]
    fn an_engine_at_rest_holds_no_buffer_and_the_shutdown_ends_it() {
        let engine = Engine::new("held", 1, crate::lib::network::engine::MIN_BUFFER);
        let seen = Arc::new(Mutex::new(None::<Arc<ExternalConnection>>));
        let sn = seen.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            *sn.lock().unwrap() = Some(state.clone());
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(vec![7u8; 10_000])))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("held", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        server.set_linger_timeout(1234);
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let g = got.clone();
        let state = connected(|done| {
            client::connect(
                server.local_addr().unwrap(),
                Some(Duration::from_secs(2)),
                move |_| {
                    (
                        Box::new(EchoSink {
                            out: g,
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        for _ in 0..400 {
            if got.lock().unwrap().len() == 10_000 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got.lock().unwrap().len(), 10_000);
        assert_eq!(seen.lock().unwrap().as_ref().unwrap().linger_timeout_ms.load(Ordering::Relaxed), 1234, "the host's linger");
        assert_eq!(engine.buffers_held(), 0, "at rest: every transmit buffer is back on its loop");
        state.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
        assert_eq!(engine.buffers_held(), 0, "stopped: nobody to ask, nothing to wait for");
        assert!(!engine.is_shared() && Engine::shared().is_shared());
    }

    /// A listener closed takes nothing in from then on, though its loop closes the socket later:
    /// closed by the host as it takes the first socket of a burst, the rest of the burst is not
    /// given to it. `buffers_held` asks no loop: from a loop's own thread it answers too.
    #[test]
    fn a_closed_listener_takes_nothing_in() {
        struct Count(std::sync::atomic::AtomicUsize, std::sync::OnceLock<Listening>);
        impl Accept for Count {
            fn admit(&self, _: SocketAddr) -> Option<bool> {
                Some(false)
            }
            fn accepted(self: Arc<Self>, _: &Loop, _: mio::net::TcpStream, _: SocketAddr, _: bool) {
                if self.0.fetch_add(1, Ordering::SeqCst) == 1 {
                    self.1.get().unwrap().close(); // the first of the burst
                }
            }
            fn failure(&self, _: &(dyn std::error::Error + 'static)) {}
        }
        let engine = Engine::new("deaf", 1, crate::lib::network::engine::MIN_BUFFER);
        let host = Arc::new(Count(Default::default(), Default::default()));
        let l = engine.listen(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0), 50, None, host.clone()).unwrap();
        let addr = l.local_addr();
        let _ = host.1.set(l);
        let first = std::net::TcpStream::connect(addr).unwrap();
        until("one taken in", &|| host.0.load(Ordering::SeqCst) == 1);
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (busy, is_busy) = std::sync::mpsc::channel::<usize>();
        let e = engine.clone();
        engine.execute(move || {
            busy.send(e.buffers_held()).unwrap(); // on the loop: no waiting for itself
            let _ = wait.recv_timeout(Duration::from_secs(10)); // the loop busy: the connects below wait in the backlog
        });
        assert_eq!(is_busy.recv_timeout(Duration::from_secs(10)).unwrap(), 0);
        let backlog: Vec<_> = (0..4).map(|_| std::net::TcpStream::connect(addr).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(50));
        go.send(()).unwrap(); // one readiness for the four: accepted in one burst
        until("the burst", &|| host.0.load(Ordering::SeqCst) >= 2);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(host.0.load(Ordering::SeqCst), 2, "nothing after the close");
        drop((first, backlog));
        engine.close();
    }

    /// An engine that stops closes what its loops hold: the port of its listener is free again at
    /// once.
    #[test]
    fn a_stopped_engine_closes_its_listeners() {
        let engine = Engine::new("stop", 1, crate::lib::network::engine::MIN_BUFFER);
        let factory: ConnectionFactory = Arc::new(move |_| {
            (
                Box::new(EchoSink {
                    out: Default::default(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("stop", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        // a connect is refused once the listener is closed - a bind would not tell it: the port
        // freed may go to a socket of another test at once
        let refused = || std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err();
        assert!(!refused(), "listening");
        engine.close();
        let mut closed = false;
        for _ in 0..400 {
            if refused() {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(closed, "the listener of a stopped loop closes");
    }

    /// A graceful close asked while the parked session of a client reconnects tears the connect
    /// down, on the loop's thread as from any other (the task of another thread does): the
    /// reconnect hears it at once, not when its timeout runs out.
    #[test]
    fn a_graceful_close_while_reconnecting_ends_the_connect() {
        let engine = Engine::new("close-connecting", 1, crate::lib::network::engine::MIN_BUFFER);
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let s = seated.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            s.lock().unwrap().push(state.clone());
            (
                Box::new(EchoSink {
                    out: Default::default(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("close-connecting", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let seat = layer(Default::default(), Default::default(), Default::default(), None);
        let state = connected(|done| client::connect_with(addr, Some(Duration::from_secs(2)), &engine, seat, done)).unwrap();
        state.identify(9);
        until("seated on the server", &|| seated.lock().unwrap().len() == 1);
        seated.lock().unwrap()[0].disconnect(Event::THIS_CLOSE_ABRUPTLY);
        until("the client parks its session", &|| engine.clients.census()[pool::PARKED as usize] == 1);

        let blackhole = SocketAddr::new(Ipv4Addr::new(10, 255, 255, 1).into(), 9); // a SYN nobody answers
        let at = std::time::Instant::now();
        let st = state.clone();
        let lp = engine.loops[state.home as usize - 1].clone();
        let r = connected(|done| {
            client::reconnect(&state, blackhole, Some(Duration::from_secs(5)), &engine, done);
            lp.submit_fn(Box::new(move |_| st.close_gracefully())); // on the loop's thread, behind the connect it starts
        });
        assert!(r.is_err(), "no session");
        assert!(
            at.elapsed() < Duration::from_secs(3),
            "the connect ends with the close, not with its timeout: {:?} {:?}",
            at.elapsed(),
            r.err()
        );
        server.shutdown();
        engine.close();
    }

    /// A negative transmit timeout set on a connection with nothing to send closes it at once: the
    /// FIN goes, the peer closes, so does this side.
    #[test]
    fn a_negative_timeout_closes_an_idle_connection() {
        let factory: ConnectionFactory = Arc::new(move |state| {
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("idle", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let addr = server.local_addr().unwrap();
        let state = connected(|done| {
            client::connect(
                addr,
                Some(Duration::from_secs(2)),
                move |_| {
                    (
                        Box::new(EchoSink {
                            out: Arc::new(Mutex::new(Vec::new())),
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(crate::lib::network::TCP::external_connection::ExternalConnection::is_open(&*state));
        crate::lib::adhoc::connection::external::External::set_transmit_timeout(&*state, -1000);
        for _ in 0..400 {
            if !crate::lib::network::TCP::external_connection::ExternalConnection::is_open(&*state) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !crate::lib::network::TCP::external_connection::ExternalConnection::is_open(&*state),
            "nothing to send: the close does not wait for anything"
        );
        server.shutdown();
    }

    /// The session hears it is open once the socket is there, and that it is over once, after the
    /// closing event: on both sides.
    #[test]
    fn the_session_opens_and_closes_once() {
        let heard = Arc::new(Mutex::new(Vec::<&str>::new()));
        let h = heard.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            let (o, e) = (h.clone(), h.clone());
            state.set_session_hook(Arc::new(move |open| o.lock().unwrap().push(if open { "server open" } else { "server close" })));
            state.set_event_sink(Arc::new(move |_, ev| {
                if ev.is_close() {
                    e.lock().unwrap().push("server event")
                }
            }));
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("session", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let addr = server.local_addr().unwrap();
        let h = heard.clone();
        let state = connected(|done| {
            client::connect(
                addr,
                Some(Duration::from_secs(2)),
                move |st| {
                    let o = h.clone();
                    st.set_session_hook(Arc::new(move |open| o.lock().unwrap().push(if open { "client open" } else { "client close" })));
                    (
                        Box::new(EchoSink {
                            out: Arc::new(Mutex::new(Vec::new())),
                            echo: Arc::new(Mutex::new(Vec::new())),
                            state: None,
                        }) as Box<dyn BytesDst>,
                        Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        crate::lib::network::TCP::external_connection::ExternalConnection::close(&*state);
        for _ in 0..400 {
            if heard.lock().unwrap().len() == 5 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        let mut got = heard.lock().unwrap().clone();
        let at = |w: &str| got.iter().position(|x| *x == w).unwrap_or(usize::MAX);
        assert!(at("server event") < at("server close"), "OnClose after the closing event: {got:?}");
        got.sort();
        assert_eq!(got, vec!["client close", "client open", "server close", "server event", "server open"]);
        server.shutdown();
    }

    /// A client connects in the slot of its connection: the protocol layer is seated there first; a
    /// connect refused or not over in time is heard by whoever waits, and the slot goes back to its
    /// loop - the next connect of that loop connects in it.
    #[test]
    fn a_client_connects_in_its_own_slot() {
        let engine = Engine::new("connect-in-slot", 1, crate::lib::network::engine::MIN_BUFFER);
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let dial = |addr: SocketAddr, timeout: Duration| {
            let s = seated.clone();
            connected(|done| {
                client::connect_with(
                    addr,
                    Some(timeout),
                    &engine,
                    move |st| {
                        s.lock().unwrap().push(st.clone());
                        (
                            Box::new(EchoSink {
                                out: Arc::new(Mutex::new(Vec::new())),
                                echo: Arc::new(Mutex::new(Vec::new())),
                                state: None,
                            }) as Box<dyn BytesDst>,
                            Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                        )
                    },
                    done,
                )
            })
        };
        let free = || engine.clients.census()[pool::FREE as usize];
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}");
        };
        // refused: nobody listens on the port
        let closed = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap();
        assert!(dial(closed, Duration::from_secs(2)).is_err(), "nobody listens");
        assert_eq!(seated.lock().unwrap().len(), 1, "the protocol layer was seated before the connect, as Java's");
        wait("the slot of the refused connect is free", &|| free() == 1);
        // not over in time: a peer that never answers
        let silent = SocketAddr::new(Ipv4Addr::new(10, 255, 255, 1).into(), 9);
        let r = dial(silent, Duration::from_millis(200));
        assert!(r.is_err(), "nobody answers");
        wait("the slot of the connect not over in time is free", &|| free() == 1);
        let first = seated.lock().unwrap()[0].clone();
        assert!(Arc::ptr_eq(&first, &seated.lock().unwrap()[1]), "the next connect of the loop connected in the same slot");
        // connected: the slot is the connection
        let server = server::Builder::new("connect-in-slot", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(Arc::new(|_| {
                (
                    Box::new(EchoSink {
                        out: Arc::new(Mutex::new(Vec::new())),
                        echo: Arc::new(Mutex::new(Vec::new())),
                        state: None,
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                )
            }))
            .build()
            .unwrap();
        let st = dial(server.local_addr().unwrap(), Duration::from_secs(2)).unwrap();
        assert!(Arc::ptr_eq(&st, &seated.lock().unwrap()[2]) && Arc::ptr_eq(&st, &first), "the slot it connected in is the connection");
        assert!(st.is_open() && st.remote() == server.local_addr().ok());
        assert_eq!(engine.clients.connections_size(), 1, "{}", engine.clients.census_text());
        st.abort();
        server.shutdown();
    }

    /// A connection over, its slot serves the next one: the same object, found new - no identity,
    /// no hooks of the session before.
    #[test]
    fn a_slot_serves_the_next_connection() {
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let s = seated.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            assert_eq!(state.session() as u32, 0, "a slot comes new");
            assert!(state.is_active() && state.is_open());
            state.identify(9);
            s.lock().unwrap().push(state.clone());
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        // one loop: a slot freed on a loop serves the next socket of that loop
        let server = server::Builder::new("reuse", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(Engine::new("reuse", 1, crate::lib::network::engine::MIN_BUFFER))
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let dial = || {
            connected(|done| {
                client::connect(
                    addr,
                    Some(Duration::from_secs(2)),
                    |_| {
                        (
                            Box::new(EchoSink {
                                out: Arc::new(Mutex::new(Vec::new())),
                                echo: Arc::new(Mutex::new(Vec::new())),
                                state: None,
                            }) as Box<dyn BytesDst>,
                            Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                        )
                    },
                    done,
                )
            })
            .unwrap()
        };
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}");
        };
        let first = dial();
        wait("the first seated", &|| seated.lock().unwrap().len() == 1);
        first.abort();
        wait("its slot is free", &|| server.pool.census()[pool::FREE as usize] == 1);
        let gone = seated.lock().unwrap()[0].clone();
        assert!(!gone.is_open() && gone.session() as u32 == 0 && gone.link().is_none(), "nothing of the session is left on it");
        let place = gone.slot.load(Ordering::Relaxed);
        assert_ne!(place, u32::MAX);
        let _second = dial();
        wait("the second seated", &|| seated.lock().unwrap().len() == 2);
        assert!(Arc::ptr_eq(&seated.lock().unwrap()[1], &gone), "the slot served the next connection");
        assert_eq!(gone.slot.load(Ordering::Relaxed), place, "its place on its loop - and its waits there - went on with it");
        assert_eq!(server.pool.connections_size(), 1, "{}", server.pool.census_text());
        server.shutdown();
    }

    /// A lost session parks with its connection number: the next connection gets another, and the
    /// number goes back only when the session is claimed.
    #[test]
    fn a_parked_session_keeps_its_number() {
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let s = seated.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            state.set_resumable_seconds_hook(Arc::new(|_| 60));
            state.identify(5);
            s.lock().unwrap().push(state.clone());
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("park", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(Engine::new("park", 1, crate::lib::network::engine::MIN_BUFFER))
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let dial = || {
            connected(|done| {
                client::connect(
                    addr,
                    Some(Duration::from_secs(2)),
                    |_| {
                        (
                            Box::new(EchoSink {
                                out: Arc::new(Mutex::new(Vec::new())),
                                echo: Arc::new(Mutex::new(Vec::new())),
                                state: None,
                            }) as Box<dyn BytesDst>,
                            Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                        )
                    },
                    done,
                )
            })
            .unwrap()
        };
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}");
        };
        let first = dial();
        wait("the first seated", &|| seated.lock().unwrap().len() == 1);
        let lost = seated.lock().unwrap()[0].clone();
        let token = lost.session();
        first.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        wait("the session parked", &|| server.pool.census()[pool::PARKED as usize] == 1);
        assert_ne!(lost.home, 0, "its own loop reaps the session");
        std::thread::sleep(Duration::from_millis(50)); // the slot of the lost connection is reaped
        let _second = dial();
        wait("the second seated", &|| seated.lock().unwrap().len() == 2);
        let taker = seated.lock().unwrap()[1].clone();
        assert_ne!(taker.index, lost.index, "a parked session's number went to another connection");
        use crate::lib::adhoc::connection::external::External;
        let foreign = (token as u32).wrapping_add(1); // a token someone guessed: the login of this connection proved another identity
        assert_eq!(External::reclaim_id(&*taker, token, foreign) as u32, foreign);
        assert_eq!(server.pool.census()[pool::PARKED as usize], 1, "a token of another identity took the session");
        assert_eq!(External::reclaim_id(&*taker, token, token as u32) as u32, token as u32); // this one proved the identity of the token
        assert_eq!(server.pool.census()[pool::PARKED as usize], 0, "the session was not claimed");
        // the claimed slot goes back to the free list on its own loop, through its door: the next connection takes it once
        // it is there
        wait("the claimed slot is free", &|| lost.conn_state.load(Ordering::Acquire) == pool::FREE);
        let _third = dial();
        wait("the third seated", &|| seated.lock().unwrap().len() == 3);
        assert_eq!(seated.lock().unwrap()[2].index, lost.index, "the claimed session's number did not go back");
        server.shutdown();
    }

    /// A server listens on more than one address, and on one more later: the connections of each
    /// are its own; once deaf it takes no address more.
    #[test]
    fn a_server_listens_on_several_addresses() {
        let engine = Engine::new("multi-bind", 1, crate::lib::network::engine::MIN_BUFFER);
        let seated = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = seated.clone();
        let factory: ConnectionFactory = Arc::new(move |_| {
            s.fetch_add(1, Ordering::Relaxed);
            (
                Box::new(EchoSink {
                    out: Default::default(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("multi", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let second = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0), 16).unwrap();
        assert_eq!(server.local_addrs().len(), 2);
        assert!(server.to_string().starts_with("Server multi") && server.to_string().contains(&second.to_string()), "{server}");
        let _a = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        let _b = std::net::TcpStream::connect(second).unwrap();
        let t = std::time::Instant::now();
        while seated.load(Ordering::Relaxed) < 2 {
            assert!(t.elapsed() < Duration::from_secs(5), "both addresses take connections");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.pool.connections_live(), 2);
        server.deafen();
        assert!(server.local_addrs().is_empty(), "deaf: the addresses are given up");
        let again = server.bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0), 16).unwrap(); // listens anew
        let _c = std::net::TcpStream::connect(again).unwrap();
        let t = std::time::Instant::now();
        while seated.load(Ordering::Relaxed) < 3 {
            assert!(t.elapsed() < Duration::from_secs(5), "a bind after deafen takes connections");
            std::thread::sleep(Duration::from_millis(5));
        }
        server.shutdown();
    }

    /// The servicing of a connection runs on the tick of its loop, at the pace it asks for, until
    /// it says it has nothing more to do.
    #[test]
    fn a_connection_is_serviced_on_the_tick_of_its_loop() {
        let engine = Engine::new("maintain", 1, crate::lib::network::engine::MIN_BUFFER);
        engine.set_tick(10);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            let c = c.clone();
            state.set_maintenance(Arc::new(move |ext| {
                assert!(ext.is_active());
                if c.fetch_add(1, Ordering::Relaxed) + 1 == 5 {
                    i32::MAX
                } else {
                    20
                }
            }));
            (
                Box::new(EchoSink {
                    out: Default::default(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("maintain", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let _a = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        let t = std::time::Instant::now();
        while calls.load(Ordering::Relaxed) < 5 {
            assert!(t.elapsed() < Duration::from_secs(5), "serviced {} times", calls.load(Ordering::Relaxed));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(Duration::from_millis(70) <= t.elapsed(), "at the pace it asked for: {:?}", t.elapsed());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::Relaxed), 5, "nothing more to do: no more calls");
        server.shutdown();
    }

    /// A reconnect called from `OnClose` - the slot still torn down - waits for the teardown and
    /// goes on; one called after the wait of the session ran out ends the stale session - its close
    /// is heard - and the slot starts a new one with the protocol layer it has: not resumed, no
    /// identity.
    #[test]
    fn a_client_reconnects_from_its_close_and_after_its_wait() {
        let engine = Engine::new("client-redial", 1, crate::lib::network::engine::MIN_BUFFER);
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let s = seated.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            s.lock().unwrap().push(state.clone());
            (
                Box::new(EchoSink {
                    out: Default::default(),
                    echo: Default::default(),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("client-redial", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let me: Arc<std::sync::OnceLock<Arc<ExternalConnection>>> = Default::default();
        let from_close: Arc<Mutex<Option<bool>>> = Default::default(); // the report of the reconnect made in OnClose
        let armed = Arc::new(AtomicBool::new(true));
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seat_on: Arc<dyn Fn(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + Sync> = {
            let (me, from_close, armed, closes, resumed, engine) = (me.clone(), from_close.clone(), armed.clone(), closes.clone(), resumed.clone(), engine.clone());
            Arc::new(move |st: &Arc<ExternalConnection>| {
                st.set_resumable_seconds_hook(Arc::new(|_| 60));
                let c = closes.clone();
                st.set_event_sink(Arc::new(move |_, ev| {
                    if ev.is_close() {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                }));
                let r = resumed.clone();
                st.set_resume_hook(Arc::new(move || {
                    r.fetch_add(1, Ordering::Relaxed);
                }));
                let (me, from_close, armed, engine) = (me.clone(), from_close.clone(), armed.clone(), engine.clone());
                st.set_session_hook(Arc::new(move |open| {
                    if !open && armed.swap(false, Ordering::AcqRel) {
                        let f = from_close.clone();
                        client::reconnect(me.get().unwrap(), addr, Some(Duration::from_secs(2)), &engine, move |r| *f.lock().unwrap() = Some(r.is_ok()));
                    }
                }));
                (
                    Box::new(EchoSink {
                        out: Default::default(),
                        echo: Default::default(),
                        state: None,
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
                )
            })
        };
        let hook = seat_on.clone();
        let state = connected(|done| {
            client::connect_with(
                addr,
                Some(Duration::from_secs(2)),
                &engine,
                move |st| {
                    let h = hook.clone();
                    st.set_seat_hook(Arc::new(move |to| Some(h(to))));
                    seat_on(st)
                },
                done,
            )
        })
        .unwrap();
        let _ = me.set(state.clone());
        state.identify(9);
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}");
        };
        wait("seated", &|| seated.lock().unwrap().len() == 1);
        seated.lock().unwrap()[0].disconnect(Event::THIS_CLOSE_ABRUPTLY); // the server drops the client: OnClose reconnects
        wait("the reconnect made in OnClose connected", &|| *from_close.lock().unwrap() == Some(true));
        wait("resumed", &|| resumed.load(Ordering::Relaxed) == 1);
        assert!(state.is_open() && state.session() as u32 == 9 && closes.load(Ordering::Relaxed) == 0, "the session went on");
        // the wait runs out while the peer is away
        wait("seated again", &|| seated.lock().unwrap().len() == 2);
        seated.lock().unwrap()[1].disconnect(Event::THIS_CLOSE_ABRUPTLY);
        wait("parked", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        state.resumable.store(1, Ordering::Release); // long past
        let again = connected(|done| client::reconnect(&state, addr, Some(Duration::from_secs(2)), &engine, done)).unwrap();
        assert!(Arc::ptr_eq(&again, &state));
        assert_eq!(closes.load(Ordering::Relaxed), 1, "the stale session heard its close");
        assert_eq!(state.session() as u32, 0, "a new session: no identity");
        assert_eq!(resumed.load(Ordering::Relaxed), 1, "not resumed");
        state.abort();
        server.shutdown();
    }

    /// A client whose connection is lost parks its identified session: no closing event;
    /// `reconnect` continues the session on a new socket - seated anew by its protocol layer,
    /// identified as before, resumed once connected - and what it sends gets through.
    #[test]
    fn a_client_parks_and_reconnects_its_session() {
        let engine = Engine::new("client-park", 1, crate::lib::network::engine::MIN_BUFFER);
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let (g, s) = (got.clone(), seated.clone());
        let factory: ConnectionFactory = Arc::new(move |state| {
            s.lock().unwrap().push(state.clone());
            (
                Box::new(EchoSink {
                    out: g.clone(),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("client-park", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let outbox = Arc::new(Mutex::new(Vec::<u8>::new()));
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (o, c, r) = (outbox.clone(), closes.clone(), resumed.clone());
        // the identity the connect handler gives the session, 0 - none
        let reidentify = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ri = reidentify.clone();
        // what the protocol layer does: seats itself on a connection, hears its events, resumes the
        // session
        let seat_on: Arc<dyn Fn(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + Sync> = Arc::new(move |st: &Arc<ExternalConnection>| {
            st.set_resumable_seconds_hook(Arc::new(|_| 60));
            let (c, ri, me) = (c.clone(), ri.clone(), Arc::downgrade(st));
            st.set_event_sink(Arc::new(move |_, ev| {
                if ev.is_close() {
                    c.fetch_add(1, Ordering::Relaxed);
                }
                let id = ri.load(Ordering::Relaxed);
                if ev == Event::THIS_CONNECT && id != 0 {
                    if let Some(m) = me.upgrade() {
                        m.identify(id);
                    }
                }
            }));
            let r = r.clone();
            st.set_resume_hook(Arc::new(move || {
                r.fetch_add(1, Ordering::Relaxed);
            }));
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(o.clone())) as Box<dyn BytesSrc>,
            )
        });
        let hook = seat_on.clone();
        let state = connected(|done| {
            client::connect_with(
                addr,
                Some(Duration::from_secs(2)),
                &engine,
                move |st| {
                    let h = hook.clone();
                    st.set_seat_hook(Arc::new(move |to| {
                        let h2 = h.clone();
                        let pair = h(to);
                        to.set_seat_hook(Arc::new(move |t| Some(h2(t))));
                        Some(pair)
                    }));
                    seat_on(st)
                },
                done,
            )
        })
        .unwrap();
        state.identify(9);
        // the deadline of the session's actors, set before the loss: it fires on the socket that
        // follows
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let f2 = fired.clone();
        state.set_deadline_hook(Arc::new(move |_| {
            f2.fetch_add(1, Ordering::Relaxed);
            0
        }));
        state.deadline(400);
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}");
        };
        wait("seated on the server", &|| seated.lock().unwrap().len() == 1);
        seated.lock().unwrap()[0].disconnect(Event::THIS_CLOSE_ABRUPTLY); // the server drops the client
        wait("the client parks its session", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        assert_eq!(closes.load(Ordering::Relaxed), 0, "a parked session hears no close");
        let fresh = connected(|done| client::reconnect(&state, addr, Some(Duration::from_secs(2)), &engine, done)).unwrap();
        assert!(Arc::ptr_eq(&fresh, &state), "the slot the session was parked on connects again ( Java )");
        assert_eq!(fresh.session() as u32, 9, "the new connection takes the identity of the session");
        wait("resumed once connected", &|| resumed.load(Ordering::Relaxed) == 1);
        assert_eq!(engine.clients.census()[pool::PARKED as usize], 0, "the session is not parked any more");
        wait("the deadline set before the loss fires on the new socket", &|| fired.load(Ordering::Relaxed) == 1);
        fresh.deadline(20);
        wait("a deadline after it fires too", &|| fired.load(Ordering::Relaxed) == 2);
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i * 7) as u8).collect();
        outbox.lock().unwrap().extend_from_slice(&payload);
        fresh.wake_transmit();
        wait("the bytes after the reconnect", &|| got.lock().unwrap().len() == payload.len());
        assert!(*got.lock().unwrap() == payload);
        assert_eq!(closes.load(Ordering::Relaxed), 0, "the session never heard a close");

        // the connect handler of the next socket gives the session another identity: it is not
        // resumed
        reidentify.store(10, Ordering::Relaxed);
        wait("seated on the server again", &|| seated.lock().unwrap().len() == 2);
        seated.lock().unwrap()[1].disconnect(Event::THIS_CLOSE_ABRUPTLY);
        wait("the client parks its session again", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        let other = connected(|done| client::reconnect(&fresh, addr, Some(Duration::from_secs(2)), &engine, done)).unwrap();
        assert_eq!(other.session() as u32, 10, "the identity the handler gave");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(resumed.load(Ordering::Relaxed), 1, "a session the handler re-identified is not resumed");
        other.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// A client of a multiplexed port parks its session; the reconnect opens with the byte naming
    /// the connection again, as the first socket did - the listening side seats the connection by
    /// it -, and what the session sends gets through.
    #[test]
    fn a_multiplexed_client_reconnects_named_by_its_byte() {
        let engine = Engine::new("mux-park", 1, crate::lib::network::engine::MIN_BUFFER);
        let named = Arc::new(Mutex::new(Vec::<u8>::new()));
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (n, s, g) = (named.clone(), seated.clone(), got.clone());
        let pick: crate::lib::network::mux::MuxPick = Arc::new(move |uid| {
            n.lock().unwrap().push(uid);
            let (s, g) = (s.clone(), g.clone());
            Some(Arc::new(move |state: &Arc<ExternalConnection>| {
                s.lock().unwrap().push(state.clone());
                (
                    Box::new(EchoSink {
                        out: g.clone(),
                        echo: Arc::new(Mutex::new(Vec::new())),
                        state: None,
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                )
            }) as ConnectionFactory)
        });
        let server = server::Builder::new("mux-park", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(crate::lib::network::mux::factory(pick, Event::REMOTE_CONNECT))
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let outbox = Arc::new(Mutex::new(Vec::<u8>::new()));
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seat = layer(outbox.clone(), closes.clone(), resumed.clone(), None);
        let state = connected(|done| client::connect_with(addr, Some(Duration::from_secs(2)), &engine, crate::lib::network::mux::dialing(5, seat), done)).unwrap();
        state.identify(9);
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}: named {:?}, seated {}", named.lock().unwrap(), seated.lock().unwrap().len());
        };
        wait("the first socket named by its byte", &|| *named.lock().unwrap() == [5] && seated.lock().unwrap().len() == 1);
        seated.lock().unwrap()[0].disconnect(Event::THIS_CLOSE_ABRUPTLY); // the server drops the client
        wait("the client parks its session", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        let fresh = connected(|done| client::reconnect(&state, addr, Some(Duration::from_secs(2)), &engine, done)).unwrap();
        wait("the reconnect named by its byte", &|| *named.lock().unwrap() == [5, 5] && seated.lock().unwrap().len() == 2);
        wait("resumed once connected", &|| resumed.load(Ordering::Relaxed) == 1);
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i * 7) as u8).collect();
        outbox.lock().unwrap().extend_from_slice(&payload);
        fresh.wake_transmit();
        wait("the bytes after the reconnect", &|| got.lock().unwrap().len() == payload.len());
        assert!(*got.lock().unwrap() == payload);
        assert_eq!(closes.load(Ordering::Relaxed), 0, "the session never heard a close");
        fresh.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// The connection of a multiplexed port stays with its slot when its socket is over: the next
    /// caller that names the same one gets it back - the factory is not asked -, a caller that
    /// names another one sends it away.
    #[test]
    fn a_multiplexed_slot_keeps_its_connection_for_the_next_caller() {
        /// What the protocol layer of a connection is: one object, seated again and again, as a
        /// generated one is.
        struct Layer {
            got: Arc<Mutex<Vec<u8>>>,
        }
        struct LayerSink(Arc<Layer>);
        impl BytesDst for LayerSink {
            fn on_bytes(&mut self, bytes: &[u8]) -> bool {
                self.0.got.lock().unwrap().extend_from_slice(bytes);
                true
            }
        }
        fn seat_layer(layer: &Arc<Layer>, state: &Arc<ExternalConnection>, seats: &Arc<std::sync::atomic::AtomicUsize>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) {
            seats.fetch_add(1, Ordering::Relaxed);
            let keep = Arc::downgrade(layer);
            state.set_session_keeper(Arc::new(move || keep.upgrade().map(|l| l as Arc<dyn std::any::Any + Send + Sync>)));
            let (again, s) = (Arc::downgrade(layer), seats.clone());
            state.set_seat_hook(Arc::new(move |to| again.upgrade().map(|l| seat_layer(&l, to, &s))));
            (Box::new(LayerSink(layer.clone())), Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))))
        }
        let engine = Engine::new("mux-incumbent", 1, crate::lib::network::engine::MIN_BUFFER);
        let made: Arc<Mutex<Vec<(u8, std::sync::Weak<Layer>)>>> = Default::default();
        let seats = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (m, s) = (made.clone(), seats.clone());
        let pick: crate::lib::network::mux::MuxPick = Arc::new(move |uid| {
            let (m, s) = (m.clone(), s.clone());
            Some(Arc::new(move |state: &Arc<ExternalConnection>| {
                let layer = Arc::new(Layer { got: Default::default() });
                m.lock().unwrap().push((uid, Arc::downgrade(&layer)));
                seat_layer(&layer, state, &s)
            }) as ConnectionFactory)
        });
        let server = server::Builder::new("mux-incumbent", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(crate::lib::network::mux::factory(pick, Event::REMOTE_CONNECT))
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let call = |uid: u8, what: &'static [u8]| {
            let st = connected(|done| {
                client::connect_with(
                    addr,
                    Some(Duration::from_secs(2)),
                    &engine,
                    crate::lib::network::mux::dialing(uid, move |_| {
                        (
                            Box::new(Touchy(Default::default())) as Box<dyn BytesDst>,
                            Box::new(EchoSource(Arc::new(Mutex::new(what.to_vec())))) as Box<dyn BytesSrc>,
                        )
                    }),
                    done,
                )
            })
            .unwrap();
            st.wake_transmit();
            st
        };
        let first = call(5, b"one");
        until("the first caller seated", &|| {
            made.lock().unwrap().len() == 1 && made.lock().unwrap()[0].1.upgrade().is_some_and(|l| l.got.lock().unwrap().as_slice() == b"one")
        });
        let layer5 = made.lock().unwrap()[0].1.clone();
        first.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        until("the slot free with its connection kept", &|| {
            server.pool.census()[pool::FREE as usize] == 1 && server.pool.connections_live() == 0
        });
        assert!(layer5.upgrade().is_some(), "the slot holds the connection it served");
        let second = call(5, b"two");
        until("the same connection seated again", &|| layer5.upgrade().is_some_and(|l| l.got.lock().unwrap().as_slice() == b"onetwo"));
        assert_eq!(made.lock().unwrap().len(), 1, "the factory was not asked");
        assert_eq!(seats.load(Ordering::Relaxed), 2);
        second.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        until("the slot free again", &|| server.pool.census()[pool::FREE as usize] == 1 && server.pool.connections_live() == 0);
        let third = call(6, b"six");
        until("another connection named: made by the factory", &|| made.lock().unwrap().len() == 2);
        until("the one kept is gone", &|| layer5.upgrade().is_none());
        let layer6 = made.lock().unwrap()[1].1.clone();
        until("the other connection got its bytes", &|| layer6.upgrade().is_some_and(|l| l.got.lock().unwrap().as_slice() == b"six"));
        third.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// Collects what it gets - and panics on a byte 0xFF: code of the application with a bug in it.
    struct Touchy(Arc<Mutex<Vec<u8>>>);
    impl BytesDst for Touchy {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            assert!(!bytes.contains(&0xFF), "a byte the sink cannot take");
            self.0.lock().unwrap().extend_from_slice(bytes);
            true
        }
    }

    fn until(what: &str, f: &dyn Fn() -> bool) {
        for _ in 0..1000 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("{what}");
    }

    /// The server on one loop: the panic of the sink of a connection goes to the handler of the
    /// server, the connection lives on, and the other connection on the same loop goes on.
    #[test]
    fn a_panic_in_a_connection_is_reported_and_the_loop_goes_on() {
        use std::io::Write;
        let engine = Engine::new("panic", 1, crate::lib::network::engine::MIN_BUFFER);
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let g = got.clone();
        let factory: ConnectionFactory = Arc::new(move |_| (Box::new(Touchy(g.clone())) as Box<dyn BytesDst>, Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>));
        let failures = Arc::new(Mutex::new(Vec::<String>::new()));
        let fl = failures.clone();
        let server = server::Builder::new("panic", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .on_failure(Arc::new(move |src, e| fl.lock().unwrap().push(format!("{src} | {e}"))))
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let mut a = std::net::TcpStream::connect(addr).unwrap();
        let mut b = std::net::TcpStream::connect(addr).unwrap();
        until("both connections live", &|| server.census().live == 2);
        a.write_all(&[1, 0xFF]).unwrap();
        until("the panic is reported", &|| failures.lock().unwrap().len() == 1);
        assert_eq!(server.census().live, 2, "the connection whose sink panicked lives on");
        b.write_all(b"after").unwrap();
        until("the other connection goes on", &|| got.lock().unwrap().ends_with(b"after"));
        let f = failures.lock().unwrap().clone();
        assert_eq!(f.len(), 1, "one failure, to the handler of the server: {f:?}");
        assert!(f[0].starts_with('[') && f[0].contains("panicked: a byte the sink cannot take"), "the connection is its source: {f:?}");
        server.shutdown();
    }

    /// Refuses everything: the stream broken for good, or not.
    struct Refuse(bool);
    impl BytesDst for Refuse {
        fn on_bytes(&mut self, _: &[u8]) -> bool {
            false
        }
        fn broken(&self) -> bool {
            self.0
        }
    }

    /// The sink refused the stream: an unknown pack id is a loss - the close of the peer's side,
    /// the session may park; a pack it could not parse broke the stream for good - our own close,
    /// the session does not park.
    #[test]
    fn a_stream_broken_for_good_is_our_close_a_refusal_a_loss() {
        use std::io::Write;
        for broken in [false, true] {
            let engine = Engine::new("refuse", 1, crate::lib::network::engine::MIN_BUFFER);
            let closed = Arc::new(Mutex::new(Vec::<Event>::new()));
            let c = closed.clone();
            let factory: ConnectionFactory = Arc::new(move |state| {
                let c = c.clone();
                state.set_event_sink(Arc::new(move |_, ev| {
                    if ev.is_close() {
                        c.lock().unwrap().push(ev)
                    }
                }));
                (Box::new(Refuse(broken)) as Box<dyn BytesDst>, Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>)
            });
            let server = server::Builder::new("refuse", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .engine(engine.clone())
                .on_connect(factory)
                .build()
                .unwrap();
            let mut a = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
            a.write_all(&[1, 2, 3]).unwrap();
            until("the connection closes", &|| !closed.lock().unwrap().is_empty());
            let want = if broken { Event::THIS_CLOSE_ABRUPTLY } else { Event::REMOTE_CLOSE_ABRUPTLY };
            assert_eq!(*closed.lock().unwrap(), [want], "broken {broken}");
            server.shutdown();
        }
    }

    /// A factory that panics refuses its connection only: the socket closes, the number and the
    /// admission go back, the next connection is taken in.
    #[test]
    fn a_panic_in_the_factory_refuses_only_that_connection() {
        use std::io::{Read, Write};
        let engine = Engine::new("panic-factory", 1, crate::lib::network::engine::MIN_BUFFER);
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (g, n) = (got.clone(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let factory: ConnectionFactory = Arc::new(move |_| {
            if n.fetch_add(1, Ordering::Relaxed) == 0 {
                panic!("the factory fails on the first connection");
            }
            (Box::new(Touchy(g.clone())) as Box<dyn BytesDst>, Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>)
        });
        let server = server::Builder::new("panic-factory", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let mut a = std::net::TcpStream::connect(addr).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut one = [0u8; 1];
        assert!(matches!(a.read(&mut one), Ok(0) | Err(_)), "the refused connection is closed");
        assert_eq!(server.census().live, 0, "its number went back");
        let mut b = std::net::TcpStream::connect(addr).unwrap();
        b.write_all(b"next").unwrap();
        until("the next connection is taken in", &|| got.lock().unwrap().as_slice() == b"next");
        server.shutdown();
    }

    /// The factory of a client that panics: the connect fails with the panic - whoever waits for it hears it, the handler
    /// of the engine too - and the next connect of the loop goes through.
    #[test]
    fn a_panic_in_the_factory_of_a_client_fails_its_connect() {
        let engine = Engine::new("panic-client-factory", 1, crate::lib::network::engine::MIN_BUFFER);
        let server = server::Builder::new("panic-client-factory", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(Arc::new(|_| {
                (
                    Box::new(EchoSink {
                        out: Default::default(),
                        echo: Default::default(),
                        state: None,
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
                )
            }))
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dial = || {
            let n = n.clone();
            connected(|done| {
                client::connect_with(
                    addr,
                    Some(Duration::from_secs(2)),
                    &engine,
                    move |_| {
                        if n.fetch_add(1, Ordering::Relaxed) == 0 {
                            panic!("the factory of the client fails its first connect");
                        }
                        (
                            Box::new(EchoSink {
                                out: Default::default(),
                                echo: Default::default(),
                                state: None,
                            }) as Box<dyn BytesDst>,
                            Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>,
                        )
                    },
                    done,
                )
            })
        };
        let Err(e) = dial() else { panic!("the factory panicked: the connect is to fail") };
        assert!(e.to_string().contains("fails its first connect"), "{e}");
        let next = dial().expect("the next connect goes through");
        next.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// A close handler that panics: the teardown goes on past it - the number goes back - and the
    /// loop serves the next connection.
    #[test]
    fn a_panic_in_the_close_handler_still_ends_the_connection() {
        use std::io::Write;
        let engine = Engine::new("panic-close", 1, crate::lib::network::engine::MIN_BUFFER);
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let g = got.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            state.set_event_sink(Arc::new(|_, ev| {
                if ev.is_close() {
                    panic!("a close handler that panics");
                }
            }));
            (Box::new(Touchy(g.clone())) as Box<dyn BytesDst>, Box::new(EchoSource(Default::default())) as Box<dyn BytesSrc>)
        });
        let server = server::Builder::new("panic-close", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let a = std::net::TcpStream::connect(addr).unwrap();
        until("the connection lives", &|| server.census().live == 1);
        drop(a); // the peer closes: the close handler panics
        until("the teardown went on past the panic", &|| server.census().live == 0);
        let mut b = std::net::TcpStream::connect(addr).unwrap();
        b.write_all(b"next").unwrap();
        until("the loop serves the next connection", &|| got.lock().unwrap().as_slice() == b"next");
        server.shutdown();
    }

    /// What a protocol layer does for a client: seats itself on a connection - again on another
    /// one, through the seat hook -, hears its close events into `closes`, counts its resumes into
    /// `resumed`, sends what `outbox` holds; declares the Noise `noise` every time it is seated.
    pub(crate) fn layer(
        outbox: Arc<Mutex<Vec<u8>>>,
        closes: Arc<std::sync::atomic::AtomicUsize>,
        resumed: Arc<std::sync::atomic::AtomicUsize>,
        noise: Option<Chain>,
    ) -> impl Fn(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + Sync + Clone + 'static {
        fn seat(
            st: &Arc<ExternalConnection>,
            o: &Arc<Mutex<Vec<u8>>>,
            c: &Arc<std::sync::atomic::AtomicUsize>,
            r: &Arc<std::sync::atomic::AtomicUsize>,
            n: &Option<Chain>,
        ) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) {
            if let Some(chain) = n {
                st.secure(Some(chain.clone()));
            }
            st.set_resumable_seconds_hook(Arc::new(|_| 60));
            let cc = c.clone();
            st.set_event_sink(Arc::new(move |_, ev| {
                if ev.is_close() {
                    cc.fetch_add(1, Ordering::Relaxed);
                }
            }));
            let rr = r.clone();
            st.set_resume_hook(Arc::new(move || {
                rr.fetch_add(1, Ordering::Relaxed);
            }));
            let (o2, c2, r2, n2) = (o.clone(), c.clone(), r.clone(), n.clone());
            st.set_seat_hook(Arc::new(move |to| Some(seat(to, &o2, &c2, &r2, &n2))));
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(o.clone())) as Box<dyn BytesSrc>,
            )
        }
        move |st: &Arc<ExternalConnection>| seat(st, &outbox, &closes, &resumed, &noise)
    }

    /// A parked session ends when its wait runs out, to the second - not at the next tick of its
    /// loop: the loop wakes for the nearest deadline of its parked sessions.
    #[test]
    fn a_parked_session_ends_at_its_deadline_not_at_the_tick() {
        let engine = Engine::new("reap-nearest", 1, crate::lib::network::engine::MIN_BUFFER);
        engine.set_tick(600_000); // a tick that never comes in this test
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let s = seated.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            s.lock().unwrap().push(state.clone());
            (
                Box::new(Touchy(Default::default())) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("reap-nearest", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = closes.clone();
        let state = connected(|done| {
            client::connect_with(
                addr,
                Some(Duration::from_secs(2)),
                &engine,
                move |st| {
                    st.set_resumable_seconds_hook(Arc::new(|_| 1));
                    let c = c.clone();
                    st.set_event_sink(Arc::new(move |_, ev| {
                        if ev.is_close() {
                            c.fetch_add(1, Ordering::Relaxed);
                        }
                    }));
                    (
                        Box::new(Touchy(Default::default())) as Box<dyn BytesDst>,
                        Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                    )
                },
                done,
            )
        })
        .unwrap();
        state.identify(9);
        until("the server seated it", &|| seated.lock().unwrap().len() == 1);
        // timed from the loss: the parking lasts its 1 s only ( whole seconds, as C#: up to 1 s ) - a test thread that slept
        // through it finds the session reaped already
        let lost_at = std::time::Instant::now();
        seated.lock().unwrap()[0].disconnect(Event::THIS_CLOSE_ABRUPTLY);
        until("the client parks its session", &|| {
            engine.clients.census()[pool::PARKED as usize] == 1 || closes.load(Ordering::Relaxed) == 1
        });
        let heard = closes.load(Ordering::Relaxed); // first: a reap between the two reads shows it no longer parked
        if engine.clients.census()[pool::PARKED as usize] == 1 {
            assert_eq!(heard, 0, "a parked session hears no close");
        }
        for _ in 0..2000 {
            if closes.load(Ordering::Relaxed) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let waited = lost_at.elapsed();
        assert_eq!(closes.load(Ordering::Relaxed), 1, "the wait of 1 s ran out and its close is heard");
        assert!(waited <= Duration::from_secs(10), "reaped at its deadline of 1 s, not at the tick of 600 s: {waited:?}");
        assert_eq!(engine.clients.census()[pool::PARKED as usize], 0);
        server.shutdown();
    }

    /// A port nobody listens on.
    pub(crate) fn dead_addr() -> SocketAddr {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap()
    }

    /// An encrypted client parks its session; a reconnect to where nobody listens fails and parks
    /// it again - the session hears no close -, the next one goes on in the Noise its protocol layer
    /// declares as it is seated again - a handshake anew -, and what the session sends gets through.
    #[test]
    fn an_encrypted_client_reconnects_after_a_failed_attempt() {
        let engine = Engine::new("enc-park", 1, crate::lib::network::engine::MIN_BUFFER);
        let (secret, public) = crate::lib::x25519::keypair();
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let (g, s) = (got.clone(), seated.clone());
        let factory: ConnectionFactory = Arc::new(move |state| {
            s.lock().unwrap().push(state.clone());
            state.secure(noise(crate::lib::noise::PATTERN_NK, &secret, &[0u8; 32]));
            (
                Box::new(EchoSink {
                    out: g.clone(),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("enc-park", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let outbox = Arc::new(Mutex::new(Vec::<u8>::new()));
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seat = layer(outbox.clone(), closes.clone(), resumed.clone(), noise(crate::lib::noise::PATTERN_NK, &[0u8; 32], &public));
        let state = connected(|done| client::connect_with(addr, Some(Duration::from_secs(2)), &engine, seat, done)).unwrap();
        state.identify(9);
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}");
        };
        wait("seated on the server", &|| seated.lock().unwrap().len() == 1);
        wait("the handshake of the first connection", &|| state.noise().is_some_and(|n| n.ready()));
        let first = state.noise().unwrap();
        seated.lock().unwrap()[0].disconnect(Event::THIS_CLOSE_ABRUPTLY);
        wait("the client parks its session", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        let failed = connected(|done| client::reconnect(&state, dead_addr(), Some(Duration::from_secs(2)), &engine, done));
        assert!(failed.is_err(), "nobody listens there");
        wait("parked again", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        assert_eq!(closes.load(Ordering::Relaxed), 0, "a failed reconnect is not the end of the session");
        let fresh = connected(|done| client::reconnect(&state, addr, Some(Duration::from_secs(2)), &engine, done)).unwrap();
        assert_eq!(fresh.session() as u32, 9);
        wait("resumed once connected", &|| resumed.load(Ordering::Relaxed) == 1);
        wait("the handshake of the new connection", &|| fresh.noise().is_some_and(|n| n.ready()));
        assert!(
            !Arc::ptr_eq(&fresh.noise().unwrap().0, &first.0),
            "a Noise stream of its own: the protocol layer seated again declared it"
        );
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i * 11) as u8).collect();
        outbox.lock().unwrap().extend_from_slice(&payload);
        fresh.wake_transmit();
        wait("the bytes after the reconnect, decrypted on the server", &|| got.lock().unwrap().len() == payload.len());
        assert!(*got.lock().unwrap() == payload);
        assert_eq!(closes.load(Ordering::Relaxed), 0);
        fresh.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// A WebSocket session opens with its upgrade, not with the socket: a peer that never upgrades
    /// - a port scanner, a refused request - opens nothing and closes nothing; one that does hears
    /// `OnOpen` once, after the connect event.
    #[test]
    fn a_websocket_session_opens_with_its_upgrade() {
        use std::io::Write;
        let engine = Engine::new("ws-open", 1, super::web_socket::MIN_BUFFER);
        let heard: Arc<Mutex<Vec<String>>> = Default::default();
        let h = heard.clone();
        let factory: ConnectionFactory = Arc::new(move |state| {
            let (a, b) = (h.clone(), h.clone());
            state.set_event_sink(Arc::new(move |_, e| a.lock().unwrap().push(format!("{e:?}"))));
            state.set_session_hook(Arc::new(move |open| b.lock().unwrap().push(if open { "open" } else { "close" }.into())));
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("ws-open", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .web_socket()
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}");
        };
        let opens = || heard.lock().unwrap().iter().filter(|x| *x == "open" || *x == "close").count();
        // a scanner: connects and goes
        drop(std::net::TcpStream::connect(addr).unwrap());
        // a request with no upgrade in it
        let mut bad = std::net::TcpStream::connect(addr).unwrap();
        bad.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        wait("both are gone", &|| server.pool.connections_live() == 0 && 4 <= heard.lock().unwrap().len());
        assert_eq!(opens(), 0, "no session opened or closed: {:?}", heard.lock().unwrap());
        heard.lock().unwrap().clear();
        // a WebSocket
        let (tx, rx) = std::sync::mpsc::channel();
        web_socket::client::connect_with(
            &format!("ws://{addr}"),
            Some(Duration::from_secs(2)),
            &engine,
            true,
            |_| {
                (
                    Box::new(EchoSink {
                        out: Arc::new(Mutex::new(Vec::new())),
                        echo: Arc::new(Mutex::new(Vec::new())),
                        state: None,
                    }) as Box<dyn BytesDst>,
                    Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                )
            },
            move |r| {
                let _ = tx.send(r.map(|c| c.state));
            },
        );
        let client = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        wait("the server side opened", &|| opens() == 1);
        let got = heard.lock().unwrap().clone();
        assert_eq!(got.last().map(String::as_str), Some("open"), "OnOpen after the events of the socket and of the upgrade: {got:?}");
        assert_eq!(got.len(), 3, "{got:?}");
        client.abort();
        wait("the server side closed", &|| opens() == 2);
        server.shutdown();
    }

    /// A WebSocket client parks its session; a reconnect to where nobody listens fails and parks it
    /// again, the next one upgrades anew and the session goes on: identified as before, resumed
    /// once the 101 came, what it sends gets through.
    #[test]
    fn a_websocket_client_parks_and_reconnects() {
        let engine = Engine::new("ws-park", 1, super::web_socket::MIN_BUFFER); // the least buffer of a WebSocket: the upgrade request and the 101 whole
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let seated: Arc<Mutex<Vec<Arc<ExternalConnection>>>> = Default::default();
        let (g, s) = (got.clone(), seated.clone());
        let factory: ConnectionFactory = Arc::new(move |state| {
            s.lock().unwrap().push(state.clone());
            (
                Box::new(EchoSink {
                    out: g.clone(),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: None,
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("ws-park", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .web_socket()
            .engine(engine.clone())
            .on_connect(factory)
            .build()
            .unwrap();
        let url = format!("ws://{}", server.local_addr().unwrap());
        let outbox = Arc::new(Mutex::new(Vec::<u8>::new()));
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seat = layer(outbox.clone(), closes.clone(), resumed.clone(), None);
        let ws = |start: &dyn Fn(Box<dyn FnOnce(std::io::Result<super::web_socket::WebSocket>) + Send>)| {
            let (tx, rx) = std::sync::mpsc::channel();
            start(Box::new(move |r| {
                let _ = tx.send(r.map(|c| c.state));
            }));
            rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| Err(std::io::Error::other("no report of the connect")))
        };
        let state = ws(&|done| web_socket::client::connect_with(&url, Some(Duration::from_secs(2)), &engine, true, seat.clone(), done)).unwrap();
        state.identify(9);
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if f() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("{what}");
        };
        wait("seated on the server", &|| seated.lock().unwrap().len() == 1);
        seated.lock().unwrap()[0].disconnect(Event::THIS_CLOSE_ABRUPTLY);
        wait("the client parks its session", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        let dead = format!("ws://{}", dead_addr());
        assert!(
            ws(&|done| web_socket::client::reconnect(&state, &dead, Some(Duration::from_secs(2)), &engine, true, done)).is_err(),
            "nobody listens there"
        );
        wait("parked again", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        // a server that refuses the upgrade: a 400 instead of the 101
        let refusing = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let no = format!("ws://{}", refusing.local_addr().unwrap());
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = refusing.accept() {
                let mut req = Vec::new();
                let mut b = [0u8; 1];
                while !req.ends_with(
                    b"

",
                ) && s.read(&mut b).map_or(false, |n| n == 1)
                {
                    req.push(b[0]);
                }
                let _ = s.write_all(
                    b"HTTP/1.1 400 Bad Request
Content-Length: 0

",
                );
            }
        });
        assert!(
            ws(&|done| web_socket::client::reconnect(&state, &no, Some(Duration::from_secs(2)), &engine, true, done)).is_err(),
            "the upgrade is refused"
        );
        wait("parked again after the refused upgrade", &|| engine.clients.census()[pool::PARKED as usize] == 1);
        assert_eq!(closes.load(Ordering::Relaxed), 0, "a refused upgrade is not the end of the session");
        assert_eq!(closes.load(Ordering::Relaxed), 0, "a failed reconnect is not the end of the session");
        let fresh = ws(&|done| web_socket::client::reconnect(&state, &url, Some(Duration::from_secs(2)), &engine, true, done)).unwrap();
        assert_eq!(fresh.session() as u32, 9);
        wait("resumed once the 101 came", &|| resumed.load(Ordering::Relaxed) == 1);
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i * 5) as u8).collect();
        outbox.lock().unwrap().extend_from_slice(&payload);
        fresh.wake_transmit();
        wait("the bytes after the reconnect", &|| got.lock().unwrap().len() == payload.len());
        assert!(*got.lock().unwrap() == payload);
        assert_eq!(closes.load(Ordering::Relaxed), 0);
        fresh.disconnect(Event::THIS_CLOSE_ABRUPTLY);
        server.shutdown();
    }

    /// A graceful shutdown: no more listening, the sessions close the polite way and end on their
    /// own within the grace; a shutdown at once tears down whoever is there.
    #[test]
    fn shutdown_gives_the_sessions_their_grace() {
        let factory: ConnectionFactory = Arc::new(move |state| {
            (
                Box::new(EchoSink {
                    out: Arc::new(Mutex::new(Vec::new())),
                    echo: Arc::new(Mutex::new(Vec::new())),
                    state: Some(state.clone()),
                }) as Box<dyn BytesDst>,
                Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
            )
        });
        let server = server::Builder::new("grace", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).on_connect(factory).build().unwrap();
        let addr = server.local_addr().unwrap();
        let client = || {
            connected(|done| {
                client::connect(
                    addr,
                    Some(Duration::from_secs(2)),
                    move |_| {
                        (
                            Box::new(EchoSink {
                                out: Arc::new(Mutex::new(Vec::new())),
                                echo: Arc::new(Mutex::new(Vec::new())),
                                state: None,
                            }) as Box<dyn BytesDst>,
                            Box::new(EchoSource(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesSrc>,
                        )
                    },
                    done,
                )
            })
        };
        let a = client().unwrap();
        let b = client().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(server.pool.connections_live(), 2);
        assert_eq!(a.remote(), Some(addr), "the peer of a client is the server");
        assert_eq!(a.local().map(|l| l.ip()), Some(Ipv4Addr::LOCALHOST.into()));
        assert!(server.shutdown_graceful(Duration::from_secs(2)), "both peers answer the FIN: the sessions end on their own");
        assert!(!crate::lib::network::TCP::external_connection::ExternalConnection::is_open(&*a));
        assert!(!crate::lib::network::TCP::external_connection::ExternalConnection::is_open(&*b));
        assert!(client().is_err(), "nothing listens any more");
    }
}
