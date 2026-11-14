// The meeting through a meeting point of SYN Mirror: the machine behind its NAT, the viewer behind another.
//
// The machine asks the meeting point for a port of meeting - the password to its port of requests; the meeting point answers with
// a port nobody holds, given to the address of the machine - and shows the line to give the viewer: the meeting point, that port
// and the token. Until its user presses Enter, it holds the port with the password and one byte more, every second: the port stays
// its own and closed. Then it knocks at that port with the password ( one UDP packet ) and dials it, every second, from one source
// port; the meeting point drops those SYNs. The viewer dials the same port; the meeting point sends its SYN on to the machine, as if
// from itself: the two meet, and the server takes the connection as if it had accepted it. The machine goes on waiting at the same
// port, from a new source port: a viewer that lost its connection, another viewer, or the machine started again as administrator
// meets there again. All from one UDP socket: the port is given to the address the request came from.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::lib::network::TCP::Server;

/// How long one try waits: the meeting point drops its SYN until the viewer comes.
const TRY: Duration = Duration::from_secs(1);

/// The port of requests of a meeting point when none is given: the first port of the range SYN Mirror installs.
pub const ASKS: u16 = 40000;

/// What the machine meets its viewer with.
pub struct Meet {
    /// The meeting point: a name or an address.
    pub point:    String,
    /// The payload of the request for a port and of the knock.
    pub password: [u8; 8],
    /// The port of the meeting point that answers the requests for a port: the first of its range.
    pub asks:     u16,
}

impl Meet {
    /// The meeting point as its owner gives it - the page of SYN Mirror copies it so, in one line: `Tr7kQ2xZ@router.example.org:40000` -
    /// apart: its password, when it is there, and the rest. A password has no `@`, nor a name or an address.
    pub fn given(text: &str) -> (Option<&str>, &str) {
        let text = text.trim();
        match text.split_once('@') {
            Some((password, point)) => (Some(password), point),
            None => (None, text),
        }
    }

    /// The meeting point and its port of requests from `router.example.org` or `router.example.org:20000`, a password before
    /// them or not.
    pub fn point(text: &str) -> Option<(String, u16)> {
        let text = Self::given(text).1;
        let (point, asks) = match text.rsplit_once(':') {
            Some((point, port)) => (point, port.parse().ok().filter(|&p| p != 0)?),
            None => (text, ASKS),
        };
        (!point.is_empty() && !point.contains(':')).then(|| (point.to_string(), asks))
    }
}

/// The port of meeting once access is granted: the machine started again as administrator waits there at once.
static GRANTED: AtomicU16 = AtomicU16::new(0);

pub fn granted() -> Option<u16> {
    match GRANTED.load(Ordering::Relaxed) {
        0 => None,
        port => Some(port),
    }
}

/// Shows the line for the viewer, waits for Enter - unless access was granted before this start ( `port` ) - and meets the
/// viewers from then on.
pub fn start(meet: Meet, server: Server, port: Option<u16>, pass: String) {
    thread::Builder::new().name("meet".into()).spawn(move || run(meet, server, port, pass)).expect("a thread of the meeting");
}

fn run(meet: Meet, server: Server, port: Option<u16>, pass: String) {
    // one socket for the request and the knocks: the meeting point gives the port to the address they come from
    let udp = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        Ok(udp) => udp,
        Err(e) => return say!("❌", Bad, "The meeting: {e}"),
    };
    let asked = port.is_none();
    let port = match port {
        Some(port) => port, // given before this start, to this address: its knocks open it
        None => match ask(&udp, &meet) {
            Ok(port) => port,
            Err(e) => return say!("❌", Bad, "The meeting point {}: {e}", meet.point),
        },
    };
    // one line for the viewer: the address of the meeting and, after #, the token and the check of the key - the page takes it whole
    let line = format!("{}:{port}#{pass}", meet.point);
    crate::console::line_to_send(&line);
    if asked {
        say!("🔑", Wait, "Press Enter when the line has reached them: the meeting point lets them in from then on");
        let held = hold(&udp, &meet, port);
        let read = crate::console::read_line();
        held.store(true, Ordering::Relaxed);
        if read.is_none() {
            return; // no console: nobody to grant it
        }
    }
    GRANTED.store(port, Ordering::Relaxed);
    say!("🔓", Good, "Access granted");
    say!("👀", Wait, "Waiting for whoever connects…");
    loop {
        // a meeting point of a dynamic address: its name is looked up for each meeting
        let at = match ipv4(&meet.point) {
            Ok(router) => SocketAddrV4::new(router, port),
            Err(e) => {
                say!("❌", Bad, "The meeting point {}: {e}", meet.point);
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        match once(&udp, at, &meet.password) {
            Ok(stream) => {
                if let Err(e) = server.adopt(stream) {
                    say!("❌", Bad, "The meeting at {at}: {e}");
                }
            }
            Err(e) => {
                say!("❌", Bad, "The meeting at {at}: {e}");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Knocks at `at` and dials it until the viewer comes - from one source port, so the NAT of the machine keeps one mapping. Each try
/// knocks too: the meeting point keeps the port open while its machine tries.
fn once(udp: &UdpSocket, at: SocketAddrV4, password: &[u8; 8]) -> io::Result<TcpStream> {
    let to = SocketAddr::V4(at).into();
    let mut from = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
    loop {
        let _ = udp.send_to(password, at); // a lost knock shows as a refused try below
        let tcp = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        tcp.set_reuse_address(true)?;
        tcp.bind(&from.into())?;
        if from.port() == 0 {
            from = tcp.local_addr()?.as_socket().ok_or_else(|| io::Error::other("no local address"))?;
        }
        let started = Instant::now();
        match tcp.connect_timeout(&to, TRY) {
            Ok(()) => return Ok(tcp.into()),
            // refused at once: the port is not open yet, or the network is down - a pause, not a storm of tries
            Err(_) => {
                if let Some(rest) = TRY.checked_sub(started.elapsed()) {
                    thread::sleep(rest)
                }
            }
        }
    }
}

fn ipv4(host: &str) -> io::Result<Ipv4Addr> {
    (host, 0)
        .to_socket_addrs()?
        .find_map(|a| match a {
            SocketAddr::V4(a) => Some(*a.ip()),
            SocketAddr::V6(_) => None,
        })
        .ok_or_else(|| io::Error::other("no IPv4 address"))
}

/// A port of meeting from the meeting point: the password to its port of requests, the answer a port of 2 bytes, high first, given to the
/// address of `udp`. A request or an answer lost is asked again: the meeting point gives the same port to the same place.
fn ask(udp: &UdpSocket, meet: &Meet) -> io::Result<u16> {
    let at = SocketAddr::V4(SocketAddrV4::new(ipv4(&meet.point)?, meet.asks));
    let mut got = [0u8; 4];
    for _ in 0..10 {
        udp.send_to(&meet.password, at)?;
        let started = Instant::now();
        while let Some(left) = TRY.checked_sub(started.elapsed()).filter(|left| !left.is_zero()) {
            udp.set_read_timeout(Some(left))?;
            match udp.recv_from(&mut got) {
                Ok((2, from)) if from == at => return Ok(u16::from_be_bytes([got[0], got[1]])),
                Ok(_) => {} // not the answer
                // no answer in time, or refused at once: the request again after the rest of the second
                Err(_) => thread::sleep(TRY.saturating_sub(started.elapsed())),
            }
        }
    }
    Err(io::Error::other(format!("no port of meeting from {at}: a wrong password, a wrong first port, or this address is blocked for wrong knocks")))
}

/// Holds `port` with the password and one byte more, every second, until the flag returned is set: the port stays given to this
/// address, and closed.
fn hold(udp: &UdpSocket, meet: &Meet, port: u16) -> Arc<AtomicBool> {
    let done = Arc::new(AtomicBool::new(false));
    let (Ok(udp), Ok(router)) = (udp.try_clone(), ipv4(&meet.point)) else { return done };
    let mut payload = [0u8; 9];
    payload[..8].copy_from_slice(&meet.password);
    let stop = done.clone();
    let at = SocketAddrV4::new(router, port);
    let _ = thread::Builder::new().name("hold".into()).spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let _ = udp.send_to(&payload, at);
            thread::sleep(TRY);
        }
    });
    done
}
