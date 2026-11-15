// The meeting through a meeting point of SYN Mirror: the machine behind its NAT, the viewer behind another.
//
// The machine asks the meeting point for a port of meeting - the password and its own 8 random bytes, its id, to the port of
// requests; the meeting point answers with a port nobody holds, given to that id - and shows the line to give the viewer: the
// meeting point, that port and the token. At once it knocks at that port with the password and the id ( one UDP packet ) and dials
// it, every second, from one source port; the meeting point drops those SYNs. The viewer dials the same port; the meeting point
// sends its SYN on to the machine, as if from itself: the two meet, and the server takes the connection as if it had accepted it.
// The machine goes on waiting at the same port, from a new source port: a viewer that lost its connection, another viewer, or the
// machine started again as administrator meets there again. The port is given to the id, not to an address: a NAT that gives the
// machine another address, or another port for each try, keeps it.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::OnceLock;
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

/// The port of meeting and the id it is given to: the machine started again as administrator knocks there at once, as `--meet-given`.
static GIVEN: OnceLock<(u16, [u8; 8])> = OnceLock::new();

/// `--meet-given` of the machine started again: the port, `.`, the id in 16 hex digits.
pub fn given() -> Option<String> {
    GIVEN.get().map(|(port, id)| format!("{port}.{}", id.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// `--meet-given` back: the port and the id.
pub fn parse_given(text: &str) -> Option<(u16, [u8; 8])> {
    let (port, id) = text.split_once('.')?;
    let id = u64::from_str_radix(id, 16).ok().filter(|_| id.len() == 16)?;
    Some((port.parse().ok().filter(|&p| p != 0)?, id.to_be_bytes()))
}

/// Shows the line for the viewer and meets the viewers from then on: at the port given before this start, when there is one.
pub fn start(meet: Meet, server: Server, given: Option<(u16, [u8; 8])>, pass: String) {
    thread::Builder::new()
        .name("meet".into())
        .spawn(move || {
            run(meet, given, |point, port| {
                // one line for the viewer: the address of the meeting and, after /, the token and the check of the key - the page takes it whole
                crate::console::line_to_send(&format!("{point}:{port}/{pass}"));
            }, |stream, at| {
                if let Err(e) = server.adopt(stream) {
                    say!("❌", Bad, "The meeting at {at}: {e}");
                }
            })
        })
        .expect("a thread of the meeting");
}

/// The port of Remote Desktop of Windows: what the line of a relay to it is for.
pub const MSTSC: u16 = 3389;

/// The relay: SYN Mirror as the way to another server of this computer, or of its network - `to`, such as `127.0.0.1:3389`, Remote
/// Desktop of Windows. Whoever dials the port of meeting, with its own program - Remote Desktop Connection, ssh, a VNC viewer -, is
/// joined to `to`: the bytes go as they are, both ways, the server there checks who comes. Runs as long as the program.
pub fn relay(meet: Meet, to: String) {
    let rdp = to.rsplit_once(':').is_some_and(|(_, p)| p == MSTSC.to_string());
    // nothing there to reach: said at once, the meetings wait for it anyway
    match to.to_socket_addrs().map(|mut a| a.next()) {
        Ok(Some(at)) if TcpStream::connect_timeout(&at, Duration::from_secs(2)).is_ok() => say!("🔌", Good, "{to} answers: whoever comes is joined to it"),
        _ if rdp => say!("⚠️", Warn, "Nothing answers at {to}: Remote Desktop is off on this computer. Windows Pro or above: Settings > System > Remote Desktop, on. Windows Home has none"),
        _ => say!("⚠️", Warn, "Nothing answers at {to} yet: whoever comes waits for it"),
    }
    if rdp {
        say!("🔑", Warn, "Whoever finds the port will try passwords: programs of the internet guess at every Remote Desktop they find, day and night");
        say!("🔑", Warn, "The account you log in with needs a long password of its own, 16 characters or more, used nowhere else; not Administrator, admin or user");
    }
    run(meet, None, |point, port| {
        let line = format!("{point}:{port}");
        if rdp {
            say!("🖥️", News, "Remote Desktop Connection ( mstsc ) of another computer reaches this one at it: as the computer, type");
        } else {
            say!("🔗", News, "A program of another computer reaches {to} at it: as the address and the port, type");
        }
        crate::console::line_to_send(&line);
    }, |stream, at| {
        let to = to.clone();
        let _ = thread::Builder::new().name("relay".into()).spawn(move || join(stream, &to, at));
    });
}

/// `came` - a connection that met through the meeting point - joined to `to` till either side ends.
fn join(came: TcpStream, to: &str, at: SocketAddrV4) {
    let there = match to.to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| io::Error::other("no such address")).and_then(|a| TcpStream::connect_timeout(&a, Duration::from_secs(5))) {
        Ok(there) => there,
        Err(e) => return say!("❌", Bad, "Someone came through {at}; {to}: {e}"),
    };
    say!("✅", Good, "Someone came through {at}: joined to {to}");
    let _ = came.set_nodelay(true);
    let _ = there.set_nodelay(true);
    let (Ok(mut came_in), Ok(mut there_out)) = (came.try_clone(), there.try_clone()) else { return };
    let (mut came_out, mut there_in) = (came, there);
    // each way on a thread of its own: its end is told the other side, which ends in its turn
    let back = thread::spawn(move || {
        let n = io::copy(&mut there_in, &mut came_out).unwrap_or(0);
        let _ = came_out.shutdown(std::net::Shutdown::Write);
        n
    });
    let sent = io::copy(&mut came_in, &mut there_out).unwrap_or(0);
    let _ = there_out.shutdown(std::net::Shutdown::Write);
    let got = back.join().unwrap_or(0);
    say!("👋", Quiet, "Gone, the one through {at}: {} in, {} out", bytes(sent), bytes(got));
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// The meeting, for whatever then takes the connections: the port asked for - or the one `given` before this start -, `show` told
/// the meeting point and the port, and from then on each connection that meets goes to `took`, the next awaited at once.
fn run(meet: Meet, given: Option<(u16, [u8; 8])>, show: impl FnOnce(&str, u16), mut took: impl FnMut(TcpStream, SocketAddrV4)) {
    // one socket for the request and the knocks, from one source port: the NAT of the machine keeps one mapping for them
    let udp = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        Ok(udp) => udp,
        Err(e) => return say!("❌", Bad, "The meeting: {e}"),
    };
    let id = given.map(|(_, id)| id).unwrap_or_else(|| {
        let mut id = [0u8; 8];
        getrandom::fill(&mut id).expect("the random numbers of the system");
        id
    });
    // the password and the id: the request for the port, and each knock at it
    let mut knock = [0u8; 16];
    knock[..8].copy_from_slice(&meet.password);
    knock[8..].copy_from_slice(&id);
    let port = match given {
        Some((port, _)) => port, // given before this start, to this id: its knocks open it
        None => match ask(&udp, &meet, &knock) {
            Ok(port) => port,
            Err(e) => return say!("❌", Bad, "The meeting point {}: {e}", meet.point),
        },
    };
    let _ = GIVEN.set((port, id));
    show(&meet.point, port);
    say!("👀", Wait, "Waiting for whoever connects…");
    crate::console::stay_awake();
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
        match once(&udp, at, &knock) {
            Ok(stream) => took(stream, at),
            Err(e) => {
                say!("❌", Bad, "The meeting at {at}: {e}");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Knocks at `at` and dials it until the viewer comes - from one source port, so the NAT of the machine keeps one mapping. Each try
/// knocks too - the password and the id: the meeting point keeps the port open while its machine tries.
fn once(udp: &UdpSocket, at: SocketAddrV4, knock: &[u8; 16]) -> io::Result<TcpStream> {
    let to = SocketAddr::V4(at).into();
    let mut from = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
    loop {
        let _ = udp.send_to(knock, at); // a lost knock shows as a refused try below
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

/// A port of meeting from the meeting point: the password and the id to its port of requests, the answer a port of 2 bytes, high
/// first, given to that id. A request or an answer lost is asked again: the meeting point gives the same port to the same id.
fn ask(udp: &UdpSocket, meet: &Meet, request: &[u8; 16]) -> io::Result<u16> {
    let at = SocketAddr::V4(SocketAddrV4::new(ipv4(&meet.point)?, meet.asks));
    let mut got = [0u8; 4];
    for _ in 0..10 {
        udp.send_to(request, at)?;
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
    Err(io::Error::other(format!("no port of meeting from {at}: a wrong password, a wrong first port, this address is blocked for wrong knocks, or SYN Mirror there is older than its ids: update it")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A connection that met, joined to a server: what goes in comes back, both ways, and the end of one side ends the other.
    #[test]
    fn relay_joins_both_ways() {
        // the server of this computer: an echo, upper case
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let to = server.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut all = Vec::new();
            s.read_to_end(&mut all).unwrap();
            s.write_all(&all.to_ascii_uppercase()).unwrap();
        });
        // the connection of the meeting: its far end is the one who came
        let meeting = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut came = TcpStream::connect(meeting.local_addr().unwrap()).unwrap();
        let (met, _) = meeting.accept().unwrap();
        let at = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 41933);
        let joined = thread::spawn(move || join(met, &to, at));
        let sent = vec![b'x'; 300_000];
        came.write_all(&sent).unwrap();
        came.shutdown(std::net::Shutdown::Write).unwrap();
        let mut back = Vec::new();
        came.read_to_end(&mut back).unwrap();
        assert_eq!(back, vec![b'X'; 300_000]);
        joined.join().unwrap();
    }

    #[test]
    fn given_goes_back() {
        let id = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let _ = GIVEN.set((41933, id));
        let text = given().unwrap();
        assert_eq!(text, "41933.0123456789abcdef");
        assert_eq!(parse_given(&text), Some((41933, id)));
        assert_eq!(parse_given("41933"), None);
        assert_eq!(parse_given("41933.0123"), None);
        assert_eq!(parse_given("0.0123456789abcdef"), None);
    }
}
