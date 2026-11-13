// Way 4, one line: the router of the remote computer is asked to open a port of its own to the program - the way ② a person sets
// up by hand, done by the program. Three protocols, the first that works: PCP ( RFC 6887 ) and NAT-PMP ( RFC 6886 ), one datagram
// each to port 5351 of the gateway, and UPnP IGD ( `igd-next` ), the one most home routers speak.
//
// The port is opened for an hour and renewed every half of it: a program that stops without a word loses it within the hour. On
// Ctrl+C, or its window closed, the program closes it at once - also on a router that keeps ports forever.
//
// A port opened on a router that has no public address of its own - behind the NAT of the provider ( CGNAT ), or behind a second
// router - leads nowhere: then it is closed again, and the two lines of the exchange stay the way.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use igd_next::{PortMappingProtocol, SearchOptions};
use parking_lot::Mutex;

/// The port of PCP and NAT-PMP on the gateway.
const PORT: u16 = 5351;
/// How long the router keeps the port, asked: renewed every half of it.
const LEASE: u32 = 3600;
/// How long PCP and NAT-PMP have to answer: a gateway that runs neither mostly says so at once ( an ICMP of a closed port ).
const ANSWERING: Duration = Duration::from_millis(1500);
/// How long the router has to answer the search of UPnP.
const SEARCHING: Duration = Duration::from_secs(2);
/// What the router shows of the port, in its list of UPnP.
const NAME: &str = "RemoteDesk";

/// How the port was opened.
pub enum How {
    Pcp { gateway: SocketAddr, nonce: [u8; 12] },
    NatPmp { gateway: SocketAddr },
    Upnp(Box<igd_next::Gateway>),
}

impl How {
    /// Its name: the tests tell by it which protocol opened the port.
    #[cfg(test)]
    pub fn name(&self) -> &'static str {
        match self {
            How::Pcp { .. } => "PCP",
            How::NatPmp { .. } => "NAT-PMP",
            How::Upnp(_) => "UPnP",
        }
    }
}

/// A port of the router opened to this computer.
pub struct Opened {
    pub how:      How,
    /// Where the router passes the connections: this computer, the port of the program.
    pub internal: SocketAddrV4,
    /// Where the internet reaches it.
    pub external: SocketAddrV4,
    /// How long the router keeps it, as it said; 0 - for good.
    pub lifetime: u32,
}

/// Why no port was opened.
pub enum Refused {
    /// No router answered: none of the three protocols, or each said no.
    NoRouter,
    /// A port was opened, and closed again: the address of the router is not public, a NAT of the provider before it.
    NotPublic,
}

/// Asks the router of this computer to open a port to `port` of it, the same port outside if it can.
pub fn open(port: u16) -> Result<Opened, Refused> {
    // the search of UPnP takes longest: it runs while PCP and NAT-PMP are asked
    let searching = thread::spawn(|| igd_next::search_gateway(search()).ok());
    let gateway = netdev::get_default_gateway().ok().and_then(|g| g.ipv4.first().copied());
    let opened =
        gateway.and_then(|g| by_pcp_or_nat_pmp(SocketAddr::from((g, PORT)), port)).or_else(|| by_upnp(searching.join().ok()??, port));
    match opened {
        None => Err(Refused::NoRouter),
        Some(o) if public(*o.external.ip()) => Ok(o),
        Some(o) => {
            o.close();
            Err(Refused::NotPublic)
        }
    }
}

/// Whether `ip` is reached from the internet: not of a home network, not of the NAT of a provider ( 100.64.0.0/10 ).
pub fn public(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a >= 240
        || (a == 100 && b & 0xC0 == 64)
        || a == 0)
}

impl Opened {
    /// Asks for the port again, for another lifetime: the address outside, as the router says it now.
    pub fn renew(&mut self) -> io::Result<SocketAddrV4> {
        let (external, lifetime) = match &self.how {
            How::Pcp { gateway, nonce } => pcp(*gateway, self.internal, self.external.port(), LEASE, nonce)?,
            How::NatPmp { gateway } => nat_pmp(*gateway, self.internal.port(), self.external.port(), LEASE)?,
            How::Upnp(g) => {
                let lifetime = if self.lifetime == 0 { 0 } else { LEASE };
                g.add_port(PortMappingProtocol::TCP, self.external.port(), self.internal.into(), lifetime, NAME)
                    .map_err(io::Error::other)?;
                (self.external, lifetime)
            }
        };
        self.external = external;
        self.lifetime = lifetime;
        Ok(external)
    }

    /// Closes the port: best effort, the router may be gone.
    pub fn close(&self) {
        let _ = match &self.how {
            How::Pcp { gateway, nonce } => pcp(*gateway, self.internal, 0, 0, nonce).map(drop),
            How::NatPmp { gateway } => nat_pmp(*gateway, self.internal.port(), 0, 0).map(drop),
            How::Upnp(g) => g.remove_port(PortMappingProtocol::TCP, self.external.port()).map_err(io::Error::other),
        };
    }

    /// Keeps the port while the program runs: renews it every half of its lifetime, calls `moved` with a new address outside,
    /// and closes it when the program is closed.
    pub fn keep(self, moved: impl Fn(SocketAddrV4) + Send + 'static) {
        let lifetime = self.lifetime;
        let opened = Arc::new(Mutex::new(Some(self)));
        let closing = opened.clone();
        let _ = ctrlc::set_handler(move || {
            if let Some(o) = closing.lock().take() {
                o.close();
            }
            std::process::exit(0);
        });
        if lifetime == 0 {
            return; // for good: nothing to renew
        }
        let _ = thread::Builder::new().name("port of the router".into()).spawn(move || {
            let mut wait = Duration::from_secs(u64::from(lifetime / 2).max(60));
            loop {
                thread::sleep(wait);
                let mut held = opened.lock();
                let Some(o) = held.as_mut() else { return };
                let before = o.external;
                match o.renew() {
                    Ok(now) => {
                        wait = Duration::from_secs(u64::from(o.lifetime / 2).max(60));
                        if now != before {
                            moved(now);
                        }
                    }
                    Err(_) => wait = Duration::from_secs(60), // the router is away a moment: again soon, the port lives on meanwhile
                }
            }
        });
    }
}

// ── PCP and NAT-PMP ──

/// PCP first; a gateway that answers in the version of NAT-PMP speaks NAT-PMP. One that answers nothing speaks neither.
fn by_pcp_or_nat_pmp(gateway: SocketAddr, port: u16) -> Option<Opened> {
    let here = toward(gateway)?;
    let internal = SocketAddrV4::new(here, port);
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).ok()?;
    match pcp(gateway, internal, port, LEASE, &nonce) {
        Ok((external, lifetime)) => Some(Opened { how: How::Pcp { gateway, nonce }, internal, external, lifetime }),
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            let (external, lifetime) = nat_pmp(gateway, port, port, LEASE).ok()?;
            Some(Opened { how: How::NatPmp { gateway }, internal, external, lifetime })
        }
        Err(_) => None,
    }
}

/// The address of this computer toward `to`: the one its packets go out from.
fn toward(to: SocketAddr) -> Option<Ipv4Addr> {
    let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    udp.connect(to).ok()?;
    match udp.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Sends `request` to `gateway` - again after 250 and 750 ms - and gives the first answer `fits`, within `ANSWERING`.
fn exchange(gateway: SocketAddr, request: &[u8], fits: impl Fn(&[u8]) -> bool) -> io::Result<Vec<u8>> {
    let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    udp.connect(gateway)?;
    let started = Instant::now();
    let mut resend = [Duration::ZERO, Duration::from_millis(250), Duration::from_millis(750)].into_iter().peekable();
    let mut b = [0u8; 1100];
    while started.elapsed() < ANSWERING {
        if resend.next_if(|at| started.elapsed() >= *at).is_some() {
            udp.send(request)?;
        }
        udp.set_read_timeout(Some(Duration::from_millis(50)))?;
        match udp.recv(&mut b) {
            Ok(n) if fits(&b[..n]) => return Ok(b[..n].to_vec()),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e), // ConnectionReset, ConnectionRefused: nothing at that port
        }
    }
    Err(io::ErrorKind::TimedOut.into())
}

/// A MAP of PCP for TCP: `internal` to `external` outside ( 0 - any ), for `lifetime` seconds ( 0 - closes it ). The address and
/// port outside, and the lifetime the gateway gave; `Unsupported` - the gateway speaks NAT-PMP.
fn pcp(gateway: SocketAddr, internal: SocketAddrV4, external: u16, lifetime: u32, nonce: &[u8; 12]) -> io::Result<(SocketAddrV4, u32)> {
    let mut r = [0u8; 60];
    r[0] = 2; // the version
    r[1] = 1; // MAP, a request
    r[4..8].copy_from_slice(&lifetime.to_be_bytes());
    r[18..20].copy_from_slice(&[0xFF, 0xFF]); // the address of the client, IPv4 in IPv6
    r[20..24].copy_from_slice(&internal.ip().octets());
    r[24..36].copy_from_slice(nonce);
    r[36] = 6; // TCP
    r[40..42].copy_from_slice(&internal.port().to_be_bytes());
    r[42..44].copy_from_slice(&external.to_be_bytes());
    r[54..56].copy_from_slice(&[0xFF, 0xFF]); // the address outside: any IPv4
    let a = exchange(gateway, &r, |a| a.len() >= 2 && (a[0] == 0 || (a.len() >= 60 && a[0] == 2 && a[1] == 0x81 && a[24..36] == *nonce)))?;
    if a[0] == 0 {
        return Err(io::ErrorKind::Unsupported.into()); // the version of NAT-PMP: it speaks that one
    }
    if a[3] != 0 {
        return Err(io::Error::other(format!("PCP: result {}", a[3])));
    }
    let lifetime = u32::from_be_bytes([a[4], a[5], a[6], a[7]]);
    let port = u16::from_be_bytes([a[42], a[43]]);
    Ok((SocketAddrV4::new(Ipv4Addr::new(a[56], a[57], a[58], a[59]), port), lifetime))
}

/// A mapping of NAT-PMP for TCP, as of `pcp`, then the address of the gateway outside - NAT-PMP gives it apart.
fn nat_pmp(gateway: SocketAddr, internal: u16, external: u16, lifetime: u32) -> io::Result<(SocketAddrV4, u32)> {
    let mut r = [0u8; 12];
    r[1] = 2; // map TCP
    r[4..6].copy_from_slice(&internal.to_be_bytes());
    r[6..8].copy_from_slice(&external.to_be_bytes());
    r[8..12].copy_from_slice(&lifetime.to_be_bytes());
    let a = exchange(gateway, &r, |a| a.len() >= 16 && a[0] == 0 && a[1] == 130 && a[8..10] == internal.to_be_bytes())?;
    if a[2..4] != [0, 0] {
        return Err(io::Error::other(format!("NAT-PMP: result {}", u16::from_be_bytes([a[2], a[3]]))));
    }
    let port = u16::from_be_bytes([a[10], a[11]]);
    let given = u32::from_be_bytes([a[12], a[13], a[14], a[15]]);
    if lifetime == 0 {
        return Ok((SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port), 0));
    }
    let b = exchange(gateway, &[0, 0], |b| b.len() >= 12 && b[0] == 0 && b[1] == 128)?;
    if b[2..4] != [0, 0] {
        return Err(io::Error::other(format!("NAT-PMP: result {}", u16::from_be_bytes([b[2], b[3]]))));
    }
    Ok((SocketAddrV4::new(Ipv4Addr::new(b[8], b[9], b[10], b[11]), port), given))
}

// ── UPnP ──

/// How the router is searched for: IPv4, `SEARCHING` at most.
fn search() -> SearchOptions {
    let mut o = SearchOptions::default();
    o.timeout = Some(SEARCHING);
    o.single_search_timeout = Some(SEARCHING);
    o.gateway_ip_version = igd_next::GatewayIpVersion::V4;
    o
}

/// The router `gateway`, found by SSDP: the same port outside, else any; for an hour, or for good where it keeps ports only so.
fn by_upnp(gateway: igd_next::Gateway, port: u16) -> Option<Opened> {
    let internal = SocketAddrV4::new(toward(gateway.addr)?, port);
    let IpAddr::V4(outside) = gateway.get_external_ip().ok()? else {
        return None;
    };
    let mut lifetime = LEASE;
    let add = |lifetime: u32| gateway.add_port(PortMappingProtocol::TCP, port, internal.into(), lifetime, NAME);
    let external = match add(lifetime) {
        Ok(()) => port,
        Err(igd_next::AddPortError::OnlyPermanentLeasesSupported) => {
            lifetime = 0;
            add(0).ok()?;
            port
        }
        Err(_) => gateway.add_any_port(PortMappingProtocol::TCP, internal.into(), lifetime, NAME).ok()?, // the port is taken outside
    };
    Some(Opened { how: How::Upnp(Box::new(gateway)), internal, external: SocketAddrV4::new(outside, external), lifetime })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gateway on 127.0.0.1: `answer` makes the reply to each request.
    fn gateway(answer: impl Fn(&[u8]) -> Vec<u8> + Send + 'static) -> SocketAddr {
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let at = udp.local_addr().unwrap();
        thread::spawn(move || {
            let mut b = [0u8; 1100];
            while let Ok((n, from)) = udp.recv_from(&mut b) {
                let _ = udp.send_to(&answer(&b[..n]), from);
            }
        });
        at
    }

    #[test]
    fn a_port_by_pcp() {
        let gw = gateway(|r| {
            assert_eq!((r.len(), r[0], r[1], r[36]), (60, 2, 1, 6));
            let mut a = r.to_vec();
            a[1] = 0x81;
            a[3] = 0; // success
            a[4..8].copy_from_slice(&1800u32.to_be_bytes());
            a[42..44].copy_from_slice(&41000u16.to_be_bytes());
            a[44..60].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 198, 51, 100, 9]);
            a
        });
        let o = by_pcp_or_nat_pmp(gw, 8765).unwrap();
        assert_eq!(o.how.name(), "PCP");
        assert_eq!((o.external, o.lifetime, o.internal.port()), ("198.51.100.9:41000".parse().unwrap(), 1800, 8765));
    }

    #[test]
    fn a_port_by_nat_pmp_after_pcp() {
        let gw = gateway(|r| match (r.len(), r[0], r[1]) {
            (60, 2, _) => vec![0, 0x81, 0, 1], // a gateway of NAT-PMP: the version of PCP is not its own
            (12, 0, 2) => {
                let mut a = vec![0, 130, 0, 0, 0, 0, 0, 7];
                a.extend_from_slice(&r[4..6]); // the internal port
                a.extend_from_slice(&40999u16.to_be_bytes());
                a.extend_from_slice(&3600u32.to_be_bytes());
                a
            }
            (2, 0, 0) => vec![0, 128, 0, 0, 0, 0, 0, 7, 198, 51, 100, 10],
            _ => panic!("{r:?}"),
        });
        let o = by_pcp_or_nat_pmp(gw, 8765).unwrap();
        assert_eq!(o.how.name(), "NAT-PMP");
        assert_eq!((o.external, o.lifetime), ("198.51.100.10:40999".parse().unwrap(), 3600));
    }

    #[test]
    fn no_gateway_at_the_port() {
        let closed = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap(); // bound, then dropped: closed
        let started = Instant::now();
        assert!(by_pcp_or_nat_pmp(closed, 8765).is_none());
        assert!(started.elapsed() <= ANSWERING + Duration::from_millis(200));
    }

    #[test]
    fn public_addresses() {
        assert!(public(Ipv4Addr::new(123, 120, 50, 115)));
        for ip in [
            [192, 168, 1, 1],
            [10, 1, 2, 3],
            [172, 16, 0, 1],
            [100, 64, 0, 1],
            [100, 127, 255, 1],
            [169, 254, 1, 1],
            [127, 0, 0, 1],
            [0, 0, 0, 0],
        ] {
            assert!(!public(Ipv4Addr::from(ip)), "{ip:?}");
        }
        assert!(public(Ipv4Addr::new(100, 128, 0, 1)));
    }

    /// The router of this computer, read only: what it answers, nothing opened. `cargo test -- --ignored router_here --nocapture`
    #[test]
    #[ignore]
    fn router_here() {
        let gw = netdev::get_default_gateway().ok().and_then(|g| g.ipv4.first().copied());
        println!("gateway {gw:?}");
        if let Some(g) = gw {
            let at = SocketAddr::from((g, PORT));
            println!("NAT-PMP, the address outside: {:?}", exchange(at, &[0, 0], |b| b.len() >= 12 && b[0] == 0 && b[1] == 128));
        }
        let options = search();
        match igd_next::search_gateway(options) {
            Ok(g) => println!("UPnP {} at {}, the address outside {:?}", g.service_type, g.addr, g.get_external_ip()),
            Err(e) => println!("UPnP: {e}"),
        }
    }
}
