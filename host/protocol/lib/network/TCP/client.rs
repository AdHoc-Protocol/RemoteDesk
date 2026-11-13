// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! TCP connection builders and a reusable single-connection client host.
//!
//! [`Client`] retains one slot across sessions. Standalone [`connect`] helpers allocate a client
//! slot from the engine. Connection results arrive on the connection's I/O loop.

use super::*;

/// Connects to `addr` on the engine of the process, with the protocol layer `seat` gives seated on
/// the connection's state first, in a slot of the clients of the engine. `on_connected` hears on
/// the thread of the connection's loop: the state once the session started, or what went wrong -
/// `TimedOut` when `timeout` ran out first.
pub fn connect(
    addr: SocketAddr,
    timeout: Option<Duration>,
    seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
    on_connected: impl FnOnce(std::io::Result<Arc<ExternalConnection>>) + Send + 'static,
) {
    connect_with(addr, timeout, &Engine::shared(), seat, on_connected)
}

/// As [`connect`](connect), on `engine`: its threads, the size of its buffers.
pub fn connect_with(
    addr: SocketAddr,
    timeout: Option<Duration>,
    engine: &Arc<Engine>,
    seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
    on_connected: impl FnOnce(std::io::Result<Arc<ExternalConnection>>) + Send + 'static,
) {
    dial(engine, addr, timeout, None, SocketOptions::default(), SlotOf::Clients, false, SeatBy::Fn(Box::new(move |s| Some(seat(s)))), Report::of(Box::new(on_connected)), None);
}

/// As [`connect_with`](connect_with), with `opts` for the socket.
pub fn connect_with_options(
    addr: SocketAddr,
    timeout: Option<Duration>,
    engine: &Arc<Engine>,
    opts: SocketOptions,
    seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
    on_connected: impl FnOnce(std::io::Result<Arc<ExternalConnection>>) + Send + 'static,
) {
    dial(engine, addr, timeout, None, opts, SlotOf::Clients, false, SeatBy::Fn(Box::new(move |s| Some(seat(s)))), Report::of(Box::new(on_connected)), None);
}

/// Continues the parked session of the client connection `state` on a new socket to `addr`: its
/// protocol layer, its journal and its receive position go on on the new connection, which takes
/// the identity of the session; once connected the peer is asked to go on from where the receiver
/// of the session is. A failed attempt parks the session again, for what is left of its wait.
/// `on_connected` hears as for [`connect`](connect): the state of the new connection.
pub fn reconnect(
    state: &Arc<ExternalConnection>,
    addr: SocketAddr,
    timeout: Option<Duration>,
    engine: &Arc<Engine>,
    on_connected: impl FnOnce(std::io::Result<Arc<ExternalConnection>>) + Send + 'static,
) {
    redial(state, engine, addr, timeout, None, SocketOptions::default(), Report::of(Box::new(on_connected)));
}


/// A client dials again on its own while its session is parked and holds what the peer has not
/// confirmed (the journal of guaranteed packs): only the side that made the connection can make it
/// again, and the peer would wait for those bytes in vain. The waits are the connection backoff of
/// gRPC (github.com/grpc/grpc/blob/master/doc/connection-backoff.md): 1 s first, each next 1.6
/// times longer, at most 120 s, each spread by ±20 % - clients that lost one server together do not
/// come back to it together; a connect may take 20 s at least. It ends with the wait of the parked
/// session, or once a connect takes it on.
///
/// The waits start anew only after a socket that served: its peer said something once the session
/// started. A door that takes the socket and shuts it - a balancer with no server behind, a server
/// that drops what it cannot take - proves nothing, and the waits go on growing instead of a dial a
/// second for as long as the session waits. A WebSocket client resolves the host of its URL anew
/// before every dial of its own - the server may have moved - and goes on to the next address of
/// the name after a dial that did not serve.
pub struct Redial;

impl Redial {
    pub const FIRST_MS: i64 = 1_000;
    pub const MAX_MS: i64 = 120_000;
    pub const CONNECT_MS: i64 = 20_000;
    pub const MULTIPLIER: f64 = 1.6;
    pub const JITTER: f64 = 0.2;

    /// How long the link may stay almost still while the session needs it
    /// ([`stalled`](Self::stalled)).
    pub const STALL_MS: i64 = 10_000;

    /// Almost still: fewer bytes than this, both ways together, in [`STALL_MS`](Self::STALL_MS).
    pub const STALL_BYTES: u64 = 1 << 10;

    /// The wait before the dial `attempt` (0 - the first after a socket that served), in
    /// milliseconds.
    pub fn wait(attempt: u32) -> i64 {
        let mut ms = Self::FIRST_MS as f64;
        let mut i = 0;
        while i < attempt && ms < Self::MAX_MS as f64 {
            ms *= Self::MULTIPLIER;
            i += 1;
        }
        // the spread needs no generator of its own: the low bits of the clock, mixed
        let x = (crate::lib::network::engine::clock() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let r = (x >> 11) as f64 / (1u64 << 53) as f64;
        (ms.min(Self::MAX_MS as f64) * (1.0 + Self::JITTER * (2.0 * r - 1.0))) as i64
    }

    /// The time a dial may take: what the application gave its connect, 20 s at least.
    pub fn connect(timeout: Option<Duration>) -> Option<Duration> {
        timeout.map(|t| t.max(Duration::from_millis(Self::CONNECT_MS as u64)))
    }

    /// The speed of the link fell almost to nothing: a new socket is likely to do better. A path
    /// that degraded, a congestion window that collapsed, a NAT or a tunnel that stalls without
    /// closing - none of them is cured on the socket that has it, a fresh connection often is. So a
    /// client watches the speed of its session's socket on the tick of its loop, and once the link carried
    /// fewer than [`STALL_BYTES`](Self::STALL_BYTES) both ways in [`STALL_MS`](Self::STALL_MS)
    /// WHILE THE SESSION NEEDED IT, takes the socket as lost: the session parks and the client dials
    /// again ([`Redial`]).
    ///
    /// The speed is judged only while there is something the link must carry - a session that has
    /// little to say is not a slow link:
    /// * the socket does not take what this side writes (a write is parked: `writing`);
    /// * the peer owes a confirmation: a step's worth of the journal
    ///   ([`Control::step`](crate::lib::adhoc::connection::control::Control::step)) has been sent
    ///   since the last one. Fewer unconfirmed bytes say nothing - the peer confirms once a step, so
    ///   bytes it has long received stay unconfirmed until the step fills;
    /// * a guaranteed pack is half received: the peer is sending its rest.
    ///
    /// Otherwise, or as soon as the link moved [`STALL_BYTES`](Self::STALL_BYTES), the watch starts
    /// again. What moves counts both ways: the bytes the socket `carried`, and what the peer
    /// confirmed - a write hands the system megabytes at once, they leave its buffer at the speed of
    /// the link with no write completing meanwhile, and only the confirmations of what got through
    /// come back. A slow link that moves lives, however slow. `watch` is the client's: the moved
    /// position and the start of the watch (ms). `true` - the link is almost still while the session
    /// needs it: take the socket as lost.
    pub fn stalled(progress: super::external_connection::Progress, carried: u64, writing: bool, now_ms: i64, watch: &mut (u64, i64)) -> bool {
        let needed = writing // the socket does not take what this side writes
            || progress.unconfirmed.owed // the peer owes a confirmation
            || progress.guarding; // the rest of a guaranteed pack is on its way
        let moved = carried.wrapping_add(progress.confirmed as u64); // a position: only its change counts, and it wraps as it may
        if !needed || Self::STALL_BYTES <= moved.wrapping_sub(watch.0) {
            *watch = (moved, now_ms); // the session does not need the link, or the link moves: the watch starts again
            return false;
        }
        Self::STALL_MS <= now_ms - watch.1
    }
}

/// A client of one connection: a host of its own, its one slot on one loop for every session - a
/// connect after a loss takes the same slot again, and a parked session goes on in it. One connect
/// at a time: a connect while one is under way hears with it.
pub struct Client {
    core: Arc<Core>,
}

impl Client {
    /// A client on an engine of its own, of one loop: one socket is all it serves. `seat` gives the
    /// protocol layer, seated on the connection before every connect.
    pub fn new(name: &str, buffer_size: usize, seat: ConnectionFactory) -> Self {
        Self::with_engine(name, &Engine::new(name, 1, buffer_size), seat)
    }

    /// As [`new`](Self::new), on `engine`: the threads the connection runs on, possibly shared with
    /// other hosts.
    pub fn with_engine(name: &str, engine: &Arc<Engine>, seat: ConnectionFactory) -> Self {
        Self { core: Core::new(name, engine, seat) }
    }

    /// How long a read after our own FIN waits for the peer's, in milliseconds.
    pub fn linger_timeout(&self) -> i64 {
        self.core.pool.linger_timeout()
    }

    /// Sets it for the sessions that start from now on.
    pub fn set_linger_timeout(&self, ms: i64) {
        self.core.pool.set_linger_timeout(ms);
    }

    /// Stops at once: the connection is torn down where it stands, and the engine's threads end -
    /// unless it is the engine of the process.
    pub fn shutdown(&self) {
        self.core.give_up();
        super::stop(&self.core.pool, &self.core.engine, Duration::ZERO);
    }

    /// The dials of the client's own since a socket last served - its peer said something: the step
    /// of the backoff ([`Redial`]).
    pub fn redials(&self) -> u32 {
        self.core.redials()
    }

    /// Stops, giving the session `grace` to finish what it was saying: asked to close the polite
    /// way, torn down when `grace` runs out, then the engine's threads end. `true` - the session
    /// ended on its own.
    pub fn shutdown_graceful(&self, grace: Duration) -> bool {
        self.core.give_up();
        super::stop(&self.core.pool, &self.core.engine, grace)
    }

    /// Its failures - of its connection, of the connects - go to `on_failure`; the engine's handler
    /// when none. Before the first connect.
    pub fn on_failure(self, on_failure: crate::lib::network::failure::OnFailure) -> Self {
        Self { core: self.core.with_failure(on_failure), ..self }
    }

    /// The engine its connection runs on.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.core.engine
    }

    /// The options of the socket of the next connect.
    pub fn set_options(&self, opts: SocketOptions) {
        *self.core.opts.lock() = opts;
    }

    /// Connects to `server`, `timeout` - 5 s when none. `on_connected` hears on the thread of the
    /// connection's loop: the connection once the session started, or what went wrong - `TimedOut`
    /// when the time ran out first. A parked session goes on on the new socket; one whose wait ran
    /// out ends and a new one starts. A connect while one is under way hears with that one. Called
    /// from a close handler, it waits for the teardown under way. `Err`: connected already -
    /// disconnect first.
    pub fn connect(
        &self,
        server: SocketAddr,
        timeout: Option<Duration>,
        on_connected: impl FnOnce(std::io::Result<Arc<ExternalConnection>>) + Send + 'static,
    ) -> std::io::Result<()> {
        self.core.connect(server, timeout, None, Box::new(on_connected))
    }

    /// As [`connect`](Self::connect), through the chain chosen - TLS: `on_connected` hears when its
    /// TLS is through. The chain holds for the connects after it, a parked session going on
    /// included, until another is given. The Noise inside it is of the protocol layer (`[Noise]` of
    /// its description).
    pub fn connect_chain(
        &self,
        chain: crate::lib::chain::Chain,
        server: SocketAddr,
        timeout: Option<Duration>,
        on_connected: impl FnOnce(std::io::Result<Arc<ExternalConnection>>) + Send + 'static,
    ) -> std::io::Result<()> {
        self.core.set_chain(chain);
        self.connect(server, timeout, on_connected)
    }

    /// TLS: `name` - what the server's certificate has to carry, `alpn` - the protocol to ask for.
    pub fn connect_tls(
        &self,
        server: SocketAddr,
        name: &str,
        alpn: Option<&str>,
        timeout: Option<Duration>,
        on_connected: impl FnOnce(std::io::Result<Arc<ExternalConnection>>) + Send + 'static,
    ) -> std::io::Result<()> {
        self.connect_chain(crate::lib::chain::Chain::default().tls(name, alpn), server, timeout, on_connected)
    }

    /// A connect is under way.
    #[inline]
    pub fn is_connecting(&self) -> bool {
        self.core.is_connecting()
    }

    /// A session runs on the connection.
    #[inline]
    pub fn is_connected(&self) -> bool {
        self.core.is_connected()
    }

    /// The connection of the client: there once a connect has taken its slot.
    pub fn connection(&self) -> Option<Arc<ExternalConnection>> {
        self.core.connection()
    }

    /// Closes the connection abruptly: what is queued is dropped, the socket closed.
    pub fn disconnect(&self) {
        self.core.disconnect(false)
    }

    /// Closes the connection gracefully: what is queued goes out, then the FIN; the connection ends
    /// with the peer's.
    pub fn disconnect_gracefully(&self) {
        self.core.disconnect(true)
    }
}

impl std::fmt::Display for Client {
    /// Formats the client name and the server from the most recent connection attempt.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&*self.core, f)
    }
}

/// What a WebSocket connect sends: the `Host` header, the request target, the masking; the host
/// and port the client resolves anew before every dial of its own - `None`: the URL names an
/// address.
pub(crate) struct Upgrade {
    pub(crate) header: String,
    pub(crate) target: String,
    pub(crate) masking: bool,
    pub(crate) name: Option<(String, u16)>,
}

/// The client of one connection, its TCP and its WebSocket form alike.
pub(crate) struct Core {
    name: String,
    pub(crate) engine: Arc<Engine>,
    /// The client's own host: the one slot of the connection, the session parked there.
    pub(crate) pool: Arc<Pool>,
    /// The loop of the connection: every session of the client runs there.
    home: u8,
    seat: ConnectionFactory,
    /// The chain of the next connect, when one was chosen.
    chain: parking_lot::Mutex<Option<Arc<crate::lib::chain::Chain>>>,
    pub(crate) opts: parking_lot::Mutex<SocketOptions>,
    /// The slot, once a connect took it.
    slot: parking_lot::Mutex<Option<Arc<ExternalConnection>>>,
    /// A connect is under way, and who waits for it: the list keeps its room from connect to
    /// connect.
    waiting: parking_lot::Mutex<(bool, Vec<OnConnected>)>,
    /// Where the connect goes: the server, the time it may take, the upgrade of a WebSocket.
    target: parking_lot::Mutex<Option<(SocketAddr, Option<Duration>, Option<Arc<Upgrade>>)>>,
    /// The dials of the client's own since a socket last served ([`Redial`]): the step of the
    /// backoff.
    redials: std::sync::atomic::AtomicU32,
    /// Which address of the name of a WebSocket's URL the next dial of the client's own goes to: one
    /// on after each dial that did not serve.
    turn: std::sync::atomic::AtomicU32,
    /// The watch of the speed of the link while the session needs it ([`Redial::stalled`]): what the
    /// socket had carried plus what the peer had confirmed, and when (ms) the watch started.
    watch: parking_lot::Mutex<(u64, i64)>,
    /// The application closed the connection, or the client stopped: no dial of the client's own.
    given_up: AtomicBool,
}

impl Core {
    pub(crate) fn new(name: &str, engine: &Arc<Engine>, seat: ConnectionFactory) -> Arc<Self> {
        Self::make(name, engine, seat, engine.on_failure())
    }

    /// The same client with its failures going to `on_failure`: before its first connect.
    pub(crate) fn with_failure(&self, on_failure: crate::lib::network::failure::OnFailure) -> Arc<Self> {
        let c = Self::make(&self.name, &self.engine, self.seat.clone(), on_failure);
        *c.opts.lock() = self.opts.lock().clone();
        c
    }

    fn make(name: &str, engine: &Arc<Engine>, seat: ConnectionFactory, on_failure: crate::lib::network::failure::OnFailure) -> Arc<Self> {
        let pool = Pool::new(name, Some(engine.loops.clone()), Some(on_failure));
        engine.watch_parked(&pool); // its parked session ends on the tick of its loop when its wait runs out
        Arc::new(Self {
            name: name.to_string(),
            engine: engine.clone(),
            pool,
            home: engine.pick().number,
            seat,
            chain: parking_lot::Mutex::new(None),
            opts: parking_lot::Mutex::new(SocketOptions::default()),
            slot: parking_lot::Mutex::new(None),
            waiting: parking_lot::Mutex::new((false, Vec::new())),
            target: parking_lot::Mutex::new(None),
            redials: std::sync::atomic::AtomicU32::new(0),
            turn: std::sync::atomic::AtomicU32::new(0),
            watch: parking_lot::Mutex::new((u64::MAX, 0)),
            given_up: AtomicBool::new(false),
        })
    }

    /// The dials of the client's own since a socket last served.
    pub(crate) fn redials(&self) -> u32 {
        self.redials.load(Ordering::Relaxed)
    }

    /// No dial of the client's own any more: the client stops, and its slot lets it go. The slot
    /// keeps the client alive while it is the client's ( `owner` ) - a client whose handle is
    /// dropped goes on - and the client holds its slot: stopped, the two would keep each other,
    /// its engine and its pool for good.
    pub(crate) fn give_up(&self) {
        self.given_up.store(true, Ordering::Release);
        let s = self.slot.lock().take(); // out of the lock first: the owner may be the last hold of this core
        if let Some(s) = s {
            let owner = s.owner.lock().take();
            drop(owner);
        }
    }

    /// The watch of the speed of the link of the client's session on the socket `s`, on the tick of
    /// its loop ( [`Redial::stalled`] ): what the session has on its way is the loop's, as its
    /// receive is - read there, with no lock. `true` - the link is almost still while the session
    /// needs it: the socket is taken as lost, the session parks and the client dials again.
    pub(crate) fn tick(s: &ExternalConnection) -> bool {
        let Some(core) = s.owner.lock().clone().and_then(|o| o.downcast::<Core>().ok()) else { return false }; // the slot of a client only
        let stalled = {
            let mut w = core.watch.lock();
            Redial::stalled(s.progress(), s.carried.load(Ordering::Relaxed), s.writing.load(Ordering::Relaxed), super::external_connection::now_ms(), &mut w)
        };
        if stalled {
            let e = std::io::Error::new(std::io::ErrorKind::TimedOut, format!("The link carried almost nothing for {} s while the session needed it: the socket is taken as lost, the client dials again.", Redial::STALL_MS / 1000));
            s.failure(&e);
            s.lost(); // the session parks, and the client dials again
        }
        stalled
    }

    /// On the loop, as the slot `s` parks its session - the loss, or a dial of it that failed: a
    /// client's session that holds what the peer has not confirmed dials again by the backoff
    /// ([`Redial`]), a wait of the slot's loop ( [`redial`](Self::redial) ). No thread waits for it.
    pub(crate) fn parked(s: &ExternalConnection) {
        let Some(core) = s.owner.lock().clone().and_then(|o| o.downcast::<Core>().ok()) else { return }; // the slot of a client only
        if core.given_up.load(Ordering::Acquire) || !s.progress().unconfirmed.held {
            return;
        }
        if s.heard.swap(false, Ordering::AcqRel) {
            core.redials.store(0, Ordering::Release); // the socket lost had served: the waits start anew, at its address
        } else {
            core.turn.fetch_add(1, Ordering::AcqRel); // the dial did not serve: the next address of the name
        }
        let wait = Redial::wait(core.redials.fetch_add(1, Ordering::AcqRel));
        core.engine.loops[s.home as usize - 1].redial_in(wait, Arc::downgrade(&core));
    }

    /// On the loop: the wait of the parked session for its next dial is over - it dials, unless a
    /// connect is under way, the application closed, or the session is gone meanwhile. The name of a
    /// WebSocket's URL is resolved anew on a thread for this one dial: the resolver may block, the
    /// loop does not; a resolver that fails leaves the address of the last dial.
    pub(crate) fn redial(self: &Arc<Self>) {
        let Some(s) = self.slot.lock().clone() else { return };
        if self.given_up.load(Ordering::Acquire) || self.is_connecting() || s.conn_state.load(Ordering::Acquire) != pool::PARKED || !s.progress().unconfirmed.held {
            return;
        }
        let Some((server, timeout, ws)) = self.target.lock().clone() else { return };
        let Some((host, port)) = ws.as_ref().and_then(|u| u.name.clone()) else {
            let _ = self.connect(server, Redial::connect(timeout), ws, Box::new(|_| {})); // whoever waits hears it through the events of the connection
            return;
        };
        let core = self.clone();
        let _ = std::thread::Builder::new().name(format!("{} resolver", self.name)).spawn(move || {
            let server = super::web_socket::client::addresses(&host, port).map_or(server, |all| all[core.turn.load(Ordering::Acquire) as usize % all.len()]);
            if !core.given_up.load(Ordering::Acquire) && !core.is_connected() {
                let _ = core.connect(server, Redial::connect(timeout), ws, Box::new(|_| {}));
            }
        });
    }

    pub(crate) fn is_connecting(&self) -> bool {
        self.waiting.lock().0
    }

    /// The chain of the next connects: TLS.
    pub(crate) fn set_chain(&self, chain: crate::lib::chain::Chain) {
        chain.tls_only();
        *self.chain.lock() = Some(Arc::new(chain));
    }

    /// The next connects of a WebSocket: of TLS for this name, with what was chosen so far inside;
    /// `None` - with no TLS.
    pub(crate) fn add_tls(&self, name: Option<&str>) {
        let mut chain = self.chain.lock();
        match name {
            Some(n) => *chain = Some(Arc::new(chain.as_deref().cloned().unwrap_or_default().tls(n, Some("http/1.1")))),
            None if chain.as_ref().is_some_and(|c| c.has_tls_for(false)) => *chain = Some(Arc::new(chain.as_deref().cloned().unwrap().without_tls())),
            None => {}
        }
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.slot.lock().as_ref().is_some_and(|s| s.is_active())
    }

    pub(crate) fn connection(&self) -> Option<Arc<ExternalConnection>> {
        self.slot.lock().clone()
    }

    pub(crate) fn disconnect(&self, gracefully: bool) {
        self.given_up.store(true, Ordering::Release); // the application's close: no dial of the client's own
        self.redials.store(0, Ordering::Release); // the dials of the client's own after its next connect count anew
        let Some(s) = self.slot.lock().clone() else { return };
        if s.is_active() {
            if gracefully {
                s.close()
            } else {
                s.abort()
            }
        }
    }

    pub(crate) fn connect(self: &Arc<Self>, server: SocketAddr, timeout: Option<Duration>, ws: Option<Arc<Upgrade>>, on_connected: OnConnected) -> std::io::Result<()> {
        if self.is_connected() {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "Client is already connected. Close the existing connection before attempting a new one."));
        }
        {
            let mut w = self.waiting.lock();
            w.1.push(on_connected);
            if w.0 {
                return Ok(()); // the connect under way answers this one too
            }
            w.0 = true;
        }
        *self.target.lock() = Some((server, Some(timeout.unwrap_or(Duration::from_secs(5))), ws));
        self.given_up.store(false, Ordering::Release);
        self.clone().start();
        Ok(())
    }

    /// Opens the socket, in the slot of the client: a teardown under way is waited for on the
    /// slot's loop, a parked session goes on, a free slot takes a session anew. The slot carries
    /// the connect to its loop (no allocation for it); the first connect, before the client has its
    /// slot, is a task of the loop.
    pub(crate) fn start(self: Arc<Self>) {
        let Some((server, timeout, ws)) = self.target.lock().clone() else { return };
        let codec = |ws: &Option<Arc<Upgrade>>| ws.as_ref().map(|u| Codec::client(&u.header, &u.target, u.masking));
        let slot = self.slot.lock().clone();
        if let Some(s) = &slot {
            match s.conn_state.load(Ordering::Acquire) {
                pool::CLEANING => {
                    // a connect from a close handler: after the pass that tears the slot down
                    let lp = self.engine.loops[s.home as usize - 1].clone();
                    *s.dial.lock() = Some(Pending::Start(self.clone()));
                    s.request(&lp, crate::lib::network::engine::CONNECT);
                    return;
                }
                pool::PARKED => {
                    s.set_chain(self.chain.lock().clone());
                    let opts = self.opts.lock().clone(); // the client's own, as for a fresh connect
                    return redial(s, &self.engine, server, timeout, codec(&ws), opts, Report::client(self.clone()));
                }
                _ => {}
            }
        }
        let opts = self.opts.lock().clone();
        dial(&self.engine, server, timeout, codec(&ws), opts, SlotOf::Pool(self.pool.clone(), self.home), false, SeatBy::Client(self.clone()), Report::client(self.clone()), slot.as_ref());
    }

    /// The protocol layer of the client on the slot `s` of its connect: the client lives with its
    /// slot, as long as its engine; the slot is the client's from the first connect on - its API
    /// sees the connection while the session starts.
    pub(crate) fn seat_on(self: &Arc<Self>, s: &Arc<ExternalConnection>) -> Option<(Box<dyn BytesDst>, Box<dyn BytesSrc>)> {
        s.owner.lock().get_or_insert_with(|| self.clone());
        *self.slot.lock() = Some(s.clone());
        let seated = (self.seat)(s);
        s.set_chain(self.chain.lock().clone());
        Some(seated)
    }

    /// The connect is over: the slot is the client's from now on, and every connect that waited
    /// hears it. The reaper took the parked session as the reconnect came (`NotFound`): the connect
    /// starts anew on the free slot.
    pub(crate) fn connected(self: &Arc<Self>, r: std::io::Result<Arc<ExternalConnection>>) {
        if let Err(e) = &r {
            if e.kind() == std::io::ErrorKind::NotFound {
                return self.clone().start();
            }
        }
        if let Ok(s) = &r {
            *self.slot.lock() = Some(s.clone());
            *self.watch.lock() = (u64::MAX, super::external_connection::now_ms()); // the speed is watched anew from now
        }
        let mut list = {
            let mut w = self.waiting.lock();
            w.0 = false;
            std::mem::take(&mut w.1)
        };
        let last = list.pop();
        for w in list.drain(..) {
            w(match &r {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            });
        }
        {
            let mut w = self.waiting.lock();
            if w.1.is_empty() {
                w.1 = list; // the room stays for the next connect
            }
        }
        if let Some(w) = last {
            w(r)
        }
    }
}

impl std::fmt::Display for Core {
    /// `Client name -> server` of the last connect: made when asked, not per connect.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.target.lock() {
            Some((_, _, Some(u))) => write!(f, "Client {} -> ws://{}{}", self.name, u.header, u.target),
            Some((a, _, None)) => write!(f, "Client {} -> {a}", self.name),
            None => write!(f, "Client {}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    struct Quiet;
    impl BytesDst for Quiet {
        fn on_bytes(&mut self, _: &[u8]) -> bool {
            true
        }
    }
    impl BytesSrc for Quiet {
        fn read(&mut self, _: &mut [u8]) -> isize {
            -1
        }
    }

    /// A server on a thread that shuts every socket it takes at once - after a byte when it
    /// `serves`; and how many it took.
    fn door(serves: bool) -> (SocketAddr, Arc<AtomicUsize>) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let (addr, taken) = (l.local_addr().unwrap(), Arc::new(AtomicUsize::new(0)));
        let t = taken.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                t.fetch_add(1, Ordering::AcqRel);
                if serves {
                    let _ = std::io::Write::write_all(&mut s, b"x");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        (addr, taken)
    }

    /// A client whose session parks on a loss holding what the peer has not confirmed: it dials
    /// by itself.
    fn holding(name: &str) -> Client {
        Client::new(name, crate::lib::network::engine::MIN_BUFFER, Arc::new(|s: &Arc<ExternalConnection>| {
            s.identify(1);
            s.set_resumable_seconds_hook(Arc::new(|_| 60));
            s.set_seat_hook(Arc::new(|_: &Arc<ExternalConnection>| Some((Box::new(Quiet) as Box<dyn BytesDst>, Box::new(Quiet) as Box<dyn BytesSrc>)))); // the session goes on on the next socket
            s.set_progress_hook(Arc::new(|| super::super::external_connection::Progress {
                unconfirmed: crate::lib::adhoc::connection::transmitter::resumable::Unconfirmed { held: true, owed: false },
                ..Default::default()
            }));
            (Box::new(Quiet) as Box<dyn BytesDst>, Box::new(Quiet) as Box<dyn BytesSrc>)
        }))
        .on_failure(Arc::new(|_: &dyn std::fmt::Display, _: &(dyn std::error::Error + 'static)| {}))
    }

    /// A door that takes the socket and shuts it proves nothing: the waits grow, not a dial a
    /// second for as long as the session waits.
    #[test]
    fn a_door_that_shuts_the_socket_does_not_start_the_waits_anew() {
        let (addr, taken) = door(false);
        let c = holding("door");
        c.connect(addr, Some(Duration::from_secs(5)), |_| {}).unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        while c.redials() < 3 {
            assert!(Instant::now() < until, "the waits do not grow: {} dials, {} sockets", c.redials(), taken.load(Ordering::Acquire));
            std::thread::sleep(Duration::from_millis(50));
        }
        c.shutdown();
    }

    /// The next dial of a parked session is a wait of its loop, as in Java: the session is asked on
    /// the loop's thread only, and the dials come by the backoff - about a second first, longer after
    /// each one that did not serve -, not on the tick of the loop ( 5 s ) and with no thread of their own.
    #[test]
    fn a_parked_session_dials_again_on_its_loop_by_the_backoff() {
        let (addr, taken) = door(false);
        let asked = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let a = asked.clone();
        let c = Client::new("loopdial", crate::lib::network::engine::MIN_BUFFER, Arc::new(move |s: &Arc<ExternalConnection>| {
            s.identify(1);
            s.set_resumable_seconds_hook(Arc::new(|_| 60));
            s.set_seat_hook(Arc::new(|_: &Arc<ExternalConnection>| Some((Box::new(Quiet) as Box<dyn BytesDst>, Box::new(Quiet) as Box<dyn BytesSrc>))));
            let a = a.clone();
            s.set_progress_hook(Arc::new(move || {
                a.lock().push(std::thread::current().name().unwrap_or("").to_string());
                super::super::external_connection::Progress { unconfirmed: crate::lib::adhoc::connection::transmitter::resumable::Unconfirmed { held: true, owed: false }, ..Default::default() }
            }));
            (Box::new(Quiet) as Box<dyn BytesDst>, Box::new(Quiet) as Box<dyn BytesSrc>)
        }))
        .on_failure(Arc::new(|_: &dyn std::fmt::Display, _: &(dyn std::error::Error + 'static)| {}));
        c.connect(addr, Some(Duration::from_secs(5)), |_| {}).unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        let mut at = vec![];
        let mut seen = 0;
        while at.len() < 3 {
            assert!(Instant::now() < until, "{} sockets only", taken.load(Ordering::Acquire));
            let n = taken.load(Ordering::Acquire);
            if seen < n {
                seen = n;
                at.push(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        c.shutdown();
        let (first, second) = (at[1] - at[0], at[2] - at[1]);
        assert!(Duration::from_millis(750) <= first && first < Duration::from_millis(2_500), "the first dial after the loss waits a second or so: {first:?}");
        assert!(Duration::from_millis(1_200) <= second && second < Duration::from_millis(3_500), "the next waits longer: {second:?}");
        let asked = asked.lock();
        assert!(!asked.is_empty() && asked.iter().all(|t| t.contains("-io-")), "the session is asked on its loop only: {asked:?}");
    }

    /// The machine slept longer than the receive timeout of a line: woken, the line ends within a
    /// tick - not a whole timeout later. Alone, as the sleep moves the clock of every test of the
    /// process: `cargo test -- --ignored a_sleep`.
    #[test]
    #[ignore]
    fn a_sleep_longer_than_the_receive_timeout_ends_the_line() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new(); // open and silent
            for s in l.incoming() {
                held.push(s);
            }
        });
        let c = Client::new("sleep", crate::lib::network::engine::MIN_BUFFER, Arc::new(|s: &Arc<ExternalConnection>| {
            s.set_receive_timeout_ms(30_000); // before the read starts: a read that waits keeps the deadline it has
            (Box::new(Quiet) as Box<dyn BytesDst>, Box::new(Quiet) as Box<dyn BytesSrc>)
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        c.connect(addr, Some(Duration::from_secs(5)), move |r| {
            let _ = tx.send(r);
        })
        .unwrap();
        let s = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(s.is_active());
        crate::lib::network::engine::sleep_for_test(40_000_000_000);
        let until = Instant::now() + Duration::from_secs(7);
        while s.is_active() {
            assert!(Instant::now() < until, "the line lives on after a sleep longer than its timeout");
            std::thread::sleep(Duration::from_millis(50));
        }
        c.shutdown();
    }

    /// A server that said something served: the waits start anew after each of its sockets.
    #[test]
    fn a_socket_that_served_starts_the_waits_anew() {
        let (addr, taken) = door(true);
        let c = holding("served");
        c.connect(addr, Some(Duration::from_secs(5)), |_| {}).unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        while taken.load(Ordering::Acquire) < 4 {
            assert!(c.redials() <= 1, "the waits grow after sockets that served: {} dials", c.redials());
            assert!(Instant::now() < until, "{} sockets", taken.load(Ordering::Acquire));
            std::thread::sleep(Duration::from_millis(20));
        }
        c.shutdown();
    }
}
