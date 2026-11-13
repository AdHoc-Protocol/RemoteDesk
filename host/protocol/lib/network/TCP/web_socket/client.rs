// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! WebSocket clients with asynchronous TCP connection and HTTP upgrade.
//!
//! [`Client`] reuses a single connection slot. Standalone [`connect`] helpers use the engine's
//! client slots. The result is delivered once a successful upgrade response arrives.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use super::super::super::admission::SocketOptions;
use super::super::super::engine::Engine;
use super::super::client::{Core, Upgrade};
use super::super::external_connection::ExternalConnection;
use super::super::ConnectionFactory;
use super::{Codec, WebSocket};
use crate::lib::adhoc::{BytesDst, BytesSrc};

/// What a WebSocket client hears of its connect: the connection once the server's 101 came and
/// checked out, or what went wrong.
pub type OnConnected = Box<dyn FnOnce(std::io::Result<WebSocket>) + Send>;

/// Parses a `ws://[user@]host[:port][/path][?query][#fragment]` URL into a dial address, HTTP Host
/// header, and request target. User information and fragments are excluded. An omitted or empty
/// port defaults to 80, of `wss://` to 443. IPv6 hosts retain brackets in the Host header.
fn parse(url: &str) -> std::io::Result<(String, u16, String, String)> {
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("not a ws:// URL: {url}"));
    let (rest, standard) = if url.len() >= 5 && url[..5].eq_ignore_ascii_case("ws://") {
        (&url[5..], 80u16)
    } else if is_wss(url) {
        (&url[6..], 443u16)
    } else {
        return Err(bad());
    };
    let rest = rest.split_once('#').map_or(rest, |(r, _)| r);
    let (authority, target) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_string()),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let port_of = |p: &str| if p.is_empty() { Ok(standard) } else { p.parse::<u16>().map_err(|_| bad()) };
    let v6 = authority.starts_with('[');
    let (host, port) = if let Some(h) = authority.strip_prefix('[') {
        let end = h.find(']').ok_or_else(bad)?;
        let port = h[end + 1..].strip_prefix(':').map(port_of).transpose()?.unwrap_or(standard);
        (h[..end].to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), port_of(p)?),
            None => (authority.to_string(), standard),
        }
    };
    if host.is_empty() {
        return Err(bad());
    }
    // Include a nondefault port and retain IPv6 brackets in the Host header.
    let name = if v6 { format!("[{host}]") } else { host.clone() };
    let header = if port == standard { name } else { format!("{name}:{port}") };
    Ok((host, port, header, target))
}

#[inline]
fn is_wss(url: &str) -> bool {
    url.len() >= 6 && url[..6].eq_ignore_ascii_case("wss://")
}

/// The chain of a connection to `url` that has no other: TLS for a `wss://` one.
fn wss_chain(url: &str) -> std::io::Result<Option<Arc<crate::lib::chain::Chain>>> {
    if !is_wss(url) {
        return Ok(None);
    }
    let (host, ..) = parse(url)?;
    Ok(Some(Arc::new(crate::lib::chain::Chain::default().tls(&host, Some("http/1.1")))))
}

/// Connects to `url` on the engine of the process, with the protocol layer `seat` gives seated on
/// the connection's state first, in a slot of the clients of the engine, and upgrades.
/// `on_connected` hears on the thread of the connection's loop once the server's 101 came and
/// checked out - `WEBSOCKET_THIS_CONNECT` has been heard by then -, or what went wrong: `TimedOut`
/// when `timeout` ran out first. A bad URL, or a host with no address, it hears at once, on this
/// thread.
pub fn connect(
    url: &str,
    timeout: Option<Duration>,
    seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
    on_connected: impl FnOnce(std::io::Result<WebSocket>) + Send + 'static,
) {
    connect_with(url, timeout, &Engine::shared(), true, seat, on_connected)
}

/// As [`connect`](connect), on `engine`; `masking` off sends the payload unmasked (a zero key) -
/// for a server reached without intermediaries.
pub fn connect_with(
    url: &str,
    timeout: Option<Duration>,
    engine: &Arc<Engine>,
    masking: bool,
    seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
    on_connected: impl FnOnce(std::io::Result<WebSocket>) + Send + 'static,
) {
    connect_with_options(url, timeout, engine, masking, SocketOptions::default(), seat, on_connected)
}

/// As [`connect_with`](connect_with), with `opts` for the socket.
pub fn connect_with_options(
    url: &str,
    timeout: Option<Duration>,
    engine: &Arc<Engine>,
    masking: bool,
    opts: SocketOptions,
    seat: impl FnOnce(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) + Send + 'static,
    on_connected: impl FnOnce(std::io::Result<WebSocket>) + Send + 'static,
) {
    if let Err(e) = super::fits(engine, url) {
        return on_connected(Err(e));
    }
    let (addr, header, target, _) = match resolve(url) {
        Ok(a) => a,
        Err(e) => return on_connected(Err(e)),
    };
    let ws = Some(Codec::client(&header, &target, masking));
    let chain = match wss_chain(url) {
        Ok(c) => c,
        Err(e) => return on_connected(Err(e)),
    };
    super::super::dial(
        engine,
        addr,
        timeout,
        ws,
        opts,
        super::super::SlotOf::Clients,
        false,
        super::super::SeatBy::Fn(Box::new(move |s| {
            s.set_chain(chain); // the slot may have had another connection
            Some(seat(s))
        })),
        super::super::Report::of(Box::new(move |r: std::io::Result<Arc<ExternalConnection>>| on_connected(r.map(|state| WebSocket { state })))),
        None,
    );
}

/// Continues the parked session of the WebSocket client connection `state` on a new socket to
/// `url`, upgraded anew: its protocol layer, in the Noise it declares, its journal and its receive
/// position go on on the new connection, which takes the identity of the session; once the 101 came
/// the peer is asked to go on from where the receiver of the session is. A failed attempt parks the
/// session again.
pub fn reconnect(state: &Arc<ExternalConnection>, url: &str, timeout: Option<Duration>, engine: &Arc<Engine>, masking: bool, on_connected: impl FnOnce(std::io::Result<WebSocket>) + Send + 'static) {
    let (addr, header, target, _) = match resolve(url) {
        Ok(a) => a,
        Err(e) => return on_connected(Err(e)),
    };
    let ws = Some(Codec::client(&header, &target, masking));
    super::super::redial(
        state,
        engine,
        addr,
        timeout,
        ws,
        SocketOptions::default(),
        super::super::Report::of(Box::new(move |r: std::io::Result<Arc<ExternalConnection>>| on_connected(r.map(|state| WebSocket { state })))),
    );
}

/// A WebSocket client of one connection: as [`super::super::Client`], every socket upgraded first.
pub struct Client {
    core: Arc<Core>,
    masking: std::sync::atomic::AtomicBool,
}

impl Client {
    /// A client on an engine of its own, of one loop. `seat` gives the protocol layer, seated on
    /// the connection before every connect.
    pub fn new(name: &str, buffer_size: usize, seat: ConnectionFactory) -> Self {
        Self::with_engine(name, &Engine::new(name, 1, buffer_size), seat)
    }

    /// As [`new`](Self::new), on `engine`: the threads the connection runs on, possibly shared with
    /// other hosts. Panics when the buffers of `engine` are less than [`super::MIN_BUFFER`].
    pub fn with_engine(name: &str, engine: &Arc<Engine>, seat: ConnectionFactory) -> Self {
        super::fits(engine, name).unwrap();
        Self {
            core: Core::new(name, engine, seat),
            masking: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// How long a read after our own FIN waits for the peer's, in milliseconds.
    pub fn linger_timeout(&self) -> i64 {
        self.core.pool.linger_timeout()
    }

    /// Sets it for the sessions that start from now on.
    pub fn set_linger_timeout(&self, ms: i64) {
        self.core.pool.set_linger_timeout(ms);
    }

    /// Connects to `server` and upgrades, naming no target: the root of whoever answers there, its
    /// address the `Host`. As [`connect`](Self::connect) otherwise.
    pub fn connect_addr(&self, server: SocketAddr, timeout: Option<Duration>, on_connected: impl FnOnce(std::io::Result<WebSocket>) + Send + 'static) -> std::io::Result<()> {
        let header = match server.ip() {
            std::net::IpAddr::V6(a) => format!("[{a}]"),
            a => a.to_string(),
        };
        self.core.add_tls(None);
        let upgrade = Arc::new(Upgrade {
            header,
            target: "/".to_string(),
            masking: self.masking.load(std::sync::atomic::Ordering::Relaxed),
            name: None,
        });
        self.core.connect(
            server,
            timeout,
            Some(upgrade),
            Box::new(move |r: std::io::Result<Arc<ExternalConnection>>| on_connected(r.map(|state| WebSocket { state }))),
        )
    }

    /// Stops at once: the connection is torn down where it stands, and the engine's threads end -
    /// unless it is the engine of the process.
    pub fn shutdown(&self) {
        self.core.give_up();
        super::super::stop(&self.core.pool, &self.core.engine, Duration::ZERO);
    }

    /// Stops, giving the session `grace` to finish what it was saying: asked to close the polite
    /// way, torn down when `grace` runs out, then the engine's threads end. `true` - the session
    /// ended on its own.
    pub fn shutdown_graceful(&self, grace: Duration) -> bool {
        self.core.give_up();
        super::super::stop(&self.core.pool, &self.core.engine, grace)
    }

    /// The dials of the client's own since a socket last served - its peer said something: the step
    /// of the backoff ([`Redial`](super::super::client::Redial)).
    pub fn redials(&self) -> u32 {
        self.core.redials()
    }

    /// Its failures - of its connection, of the connects - go to `on_failure`; the engine's handler
    /// when none. Before the first connect.
    pub fn on_failure(self, on_failure: crate::lib::network::failure::OnFailure) -> Self {
        Self {
            core: self.core.with_failure(on_failure),
            ..self
        }
    }

    /// The engine its connection runs on.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.core.engine
    }

    /// The options of the socket of the next connect.
    pub fn set_options(&self, opts: SocketOptions) {
        *self.core.opts.lock() = opts;
    }

    /// Off: the payload of the next connects goes unmasked (a zero key) - for a server reached
    /// without intermediaries.
    pub fn set_masking(&self, on: bool) {
        self.masking.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Connects to `url` and upgrades, `timeout` - 5 s when none. `on_connected` hears on the
    /// thread of the connection's loop once the server's 101 came and checked out, or what went
    /// wrong. A parked session goes on on the new socket. `Err`: a bad URL, a host with no address,
    /// or connected already.
    pub fn connect(&self, url: &str, timeout: Option<Duration>, on_connected: impl FnOnce(std::io::Result<WebSocket>) + Send + 'static) -> std::io::Result<()> {
        let (addr, header, target, name) = resolve(url)?;
        if is_wss(url) {
            let (host, ..) = parse(url)?;
            self.core.add_tls(Some(&host)); // TLS under what was chosen for inside it
        } else {
            self.core.add_tls(None);
        }
        let upgrade = Arc::new(Upgrade {
            header,
            target,
            masking: self.masking.load(std::sync::atomic::Ordering::Relaxed),
            name,
        });
        self.core.connect(
            addr,
            timeout,
            Some(upgrade),
            Box::new(move |r: std::io::Result<Arc<ExternalConnection>>| on_connected(r.map(|state| WebSocket { state }))),
        )
    }

    /// As [`connect`](Self::connect), with a chain chosen - TLS: `wss://` puts it under the
    /// WebSocket. It holds for the connects after it, a parked session going on included, until
    /// another is given. The Noise inside the frames is of the protocol layer (`[Noise]` of its
    /// description).
    pub fn connect_chain(&self, chain: crate::lib::chain::Chain, url: &str, timeout: Option<Duration>, on_connected: impl FnOnce(std::io::Result<WebSocket>) + Send + 'static) -> std::io::Result<()> {
        self.core.set_chain(chain);
        self.connect(url, timeout, on_connected)
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

    /// Closes the connection abruptly.
    pub fn disconnect(&self) {
        self.core.disconnect(false)
    }

    /// Closes the connection gracefully: the CLOSE frame after what is queued.
    pub fn disconnect_gracefully(&self) {
        self.core.disconnect(true)
    }
}

impl std::fmt::Display for Client {
    /// Formats the client name and URL from the latest connection attempt.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&*self.core, f)
    }
}

/// The address `url` names, its `Host` header, its request target, and the host and port a client
/// resolves anew before every dial of its own - `None`: the URL names an address.
fn resolve(url: &str) -> std::io::Result<(SocketAddr, String, String, Option<(String, u16)>)> {
    let (host, port, header, target) = parse(url)?;
    let addr = addresses(&host, port)?[0];
    let name = host.parse::<std::net::IpAddr>().is_err().then_some((host, port));
    Ok((addr, header, target, name))
}

/// The addresses of `host`, never none: IPv4 first, so `localhost` reaches an IPv4-only listener
/// even when DNS lists IPv6 first; each family in the order DNS gave.
pub(crate) fn addresses(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs;
    let mut all: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    if all.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("no address for {host}")));
    }
    all.sort_by_key(|a| !a.is_ipv4()); // stable
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::network::TCP::external_connection::Event;
    use std::net::Ipv4Addr;
    use std::sync::Mutex;

    #[test]
    fn urls() {
        assert_eq!(parse("ws://localhost:1234").unwrap(), ("localhost".into(), 1234, "localhost:1234".into(), "/".into()));
        assert_eq!(parse("ws://h/p?q=1").unwrap(), ("h".into(), 80, "h".into(), "/p?q=1".into()));
        assert_eq!(parse("ws://[::1]:9/x").unwrap(), ("::1".into(), 9, "[::1]:9".into(), "/x".into()));
        assert_eq!(parse("ws://[::1]:80/x").unwrap(), ("::1".into(), 80, "[::1]".into(), "/x".into()));
        assert_eq!(parse("ws://[::1]").unwrap(), ("::1".into(), 80, "[::1]".into(), "/".into()));
        assert_eq!(parse("ws://h:80").unwrap(), ("h".into(), 80, "h".into(), "/".into()));
        // URL parsing excludes user information and fragments and defaults an empty port to 80.
        assert_eq!(parse("ws://u:pw@h:7/p#frag").unwrap(), ("h".into(), 7, "h:7".into(), "/p".into()));
        assert_eq!(parse("ws://h:/p?q#f").unwrap(), ("h".into(), 80, "h".into(), "/p?q".into()));
        assert_eq!(parse("ws://[::1]:#f").unwrap(), ("::1".into(), 80, "[::1]".into(), "/".into()));
        assert_eq!(parse("wss://h").unwrap().1, 443);
    }

    /// A URL that names a host keeps the name: the client resolves it anew before every dial of its
    /// own; one that names an address has none. The addresses of a name go IPv4 first.
    #[test]
    fn a_name_is_kept_to_be_resolved_again() {
        let (a, _, _, name) = resolve("ws://localhost:1234/x").unwrap();
        assert!(a.is_ipv4() && a.port() == 1234, "{a}");
        assert_eq!(name, Some(("localhost".to_string(), 1234)));
        assert_eq!(resolve("ws://127.0.0.1:9").unwrap().3, None);
        assert_eq!(resolve("ws://[::1]:9").unwrap().3, None);
        let all = addresses("localhost", 7).unwrap();
        let v4 = all.iter().take_while(|a| a.is_ipv4()).count();
        assert!(0 < v4 && all[v4..].iter().all(|a| a.is_ipv6()), "{all:?}");
    }

    struct Collect(Arc<Mutex<Vec<u8>>>, Option<Arc<ExternalConnection>>, Arc<Mutex<Vec<u8>>>);
    impl BytesDst for Collect {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.0.lock().unwrap().extend_from_slice(bytes);
            if let Some(s) = &self.1 {
                self.2.lock().unwrap().extend_from_slice(bytes); // echoed
                s.wake_transmit();
            }
            true
        }
    }

    struct Out(Arc<Mutex<Vec<u8>>>);
    impl BytesSrc for Out {
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

    /// A peer that breaks the protocol - a frame with an RSV bit and no extension negotiated: the
    /// server hears WEBSOCKET_PROTOCOL_ERROR, why goes to its handler of failures, and the connection
    /// is closed.
    #[test]
    fn a_protocol_error_is_an_event_and_a_failure() {
        use std::io::{Read, Write};
        let events = Arc::new(Mutex::new(Vec::new()));
        let failures = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ev, fl) = (events.clone(), failures.clone());
        let server = crate::lib::network::TCP::server::Builder::new("ws-fail", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .web_socket()
            .on_connect(Arc::new(move |_| {
                (
                    Box::new(Collect(Default::default(), None, Default::default())) as Box<dyn BytesDst>,
                    Box::new(Out(Default::default())) as Box<dyn BytesSrc>,
                )
            }))
            .on_event(Arc::new(move |_, e| ev.lock().unwrap().push(e)))
            .on_failure(Arc::new(move |_, e| fl.lock().unwrap().push(e.to_string())))
            .build()
            .unwrap();
        let mut c = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
            .unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut answer = Vec::new();
        let mut b = [0u8; 256];
        while !answer.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = c.read(&mut b).unwrap();
            assert!(0 < n, "no 101");
            answer.extend_from_slice(&b[..n]);
        }
        c.write_all(&[0x80 | 0x40 | 0x2, 0x80 | 1, 1, 2, 3, 4, 0]).unwrap(); // FIN, RSV1, binary; masked, 1 byte
        let mut one = [0u8; 1];
        loop {
            match c.read(&mut one) {
                Ok(0) | Err(_) => break, // closed
                Ok(_) => continue,       // what the server sends before it closes
            }
        }
        for _ in 0..200 {
            if !failures.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(events.lock().unwrap().contains(&Event::WEBSOCKET_PROTOCOL_ERROR));
        assert_eq!(*failures.lock().unwrap(), ["Protocol Error: RSV bits set with no negotiated extension."]);
        server.shutdown();
    }

    /// One CLOSE however many times it is asked for, its reason cut to 123 bytes: the peer sees one
    /// frame of 125 bytes of payload.
    #[test]
    fn one_close_with_its_reason_cut() {
        use std::io::{Read, Write};
        let seated = Arc::new(Mutex::new(None::<Arc<ExternalConnection>>));
        let st = seated.clone();
        let server = crate::lib::network::TCP::server::Builder::new("ws-close", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .web_socket()
            .on_connect(Arc::new(move |state| {
                *st.lock().unwrap() = Some(state.clone());
                (
                    Box::new(Collect(Default::default(), None, Default::default())) as Box<dyn BytesDst>,
                    Box::new(Out(Default::default())) as Box<dyn BytesSrc>,
                )
            }))
            .build()
            .unwrap();
        let mut c = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
            .unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut answer = Vec::new();
        let mut b = [0u8; 512];
        while !answer.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = c.read(&mut b).unwrap();
            assert!(0 < n, "no 101");
            answer.extend_from_slice(&b[..n]);
        }
        let at = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let mut frames = answer[at..].to_vec();
        let ws = super::super::WebSocket {
            state: seated.lock().unwrap().clone().unwrap(),
        };
        let reason = "x".repeat(200);
        assert!(ws.close_graceful(1001, &reason).is_ok(), "a long reason is cut, not refused");
        assert!(ws.close_graceful(1000, "again").is_ok());
        assert!(ws.close().is_ok());
        c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        while let Ok(n) = c.read(&mut b) {
            if n == 0 {
                break;
            }
            frames.extend_from_slice(&b[..n]);
        }
        let mut one = vec![0x88u8, 125, 0x03, 0xE9];
        one.extend_from_slice(&reason.as_bytes()[..123]);
        assert_eq!(frames, one, "one CLOSE, the first asked for, its reason 123 bytes");
        server.shutdown();
    }

    /// An upgrade request longer than the buffer (a long target) fails the connect: whoever waits
    /// for it hears why, not a close.
    #[test]
    fn an_upgrade_request_too_long_fails_the_connect_with_why() {
        let server = crate::lib::network::TCP::server::Builder::new("ws-long", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .web_socket()
            .on_connect(Arc::new(move |_| {
                (
                    Box::new(Collect(Default::default(), None, Default::default())) as Box<dyn BytesDst>,
                    Box::new(Out(Default::default())) as Box<dyn BytesSrc>,
                )
            }))
            .build()
            .unwrap();
        let client = Client::new(
            "ws-long",
            super::super::MIN_BUFFER,
            Arc::new(move |_| {
                (
                    Box::new(Collect(Default::default(), None, Default::default())) as Box<dyn BytesDst>,
                    Box::new(Out(Default::default())) as Box<dyn BytesSrc>,
                )
            }),
        )
        .on_failure(Arc::new(|_: &dyn std::fmt::Display, _: &(dyn std::error::Error + 'static)| {}));
        let (tx, rx) = std::sync::mpsc::channel();
        let url = format!("ws://{}/{}", server.local_addr().unwrap(), "a".repeat(super::super::MIN_BUFFER));
        client
            .connect(&url, Some(Duration::from_secs(5)), move |r| {
                let _ = tx.send(r.map(|_| ()));
            })
            .unwrap();
        let e = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "the connect hears why: {e}");
        client.shutdown();
        server.shutdown();
    }

    /// A client connects by the address alone: the root of whoever answers there, its failures to
    /// the handler it was given, and its shutdown ends the engine of its own.
    #[test]
    fn a_client_connects_by_address() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let server = crate::lib::network::TCP::server::Builder::new("ws-addr", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .web_socket()
            .on_connect(Arc::new(move |_| {
                (
                    Box::new(Collect(g.clone(), None, Default::default())) as Box<dyn BytesDst>,
                    Box::new(Out(Default::default())) as Box<dyn BytesSrc>,
                )
            }))
            .build()
            .unwrap();
        let outbox = Arc::new(Mutex::new(b"by address".to_vec()));
        let ob = outbox.clone();
        let client = Client::new(
            "ws-addr",
            super::super::MIN_BUFFER,
            Arc::new(move |_| {
                (
                    Box::new(Collect(Default::default(), None, Default::default())) as Box<dyn BytesDst>,
                    Box::new(Out(ob.clone())) as Box<dyn BytesSrc>,
                )
            }),
        )
        .on_failure(Arc::new(|_: &dyn std::fmt::Display, _: &(dyn std::error::Error + 'static)| {}));
        assert!(!client.engine().is_shared());
        let (tx, rx) = std::sync::mpsc::channel();
        client
            .connect_addr(server.local_addr().unwrap(), Some(Duration::from_secs(5)), move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        let conn = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        conn.wake_transmit();
        for _ in 0..400 {
            if got.lock().unwrap().len() == 10 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(&got.lock().unwrap()[..], b"by address");
        client.shutdown();
        server.shutdown();
    }

    /// A client that stopped lets its engine, its pool and its connection go: its slot keeps it alive
    /// only till then ( `Core::give_up` ) - the two held each other for good before.
    #[test]
    fn a_stopped_client_lets_its_engine_go() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let (core, pool, engine, slot) = {
            let server = crate::lib::network::TCP::server::Builder::new("ws-gone", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .web_socket()
                .on_connect(Arc::new(move |_| {
                    (
                        Box::new(Collect(g.clone(), None, Default::default())) as Box<dyn BytesDst>,
                        Box::new(Out(Default::default())) as Box<dyn BytesSrc>,
                    )
                }))
                .build()
                .unwrap();
            let client = Client::new(
                "ws-gone",
                super::super::MIN_BUFFER,
                Arc::new(move |_| {
                    (
                        Box::new(Collect(Default::default(), None, Default::default())) as Box<dyn BytesDst>,
                        Box::new(Out(Arc::new(Mutex::new(b"gone".to_vec())))) as Box<dyn BytesSrc>,
                    )
                }),
            )
            .on_failure(Arc::new(|_: &dyn std::fmt::Display, _: &(dyn std::error::Error + 'static)| {}));
            let (tx, rx) = std::sync::mpsc::channel();
            client
                .connect_addr(server.local_addr().unwrap(), Some(Duration::from_secs(5)), move |r| {
                    let _ = tx.send(r);
                })
                .unwrap();
            let conn = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
            conn.wake_transmit();
            for _ in 0..400 {
                if got.lock().unwrap().len() == 4 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(&got.lock().unwrap()[..], b"gone");
            let watch = (
                Arc::downgrade(&client.core),
                Arc::downgrade(&client.core.pool),
                Arc::downgrade(&client.core.engine),
                Arc::downgrade(&conn),
            );
            client.shutdown();
            server.shutdown();
            watch
        };
        for _ in 0..1000 {
            if core.strong_count() == 0 && pool.strong_count() == 0 && engine.strong_count() == 0 && slot.strong_count() == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the client gone");
    }

    /// A close asked for with data queued: the data goes out first, then the one CLOSE, and nothing
    /// after it (RFC 6455 §5.5.1) - a CLOSE ahead of the data would lose it.
    #[test]
    fn the_data_queued_goes_before_the_close() {
        use std::io::{Read, Write};
        let seated = Arc::new(Mutex::new(None::<Arc<ExternalConnection>>));
        let outbox = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (st, ob) = (seated.clone(), outbox.clone());
        let server = crate::lib::network::TCP::server::Builder::new("ws-order", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .web_socket()
            .on_connect(Arc::new(move |state| {
                *st.lock().unwrap() = Some(state.clone());
                (
                    Box::new(Collect(Default::default(), None, Default::default())) as Box<dyn BytesDst>,
                    Box::new(Out(ob.clone())) as Box<dyn BytesSrc>,
                )
            }))
            .build()
            .unwrap();
        let mut c = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
            .unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        let mut b = [0u8; 4096];
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = c.read(&mut b).unwrap();
            assert!(0 < n, "no 101");
            got.extend_from_slice(&b[..n]);
        }
        got.drain(..got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4);
        let state = seated.lock().unwrap().clone().unwrap();
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i * 7) as u8).collect();
        outbox.lock().unwrap().extend_from_slice(&payload);
        let ws = super::super::WebSocket { state: state.clone() };
        assert!(ws.close_graceful(1001, "going away").is_ok()); // before the data is even woken
        state.wake_transmit();
        c.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        while let Ok(n) = c.read(&mut b) {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&b[..n]);
        }
        // the frames, as a server sends them: unmasked
        let (mut data, mut ops, mut at) = (Vec::new(), Vec::new(), 0usize);
        while at + 2 <= got.len() {
            let op = got[at] & 0x0F;
            let mut len = (got[at + 1] & 0x7F) as usize;
            at += 2;
            if len == 126 {
                len = u16::from_be_bytes([got[at], got[at + 1]]) as usize;
                at += 2;
            } else if len == 127 {
                len = u64::from_be_bytes(got[at..at + 8].try_into().unwrap()) as usize;
                at += 8;
            }
            if op == 0x2 {
                data.extend_from_slice(&got[at..at + len]);
            }
            ops.push(op);
            at += len;
        }
        assert_eq!(at, got.len(), "whole frames");
        assert!(data == payload, "the data queued, all of it: {} of {}", data.len(), payload.len());
        assert_eq!(ops.iter().filter(|&&o| o == 0x8).count(), 1, "one CLOSE: {ops:?}");
        assert_eq!(*ops.last().unwrap(), 0x8, "the CLOSE last, nothing after it: {ops:?}");
        server.shutdown();
    }

    /// A server that sends a part of its 101 and falls silent: the connect still ends at its
    /// timeout - the bytes of the 101 that came do not stop the wait for the rest.
    #[test]
    fn a_part_of_the_101_does_not_stop_the_connect_timeout() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut b = [0u8; 1024];
            let _ = c.read(&mut b); // the upgrade request
            c.write_all(b"HTTP/1.1 101 Swi").unwrap(); // a part of the answer, and nothing more
            std::thread::sleep(Duration::from_secs(3));
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        connect(
            &format!("ws://127.0.0.1:{port}/"),
            Some(Duration::from_millis(300)),
            |_| {
                (
                    Box::new(Collect(Default::default(), None, Default::default())) as Box<dyn BytesDst>,
                    Box::new(Out(Default::default())) as Box<dyn BytesSrc>,
                )
            },
            move |r| {
                let _ = tx.send(r.map(|_| ()));
            },
        );
        let r = rx.recv_timeout(Duration::from_secs(2)).expect("the connect never ended");
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        server.join().unwrap();
    }

    /// A WebSocket echo through the transport's own framing, both ends: masked from the client,
    /// plain from the server, frames of every length form.
    #[test]
    fn echo_round_trip() {
        let got_server = Arc::new(Mutex::new(Vec::new()));
        let gs = got_server.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let server = crate::lib::network::TCP::server::Builder::new("ws-test", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .web_socket()
            .on_connect(Arc::new(move |state| {
                let echo = Arc::new(Mutex::new(Vec::new()));
                (
                    Box::new(Collect(gs.clone(), Some(state.clone()), echo.clone())) as Box<dyn BytesDst>,
                    Box::new(Out(echo)) as Box<dyn BytesSrc>,
                )
            }))
            .on_event(Arc::new(move |_, e| ev.lock().unwrap().push(e)))
            .build()
            .unwrap();
        let addr = server.local_addr().unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i * 13 + 7) as u8).collect();
        let outbox = Arc::new(Mutex::new(payload.clone()));
        let got_client = Arc::new(Mutex::new(Vec::new()));
        let gc = got_client.clone();
        let ob = outbox.clone();
        let client_events = Arc::new(Mutex::new(Vec::new()));
        let cev = client_events.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        connect(
            &format!("ws://127.0.0.1:{}/x", addr.port()),
            Some(Duration::from_secs(5)),
            move |st| {
                st.set_event_sink(Arc::new(move |_, e| cev.lock().unwrap().push(e)));
                (
                    Box::new(Collect(gc, None, Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesDst>,
                    Box::new(Out(ob)) as Box<dyn BytesSrc>,
                )
            },
            move |r| {
                let _ = tx.send(r);
            },
        );
        let conn = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        conn.wake_transmit();
        for _ in 0..500 {
            if got_client.lock().unwrap().len() == payload.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(*got_server.lock().unwrap() == payload, "server got {}", got_server.lock().unwrap().len());
        assert!(*got_client.lock().unwrap() == payload, "client got {}", got_client.lock().unwrap().len());
        conn.ping(Some(b"hi")).unwrap();
        for _ in 0..100 {
            if events.lock().unwrap().contains(&Event::WEBSOCKET_PING) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(events.lock().unwrap().contains(&Event::WEBSOCKET_REMOTE_CONNECTED));
        // the socket first, the 101 then: the actors start on the first
        let at = |v: &Vec<Event>, e: Event| v.iter().position(|x| *x == e);
        let (sv, cv) = (events.lock().unwrap().clone(), client_events.lock().unwrap().clone());
        assert!(
            at(&sv, Event::REMOTE_CONNECT).is_some() && at(&sv, Event::REMOTE_CONNECT) < at(&sv, Event::WEBSOCKET_REMOTE_CONNECTED),
            "server: {sv:?}"
        );
        assert!(
            at(&cv, Event::THIS_CONNECT).is_some() && at(&cv, Event::THIS_CONNECT) < at(&cv, Event::WEBSOCKET_THIS_CONNECT),
            "client: {cv:?}"
        );
        assert!(events.lock().unwrap().contains(&Event::WEBSOCKET_PING));
        for _ in 0..100 {
            if client_events.lock().unwrap().contains(&Event::WEBSOCKET_PONG) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            client_events.lock().unwrap().contains(&Event::WEBSOCKET_PONG),
            "the server answers the PING: {:?}",
            client_events.lock().unwrap()
        );
        conn.close().unwrap();
        for _ in 0..200 {
            if !conn.state.is_open.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!conn.state.is_open.load(std::sync::atomic::Ordering::Acquire), "the closing handshake closes the client");
        server.shutdown();
    }
}
