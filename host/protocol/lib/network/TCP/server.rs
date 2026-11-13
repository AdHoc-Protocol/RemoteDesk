// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! TCP listener and builder for protocol connection factories.

use super::*;

pub struct Builder {
    name: String,
    addr: SocketAddr,
    backlog: u32,
    on_connect: Option<ConnectionFactory>,
    on_event: Option<EventCallback>,
    chain: Option<Arc<crate::lib::chain::Chain>>,
    choose_chain: Option<super::ChooseChain>,
    max_connections: Option<usize>,
    accept_rate: Option<usize>,
    accept_window: Duration,
    max_per_address: Option<usize>,
    socket_opts: SocketOptions,
    /// How long the listener stays deaf after accept failed.
    accept_retry: Duration,
    /// How long an accepted connection may take over its handshakes, in milliseconds.
    handshake_timeout_ms: i64,
    engine: Option<Arc<Engine>>,
    on_failure: Option<crate::lib::network::failure::OnFailure>,
    /// The connections are WebSockets.
    ws: bool,
}

impl Builder {
    pub fn new<A: Into<SocketAddr>>(name: impl Into<String>, addr: A) -> Self {
        Self {
            name: name.into(),
            addr: addr.into(),
            backlog: 1024,
            on_connect: None,
            on_event: None,
            chain: None,
            choose_chain: None,
            max_connections: None,
            accept_rate: None,
            accept_window: Duration::from_secs(1),
            max_per_address: None,
            socket_opts: SocketOptions::default(),
            accept_retry: Duration::from_secs(1),
            handshake_timeout_ms: 30_000,
            engine: None,
            on_failure: None,
            ws: false,
        }
    }

    /// The connections speak RFC 6455: each one a [`super::web_socket::WebSocket`].
    pub fn web_socket(mut self) -> Self {
        self.ws = true;
        self
    }

    /// Where the failures of the server and of its connections go. Default: the handler of its
    /// engine.
    pub fn on_failure(mut self, f: crate::lib::network::failure::OnFailure) -> Self {
        self.on_failure = Some(f);
        self
    }

    /// How long the listener stays deaf after accept failed - out of descriptors, of memory -
    /// before it listens again: the backlog holds the peers meanwhile, and the failure is reported
    /// once, not once per attempt.
    pub fn accept_retry(mut self, wait: Duration) -> Self {
        self.accept_retry = wait;
        self
    }

    /// How long an accepted connection may take over its handshakes - TLS, Noise, the upgrade of a
    /// WebSocket - in milliseconds. A peer that connected and stays silent, or speaks plain text to a
    /// port of TLS, holds a slot no longer than this. Default 30000; non-positive: no limit. A
    /// connection with no handshake of its own has nothing to limit.
    pub fn handshake_timeout(mut self, ms: i64) -> Self {
        self.handshake_timeout_ms = ms;
        self
    }

    /// The listen backlog: how many completed connections the kernel queues while the acceptor is
    /// busy.
    pub fn listen_backlog(mut self, backlog: u32) -> Self {
        self.backlog = backlog;
        self
    }

    /// Per-connection factory. Required.
    pub fn on_connect(mut self, f: ConnectionFactory) -> Self {
        self.on_connect = Some(f);
        self
    }

    pub fn on_event(mut self, f: EventCallback) -> Self {
        self.on_event = Some(f);
        self
    }

    /// TLS under every accepted connection ([`Chain::tls_server`](crate::lib::chain::Chain::tls_server)). The Noise inside it
    /// is not chosen here: the protocol layer seated declares it (`[Noise]` of its description).
    pub fn chain(mut self, chain: crate::lib::chain::Chain) -> Self {
        chain.tls_only();
        self.chain = Some(Arc::new(chain));
        self
    }

    /// Chooses the chain of every incoming connection by where it comes from, in place of
    /// [`chain`](Self::chain): TLS alone, as there. `None` - the socket is used as it is.
    pub fn choose_chain(mut self, choose: impl Fn(SocketAddr) -> Option<Arc<crate::lib::chain::Chain>> + Send + Sync + 'static) -> Self {
        self.choose_chain = Some(Arc::new(choose));
        self
    }

    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = Some(max);
        self
    }

    pub fn accept_rate(mut self, rate: usize, window: Duration) -> Self {
        self.accept_rate = Some(rate);
        self.accept_window = window;
        self
    }

    pub fn max_per_address(mut self, max: usize) -> Self {
        self.max_per_address = Some(max);
        self
    }

    pub fn no_delay(mut self, on: bool) -> Self {
        self.socket_opts.no_delay = on;
        self
    }

    pub fn keep_alive(mut self, idle: Duration) -> Self {
        self.socket_opts.keep_alive = Some(idle);
        self
    }

    pub fn socket_receive_buffer(mut self, size: usize) -> Self {
        self.socket_opts.socket_receive_buffer = Some(size);
        self
    }

    pub fn socket_transmit_buffer(mut self, size: usize) -> Self {
        self.socket_opts.socket_transmit_buffer = Some(size);
        self
    }

    /// The threads the connections run on, and the size of their buffers. Default:
    /// [`Engine::shared`].
    pub fn engine(mut self, engine: Arc<Engine>) -> Self {
        self.engine = Some(engine);
        self
    }

    /// Listens from here on.
    pub fn build(self) -> std::io::Result<Server> {
        build(self.name, self.addr, self.backlog, self.ws, self.on_connect, self.on_event, self.chain, self.choose_chain, self.max_connections, self.accept_rate, self.accept_window, self.max_per_address, self.socket_opts, self.accept_retry, self.handshake_timeout_ms, self.engine, self.on_failure)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    name: String,
    addr: SocketAddr,
    backlog: u32,
    ws: bool,
    on_connect: Option<ConnectionFactory>,
    on_event: Option<EventCallback>,
    chain: Option<Arc<crate::lib::chain::Chain>>,
    choose_chain: Option<super::ChooseChain>,
    max_connections: Option<usize>,
    accept_rate: Option<usize>,
    accept_window: Duration,
    max_per_address: Option<usize>,
    socket_opts: SocketOptions,
    accept_retry: Duration,
    handshake_timeout_ms: i64,
    engine: Option<Arc<Engine>>,
    on_failure: Option<crate::lib::network::failure::OnFailure>,
) -> std::io::Result<Server> {
    let on_connect = on_connect.ok_or_else(|| std::io::Error::other("on_connect factory is required"))?;
    let engine = engine.unwrap_or_else(Engine::shared);
    if ws {
        super::web_socket::fits(&engine, &name)?;
    }
    let admission = Arc::new(Admission::new(max_connections, accept_rate, accept_window, max_per_address));
    let on_failure = on_failure.unwrap_or_else(|| engine.on_failure());
    let pool = super::Pool::new(name.clone(), Some(engine.loops.clone()), Some(on_failure.clone()));
    let host = Arc::new(Host {
        name: name.clone(),
        on_failure,
        ws,
        on_connect,
        on_event,
        chain,
        choose_chain,
        pool: pool.clone(),
        admission: admission.clone(),
        socket_opts,
        accept_retry,
        handshake_timeout_ms,
    });
    let first = engine.listen(addr, backlog.min(i32::MAX as u32) as i32, socket_opts.socket_receive_buffer, host.clone())?;
    engine.watch_parked(&pool); // the tick of the engine reaps its parked sessions
    Ok(Server { name, listening: Arc::new(parking_lot::Mutex::new(vec![first])), pool, admission, host, engine })
}

/// A listening TCP server: it accepts from the moment it is built, the tick of its engine reaps its
/// parked sessions, [`shutdown`](Self::shutdown) ends it. A handle: any clone stops it.
#[derive(Clone)]
pub struct Server {
    pub name: String,
    /// What it listens on: the address it was built with, and those bound since.
    listening: Arc<parking_lot::Mutex<Vec<Listening>>>,
    /// The slots of its connections.
    pub pool: Arc<super::Pool>,
    pub admission: Arc<Admission>,
    host: Arc<Host>,
    engine: Arc<Engine>,
}

impl std::fmt::Display for Server {
    /// Formats the server name and its listening addresses.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Server {}", self.name)?;
        for a in self.local_addrs() {
            write!(f, "\n\t\t -> {a}")?;
        }
        Ok(())
    }
}

impl Server {
    /// The first address it listens on, as bound.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listening.lock().first().map(Listening::local_addr).ok_or_else(|| std::io::Error::other("it listens on nothing"))
    }

    /// Every address it listens on, as bound.
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.listening.lock().iter().map(Listening::local_addr).collect()
    }

    /// Listens on `addr` too, from now on: the connections accepted there are this server's as any.
    /// Returns the address as bound. A server that was deafened listens again.
    pub fn bind(&self, addr: SocketAddr, backlog: u32) -> std::io::Result<SocketAddr> {
        let mut l = self.listening.lock();
        let more = self.engine.listen(addr, backlog.min(i32::MAX as u32) as i32, self.host.socket_opts.socket_receive_buffer, self.host.clone())?;
        let at = more.local_addr();
        l.push(more);
        Ok(at)
    }

    /// Takes in a socket connected some other way as if this server had accepted it: the admission, the protocol layer, the
    /// upgrade of a WebSocket - all as for one it accepted. A socket dialed out to meet a peer in the NAT of a router ( TCP
    /// simultaneous open ), one handed over by another process. Refused by the admission: an error, and the socket closes.
    pub fn adopt(&self, stream: std::net::TcpStream) -> std::io::Result<()> {
        let peer = stream.peer_addr()?;
        stream.set_nonblocking(true)?;
        let counted = self.host.admit(peer).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "the admission of the server refused it"))?;
        let host = self.host.clone();
        let stream = mio::net::TcpStream::from_std(stream);
        self.engine.pick().submit_fn(Box::new(move |lp: &Loop| {
            let failed = host.clone();
            if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.accepted(lp, stream, peer, counted))) {
                failed.failure(&crate::lib::network::failure::panicked(&*why));
            }
        }));
        Ok(())
    }

    /// The live WebSocket connection `id`: its PING, its CLOSE.
    pub fn web_socket(&self, id: ConnId) -> Option<super::web_socket::WebSocket> {
        self.pool.get_connection(id.0).map(|state| super::web_socket::WebSocket { state: state.clone() })
    }

    /// A snapshot of the host's counters.
    pub fn census(&self) -> Census {
        Census {
            live: self.pool.connections_live(),
            parked: self.pool.census()[super::pool::PARKED as usize],
            crowded: self.admission.crowded(),
            throttled: self.admission.throttled(),
            refused: self.admission.refused(),
        }
    }

    /// The engine its connections run on.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// How long a read after our own FIN waits for the peer's, in milliseconds.
    pub fn linger_timeout(&self) -> i64 {
        self.pool.linger_timeout()
    }

    /// Sets it for the sessions that start from now on.
    pub fn set_linger_timeout(&self, ms: i64) {
        self.pool.set_linger_timeout(ms);
    }

    /// Takes nothing new in: the listener is given up, what is accepted goes on. The way to stop
    /// taking new peers while the server goes on serving the ones it has.
    pub fn deafen(&self) {
        for one in self.listening.lock().drain(..) {
            one.close(); // the addresses are given up: a bind later listens anew
        }
    }

    /// The process ends, and the sessions of this server go on in the next one: nothing new is
    /// taken in, then every identified session parks and its files are closed cleanly
    /// ([`super::pool::Pool::stop`]). Returns how many sessions were closed.
    pub fn stop(&self, wait_ms: u64) -> usize {
        self.deafen(); // no peer comes back into this process: what it would take after the stop the next process
        // would not know
        self.pool.stop(wait_ms)
    }

    /// The sessions the process before left in their files - it ended or it fell - go on in this
    /// one, parked ([`super::pool::Pool::restore`]). Before the server listens.
    pub fn restore(&self, internals: &dyn Fn(u32, &mut &[u8], &Arc<ExternalConnection>) -> Option<Arc<dyn std::any::Any + Send + Sync>>) -> std::io::Result<Vec<Arc<ExternalConnection>>> {
        self.pool.restore(internals)
    }

    /// Stops at once: no more listening, every connection torn down where it stands. Whatever was
    /// in a send queue stays there.
    pub fn shutdown(&self) {
        self.shutdown_graceful(Duration::ZERO);
    }

    /// Stops, giving the sessions `grace` to finish what they were saying: nothing new is taken in,
    /// every connection is asked to close the polite way - its queue drains, then the FIN (a
    /// WebSocket's CLOSE) -, whoever is still there when `grace` runs out is torn down. `true` —
    /// every session ended on its own.
    pub fn shutdown_graceful(&self, grace: Duration) -> bool {
        self.deafen();
        super::stop(&self.pool, &self.engine, grace)
    }

}

