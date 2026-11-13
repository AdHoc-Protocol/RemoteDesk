// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Connection admission limits, socket options, and server counters.
//!
//! [`Admission`] enforces total live connections, a fixed-window accept rate, and a per-address
//! limit. IPv6 addresses share counts by /64 prefix. Rejection categories are counted without
//! logging an error for every rejected socket.
//!
//! [`SocketOptions`] applies TCP options through `socket2`. [`Census`] snapshots host counters for
//! monitoring.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

/// The fate of one accepted connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Take the connection.
    Allowed,
    /// The host is at `max_connections`.
    Refused,
    /// This address knocked more than `accept_rate` times in this `accept_window`.
    Throttled,
    /// This address already holds `max_per_address` connections.
    Crowded,
}

/// The key an address is counted under: an IPv4 address whole, an IPv6 address by its /64 prefix.
fn address_key(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(v4) => u32::from(v4) as u128 | 1 << 32,
        // an IPv4 client of a dual-stack listener is `::ffff:a.b.c.d`: it counts as the IPv4 it is
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => u32::from(v4) as u128 | 1 << 32,
            None => (u128::from(v6) >> 64) << 64 | 1,
        },
    }
}

/// Who knocked in this window and how often, and how many connections each address holds.
#[derive(Default)]
struct Gate {
    window_from: i64,
    knocks: HashMap<u128, usize>,
    held: HashMap<u128, usize>,
}

/// Admission policy + counters of one host.
pub struct Admission {
    // the limits, 0 - off: they change while the server runs
    max_connections: AtomicUsize,
    accept_rate: AtomicUsize,
    accept_window_ms: AtomicI64,
    max_per_address: AtomicUsize,

    /// Turned away at `max_connections`.
    pub refused: AtomicU64,
    /// Turned away at `accept_rate` - one address knocking too often.
    pub throttled: AtomicU64,
    /// Turned away at `max_per_address` - one address holding too many at once.
    pub crowded: AtomicU64,

    gate: Mutex<Gate>,
}

impl Admission {
    pub fn new(max_connections: Option<usize>, accept_rate: Option<usize>, accept_window: Duration, max_per_address: Option<usize>) -> Self {
        Self {
            max_connections: AtomicUsize::new(max_connections.unwrap_or(0)),
            accept_rate: AtomicUsize::new(accept_rate.unwrap_or(0)),
            accept_window_ms: AtomicI64::new(accept_window.as_millis() as i64),
            max_per_address: AtomicUsize::new(max_per_address.unwrap_or(0)),
            refused: AtomicU64::new(0),
            throttled: AtomicU64::new(0),
            crowded: AtomicU64::new(0),
            gate: Mutex::new(Gate::default()),
        }
    }

    /// No limits: every connection is admitted (the default).
    pub fn unlimited() -> Self {
        Self::new(None, None, Duration::from_secs(1), None)
    }

    /// Decide the fate of one accepted connection. `live` is the host's current connection count.
    pub fn admit(&self, peer: SocketAddr, live: usize) -> Admit {
        self.admit_counted(peer, live).0
    }

    /// As [`admit`](Self::admit), and whether the peer was counted under `max_per_address`: a
    /// counted one is given back with [`connection_closed`](Self::connection_closed) at its
    /// teardown.
    pub fn admit_counted(&self, peer: SocketAddr, live: usize) -> (Admit, bool) {
        let max = self.max_connections.load(Ordering::Relaxed);
        if 0 < max && max <= live {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return (Admit::Refused, false);
        }
        let (rate, per_address) = (self.accept_rate.load(Ordering::Relaxed), self.max_per_address.load(Ordering::Relaxed));
        if rate == 0 && per_address == 0 {
            return (Admit::Allowed, false); // both limits off: no address looked up, no map touched
        }
        let who = address_key(peer.ip());
        let mut g = self.gate.lock();
        if 0 < rate {
            let now = super::TCP::external_connection::now_ms();
            if g.window_from + self.accept_window_ms.load(Ordering::Relaxed).max(1) <= now {
                g.knocks.clear(); // the window turned: every address starts again
                g.window_from = now;
            }
            let knocked = g.knocks.entry(who).or_insert(0);
            *knocked += 1;
            if rate < *knocked {
                self.throttled.fetch_add(1, Ordering::Relaxed);
                return (Admit::Throttled, false);
            }
        }
        if 0 < per_address {
            let max = per_address;
            let holds = g.held.entry(who).or_insert(0);
            if max <= *holds {
                self.crowded.fetch_add(1, Ordering::Relaxed);
                return (Admit::Crowded, false);
            }
            *holds += 1;
        }
        (Admit::Allowed, 0 < per_address)
    }

    /// The admitted connection of `peer` is over.
    pub fn connection_closed(&self, peer: SocketAddr) {
        {
            let who = address_key(peer.ip());
            let mut g = self.gate.lock();
            if let Some(holds) = g.held.get_mut(&who) {
                *holds -= 1;
                if *holds == 0 {
                    g.held.remove(&who); // holds nothing: out of the map, so that it does not grow for ever
                }
            }
        }
    }

    /// At most this many connections at once; 0 - no limit. From now on.
    pub fn set_max_connections(&self, max: usize) {
        self.max_connections.store(max, Ordering::Relaxed);
    }

    pub fn max_connections(&self) -> usize {
        self.max_connections.load(Ordering::Relaxed)
    }

    /// At most `rate` connects of one address in `window`; 0 - no limit.
    pub fn set_accept_rate(&self, rate: usize, window: Duration) {
        self.accept_window_ms.store(window.as_millis() as i64, Ordering::Relaxed);
        self.accept_rate.store(rate, Ordering::Relaxed);
    }

    pub fn accept_rate(&self) -> (usize, Duration) {
        (self.accept_rate.load(Ordering::Relaxed), Duration::from_millis(self.accept_window_ms.load(Ordering::Relaxed) as u64))
    }

    /// At most this many connections of one address at once; 0 - no limit. The connections admitted
    /// while it was off are not counted.
    pub fn set_max_per_address(&self, max: usize) {
        self.max_per_address.store(max, Ordering::Relaxed);
    }

    pub fn max_per_address(&self) -> usize {
        self.max_per_address.load(Ordering::Relaxed)
    }

    pub fn crowded(&self) -> u64 {
        self.crowded.load(Ordering::Relaxed)
    }
    pub fn throttled(&self) -> u64 {
        self.throttled.load(Ordering::Relaxed)
    }
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }
}

// Socket options

/// Socket tuning of accepted/connected streams.
#[derive(Debug, Clone, Copy)]
pub struct SocketOptions {
    /// Enables TCP_NODELAY when true. Defaults to true.
    pub no_delay: bool,
    /// `SO_KEEPALIVE`, and the idle time before the first probe (`TCP_KEEPIDLE`); `Duration::ZERO`
    /// - `SO_KEEPALIVE` alone, the system's idle time; `None` - off.
    pub keep_alive: Option<Duration>,
    /// `socket_receive_buffer`; `None` — OS default.
    pub socket_receive_buffer: Option<usize>,
    /// `socket_transmit_buffer`; `None` — OS default.
    pub socket_transmit_buffer: Option<usize>,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            no_delay: true,
            keep_alive: None,
            socket_receive_buffer: None,
            socket_transmit_buffer: None,
        }
    }
}

impl SocketOptions {
    /// Applies socket options to `stream`. Reports each failed operation through `failed` and
    /// continues applying the remaining options. An unsupported option does not close the stream.
    pub fn apply_mio(&self, stream: &mio::net::TcpStream, failed: impl Fn(&std::io::Error)) {
        let check = |r: std::io::Result<()>| {
            if let Err(e) = r {
                failed(&e);
            }
        };
        check(stream.set_nodelay(self.no_delay));
        let sock = socket2::SockRef::from(stream);
        if let Some(idle) = self.keep_alive {
            check(sock.set_keepalive(true));
            if !idle.is_zero() {
                check(sock.set_tcp_keepalive(&socket2::TcpKeepalive::new().with_time(idle)));
            }
        }
        if let Some(size) = self.socket_receive_buffer {
            check(sock.set_recv_buffer_size(size));
        }
        if let Some(size) = self.socket_transmit_buffer {
            check(sock.set_send_buffer_size(size));
        }
    }

    /// A socket to `addr`, connecting: its buffer sizes are set before the connect - the window
    /// scaling of the handshake is settled by then -, the rest once it is a stream. An option the
    /// system refuses goes to `failed`, the rest are still set.
    pub fn connect_mio(&self, addr: std::net::SocketAddr, failed: impl Fn(&std::io::Error)) -> std::io::Result<mio::net::TcpStream> {
        let check = |r: std::io::Result<()>| {
            if let Err(e) = r {
                failed(&e);
            }
        };
        let sock = socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
        sock.set_nonblocking(true)?;
        if let Some(size) = self.socket_receive_buffer {
            check(sock.set_recv_buffer_size(size));
        }
        if let Some(size) = self.socket_transmit_buffer {
            check(sock.set_send_buffer_size(size));
        }
        // all before the connect: on Windows an option set while a non-blocking connect is under way fails
        // with WSAEINVAL, on a remote host - a loopback connect is over at once and does not show it
        check(sock.set_tcp_nodelay(self.no_delay));
        if let Some(idle) = self.keep_alive {
            check(sock.set_keepalive(true));
            if !idle.is_zero() {
                check(sock.set_tcp_keepalive(&socket2::TcpKeepalive::new().with_time(idle)));
            }
        }
        match sock.connect(&addr.into()) {
            Ok(()) => {}
            Err(e) if in_progress(&e) => {} // on its way
            Err(e) => return Err(e),
        }
        Ok(mio::net::TcpStream::from_std(sock.into()))
    }
}

/// A non-blocking connect under way: `EINPROGRESS` on unix, `WSAEWOULDBLOCK` on Windows
/// (`ErrorKind::InProgress` is not stable).
#[inline]
fn in_progress(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EINPROGRESS)
    }
    #[cfg(not(unix))]
    {
        e.kind() == std::io::ErrorKind::WouldBlock
    }
}

// Census

/// A snapshot of the host's counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct Census {
    /// Connections live right now (`connections_live`).
    pub live: usize,
    /// Sessions parked, awaiting reclaim (`parked`).
    pub parked: usize,
    /// Turned away at `max_connections`, lifetime.
    pub refused: u64,
    /// Turned away at `accept_rate`, lifetime.
    pub throttled: u64,
    /// Turned away at `max_per_address`, lifetime.
    pub crowded: u64,
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port)
    }

    /// The limits change while the server runs; a connection admitted while the per-address limit
    /// was off is not counted, so its close gives back nothing of the ones counted since.
    #[test]
    fn limits_change_on_the_fly() {
        let a = Admission::unlimited();
        assert_eq!(a.admit_counted(peer(1), 0), (Admit::Allowed, false));
        a.set_max_per_address(1);
        assert_eq!(a.admit_counted(peer(2), 1), (Admit::Allowed, true));
        assert_eq!(a.admit_counted(peer(3), 2), (Admit::Crowded, false));
        a.set_max_connections(2);
        assert_eq!(a.admit_counted(peer(4), 2), (Admit::Refused, false));
        a.set_max_connections(0);
        a.connection_closed(peer(2)); // the counted one goes
        assert_eq!(a.admit_counted(peer(5), 1), (Admit::Allowed, true));
        a.set_max_per_address(0);
        assert_eq!(a.admit_counted(peer(6), 2), (Admit::Allowed, false));
    }

    #[test]
    fn unlimited_admits_everything() {
        let a = Admission::unlimited();
        for i in 0..100u16 {
            assert_eq!(a.admit(peer(i), 0), Admit::Allowed);
            a.connection_closed(peer(i));
        }
    }

    #[test]
    fn max_connections_refuses() {
        let a = Admission::new(Some(2), None, Duration::from_secs(1), None);
        assert_eq!(a.admit(peer(1), 0), Admit::Allowed);
        assert_eq!(a.admit(peer(2), 1), Admit::Allowed);
        assert_eq!(a.admit(peer(3), 2), Admit::Refused);
        assert_eq!(a.refused(), 1);
    }

    #[test]
    fn per_address_crowds_and_recovers() {
        let a = Admission::new(None, None, Duration::from_secs(1), Some(1));
        let p = peer(1000);
        assert_eq!(a.admit(p, 0), Admit::Allowed);
        assert_eq!(a.admit(peer(1001), 1), Admit::Crowded, "same address, any port");
        assert_eq!(a.crowded(), 1);
        a.connection_closed(p);
        assert_eq!(a.admit(peer(1002), 0), Admit::Allowed, "the slot freed");
    }

    #[test]
    fn ipv6_counted_by_prefix() {
        let a = Admission::new(None, None, Duration::from_secs(1), Some(1));
        let p1 = SocketAddr::new(IpAddr::from([0x2001u16, 1, 0, 0, 0, 0, 0, 1]), 1);
        let p2 = SocketAddr::new(IpAddr::from([0x2001u16, 1, 0, 0, 0, 0, 0, 2]), 2);
        assert_eq!(a.admit(p1, 0), Admit::Allowed);
        assert_eq!(a.admit(p2, 1), Admit::Crowded, "same /64, different IID");
    }

    #[test]
    fn ipv4_mapped_counts_as_its_ipv4() {
        let a = Admission::new(None, None, Duration::from_secs(1), Some(1));
        let mapped = SocketAddr::new(IpAddr::V6(std::net::Ipv4Addr::new(10, 0, 0, 7).to_ipv6_mapped()), 1);
        let plain = SocketAddr::new(IpAddr::from([10, 0, 0, 7]), 2);
        let other = SocketAddr::new(IpAddr::V6(std::net::Ipv4Addr::new(10, 0, 0, 8).to_ipv6_mapped()), 3);
        assert_eq!(a.admit(mapped, 0), Admit::Allowed);
        assert_eq!(a.admit(plain, 1), Admit::Crowded, "the same IPv4, mapped or not");
        assert_eq!(a.admit(other, 1), Admit::Allowed, "another IPv4 on a dual-stack listener is another address");
    }

    #[test]
    fn accept_rate_window() {
        let a = Admission::new(None, Some(2), Duration::from_millis(50), None);
        assert_eq!(a.admit(peer(1), 0), Admit::Allowed);
        assert_eq!(a.admit(peer(2), 1), Admit::Allowed);
        assert_eq!(a.admit(peer(3), 2), Admit::Throttled);
        assert_eq!(a.throttled(), 1);
        let elsewhere = SocketAddr::new(IpAddr::from([10, 0, 0, 1]), 4);
        assert_eq!(a.admit(elsewhere, 2), Admit::Allowed, "the rate is of one address: another knocks on its own");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(a.admit(peer(4), 2), Admit::Allowed, "the window expired");
    }
}
