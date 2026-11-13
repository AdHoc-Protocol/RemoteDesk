// Way 4, direct: the page and the machine meet with no one between them - a data channel of WebRTC - and the two lines of the
// exchange go by hand, through any messenger.
//
// First the program asks its router to open a port to it ( src/portmap.rs ): when it does, and its address is public, one line
// `address:port#token` is all, as on way ②, and the two lines below stay for where it does not connect.
//
// The page makes the offer: its line `rd1o.…` - the ICE name and password of the page, the fingerprint of its DTLS
// certificate, the addresses it can be reached at, the one a public STUN server saw among them. The user of the machine pastes it
// here; the machine answers with its own line `rd1a.…` - the same of the machine, and the token - and from then on keeps trying
// the page every second: the minutes the answer takes to come back by hand cost nothing, the machine keeps its NAT open
// meanwhile. When the page has the answer, the two meet: ICE, DTLS, SCTP - all of it `str0m`, no I/O of its own - and the data
// channel opens.
//
// The data channel carries the session as a socket would: the machine hands its bytes to a server of its own on 127.0.0.1 - the
// session there is the one of any other way, its Noise, its token, its viewers.
//
// The page is the controlling side of ICE and writes the first message of Noise, as it dials on the other ways; the machine is
// the client of DTLS and SCTP. No TURN: two NATs that are both symmetric meet no other way than ② or ③.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::config::Fingerprint;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, IceCreds, Input, Output, Rtc};

/// The public STUN servers that tell the machine the address the internet sees it at. Not each is reached from everywhere: Google
/// is closed in mainland China, a server in China may be slow abroad or gone - so the machine asks them all at its start and keeps
/// the ones that answer, the nearest first ( `probe` ).
const STUN: [&str; 11] = [
    // known all over the world
    "stun.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "global.stun.twilio.com:3478",
    "stun.nextcloud.com:3478",
    "stun.sipgate.net:3478",
    // mainland China
    "stun.miwifi.com:3478",        // Xiaomi
    "stun.chat.bilibili.com:3478", // Bilibili
    "stun.hitv.com:3478",          // Mango TV
    "stun.douyucdn.cn:18000",      // Douyu
    "stun.qq.com:3478",            // Tencent
    // Hong Kong
    "stun.kinesisvideo.ap-east-1.amazonaws.com:443", // Amazon, its region ap-east-1
];
/// How long the names of `STUN` are looked up at most: a lookup that hangs is left behind.
const RESOLVING: Duration = Duration::from_secs(2);
/// How long the servers have to answer at the start.
const PROBING: Duration = Duration::from_secs(2);
/// How long the servers that answered at the start have to answer again, at a meeting.
const ASKING: Duration = Duration::from_millis(1500);
/// A request of STUN not answered goes again after this.
const RESEND: Duration = Duration::from_millis(400);
/// The most addresses the internet sees the machine at that go into its answer: a computer whose traffic goes out by several
/// ways ( a VPN for some addresses ) is seen at several.
const SEEN: usize = 3;
/// The data channel of the session: negotiated, the same id on both sides - no DCEP.
const CHANNEL: u16 = 0;
/// How long the machine keeps trying a page that does not come.
const PATIENCE: Duration = Duration::from_secs(15 * 60);
/// The most the data channel holds unsent before the session waits: past it the bytes of the session stay in its socket.
const HIGH: usize = 1 << 20;
/// A piece of the session, as one message of the data channel.
const PIECE: usize = 1 << 14;

// ── the lines of the exchange ──

/// What one side tells the other: its ICE name and password, the SHA-256 of its DTLS certificate, the addresses it is reached at,
/// and - in the answer of the machine - the token and whether its router is picky.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub ufrag:      String,
    pub pwd:        String,
    pub print:      [u8; 32],
    pub candidates: Vec<SocketAddr>,
    pub token:      String,
    /// The answer: the router of the machine lets in only what it asked for ( `Router::Picky` ) - a page whose network gives it a
    /// new port for each address does not meet it. In the line: a byte of flags after the token, this one its bit 0.
    pub picky:      bool,
}

/// The kind of a line, its first byte: the offer of the page, the answer of the machine.
const OFFER: u8 = 0x10;
const ANSWER: u8 = 0x11;

impl Card {
    /// The line of the card: `rd1o.` or `rd1a.`, then its bytes in base64url.
    pub fn line(&self, answer: bool) -> String {
        let mut b = Vec::with_capacity(96);
        b.push(if answer { ANSWER } else { OFFER });
        short(&mut b, self.ufrag.as_bytes());
        short(&mut b, self.pwd.as_bytes());
        b.extend_from_slice(&self.print);
        b.push(self.candidates.len().min(255) as u8);
        for a in self.candidates.iter().take(255) {
            match a.ip() {
                IpAddr::V4(ip) => {
                    b.push(4);
                    b.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    b.push(6);
                    b.extend_from_slice(&ip.octets());
                }
            }
            b.extend_from_slice(&a.port().to_be_bytes());
        }
        if answer {
            short(&mut b, self.token.as_bytes());
            b.push(self.picky as u8);
        }
        format!("{}.{}", if answer { "rd1a" } else { "rd1o" }, base64url(&b))
    }

    /// The card of a line; None - not one, or not of this kind.
    pub fn of(line: &str, answer: bool) -> Option<Card> {
        let line = line.trim();
        let rest = line.strip_prefix(if answer { "rd1a." } else { "rd1o." })?;
        let b = unbase64url(rest)?;
        let mut r = Reader { b: &b, at: 0 };
        if r.byte()? != if answer { ANSWER } else { OFFER } {
            return None;
        }
        let ufrag = r.text()?;
        let pwd = r.text()?;
        let print: [u8; 32] = r.take(32)?.try_into().ok()?;
        let n = r.byte()?;
        let mut candidates = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let ip = match r.byte()? {
                4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(r.take(4)?).ok()?)),
                6 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(r.take(16)?).ok()?)),
                _ => return None,
            };
            let port = u16::from_be_bytes(r.take(2)?.try_into().ok()?);
            candidates.push(SocketAddr::new(ip, port));
        }
        let token = if answer { r.text()? } else { String::new() };
        let picky = answer && r.byte().is_some_and(|flags| flags & 1 != 0);
        Some(Card { ufrag, pwd, print, candidates, token, picky })
    }
}

fn short(b: &mut Vec<u8>, s: &[u8]) {
    let s = &s[..s.len().min(255)];
    b.push(s.len() as u8);
    b.extend_from_slice(s);
}

struct Reader<'a> {
    b:  &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.b.get(self.at..self.at + n)?;
        self.at += n;
        Some(v)
    }
    fn text(&mut self) -> Option<String> {
        let n = self.byte()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            s.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    s
}

fn unbase64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = ABC.iter().position(|&x| x == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// ── the machine ──

/// Waits for the lines of the pages on the console: each one gets its answer and its own meeting; `bridge` - the server of the
/// sessions on 127.0.0.1, `token` - in the answer; `pass` - the token and the check of the key, after # in the line of one way.
pub fn start(bridge: SocketAddr, token: String, pass: String, listen: SocketAddr) {
    thread::Builder::new()
        .name("direct".into())
        .spawn(move || {
            // one line, if the router opens a port to the program; meanwhile the STUN servers, for the two lines
            let opening = (listen.is_ipv4() && !listen.ip().is_loopback()).then(|| thread::spawn(move || crate::portmap::open(listen.port())));
            let (stun, router) = probe();
            let stun: Arc<[SocketAddr]> = stun.into();
            let opened = match opening.map(thread::JoinHandle::join) {
                Some(Ok(Ok(opened))) => {
                    one_line(opened, &pass);
                    true
                }
                // no port: its address is not public ( a NAT of the provider before it ), or it opens none - two lines then
                Some(_) => false,
                None => false, // the program listens on this computer alone: no port of the router leads to it
            };
            if opened {
                say!("🔁", Quiet, "If that line does not connect: paste here the line from the page ( rd1o.… ), then Enter");
            } else {
                say!("📥", Wait, "Paste here the line from the page ( rd1o.… ), then Enter");
                say!("💡", Quiet, "On the page: 4, then Make the line - it goes into the clipboard there");
            }
            while let Some(line) = crate::console::read_line() {
                if line.trim().is_empty() {
                    continue;
                }
                let Some(page) = Card::of(&line, false) else {
                    say!("❓", Warn, "Not a line of the page: it starts with rd1o.");
                    continue;
                };
                let token = token.clone();
                let stun = stun.clone();
                let _ = thread::Builder::new().name("direct meeting".into()).spawn(move || {
                    if let Err(e) = meet(page, bridge, &token, &stun, router) {
                        say!("❌", Bad, "{e}");
                    }
                });
            }
        })
        .expect("a thread of the direct way");
}

/// The router opened a port to the program: the line of way ②, with the address the router has, kept while the program runs.
fn one_line(opened: crate::portmap::Opened, pass: &str) {
    let line = format!("{}#{pass}", opened.external);
    crate::console::line_to_send(&line);
    let pass = pass.to_owned();
    opened.keep(move |now| {
        say!("🔄", Warn, "The router gave another port: this line instead");
        crate::console::line_to_send(&format!("{now}#{pass}"));
    });
}

/// One meeting: the answer printed, the page tried till it comes, the data channel bridged to the session.
fn meet(page: Card, bridge: SocketAddr, token: &str, stun: &[SocketAddr], router: Router) -> io::Result<()> {
    let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let port = udp.local_addr()?.port();
    // where it is reached: its address in its network, and the ones the internet sees
    let here = crate::session::here_ip().map(|ip| SocketAddr::new(ip, port));
    let seen = reflexive(&udp, stun);
    let local = here.unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));

    let mut rtc = Rtc::builder().build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").map_err(io::Error::other)?);
    for s in seen.iter().filter(|s| Some(**s) != here) {
        rtc.add_local_candidate(Candidate::server_reflexive(*s, local, "udp").map_err(io::Error::other)?);
    }
    for a in &page.candidates {
        if let Ok(c) = Candidate::host(*a, "udp") {
            rtc.add_remote_candidate(c);
        }
    }
    let mut api = rtc.direct_api();
    api.set_ice_controlling(false);
    api.set_remote_ice_credentials(IceCreds { ufrag: page.ufrag.clone(), pass: page.pwd.clone() });
    api.set_remote_fingerprint(Fingerprint { hash_func: "sha-256".into(), bytes: page.print.to_vec() });
    // DTLS and SCTP start when ICE meets ( `run` ): the handshake of DTLS has 40 seconds from its start, the answer may take minutes
    let channel = api.create_data_channel(ChannelConfig { label: "rdp".into(), ordered: true, reliability: Reliability::Reliable, negotiated: Some(CHANNEL), protocol: String::new() });
    let creds = api.local_ice_credentials();
    let print: [u8; 32] = api.local_dtls_fingerprint().bytes.as_slice().try_into().map_err(|_| io::Error::other("the fingerprint of DTLS is not SHA-256"))?;

    let mut candidates = seen.clone();
    if let Some(here) = here.filter(|h| !seen.contains(h)) {
        candidates.push(here);
    }
    let picky = router == Router::Picky;
    let answer = Card { ufrag: creds.ufrag, pwd: creds.pass, print, candidates, token: token.into(), picky };
    let line = answer.line(true);
    say!("✅", Good, "The answer is ready");
    crate::console::line_to_send(&line);
    if seen.is_empty() {
        say!("⚠️", Warn, "No public STUN server answered: only a page in this network can meet this computer");
    }
    // one IP, a port of its own for each STUN server: the network of the page is strict, as the one of a mobile operator
    if page.candidates.iter().any(|a| page.candidates.iter().any(|b| a.is_ipv4() && a.ip() == b.ip() && a.port() != b.port())) {
        match router {
            Router::Picky => say!("⛔", Bad, "This page will not meet this computer directly: its network gives it a new port for each address ( as mobile operators do ), and the router here lets in only what it asked for. Use way 3, or the page on Wi-Fi"),
            Router::Unknown => say!("⚠️", Warn, "The network of the page gives it a new port for each address ( a strict NAT, as mobile operators have ): it meets this computer only if this router lets in what it did not ask for; if not, use way 3"),
            Router::Open => {}
        }
    }

    run(rtc, udp, local, &page.candidates, channel, bridge, port, stun)
}

/// The meeting and then the session: the socket of UDP, the timers of `rtc`, the bytes of the session both ways.
#[allow(clippy::too_many_arguments)]
fn run(mut rtc: Rtc, udp: UdpSocket, local: SocketAddr, page: &[SocketAddr], channel: ChannelId, bridge: SocketAddr, port: u16, stun: &[SocketAddr]) -> io::Result<()> {
    // the reader of the session's socket wakes this loop through its own socket of UDP: a byte from 127.0.0.1
    let waker = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    let woken = waker.local_addr()?;
    let wake_at = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let (to_page, pieces) = mpsc::sync_channel::<Vec<u8>>(64);

    let started = Instant::now();
    let mut nudged = Instant::now() - Duration::from_secs(10);
    let mut met = false;
    let mut session: Option<TcpStream> = None;
    let mut pending: Option<Vec<u8>> = None;
    let mut buf = vec![0u8; 2048];

    loop {
        // what the meeting says: datagrams to send, events, the time it next wants to be asked
        let timeout = loop {
            match rtc.poll_output().map_err(io::Error::other)? {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    let _ = udp.send_to(&t.contents, t.destination);
                }
                Output::Event(e) => match e {
                    Event::IceConnectionStateChange(IceConnectionState::Connected | IceConnectionState::Completed) => {
                        if !met {
                            let mut api = rtc.direct_api();
                            api.start_dtls(true).map_err(io::Error::other)?;
                            api.start_sctp(true);
                        }
                        met = true;
                    }
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) if met => {
                        say!("👋", Quiet, "The page is gone");
                        return Ok(());
                    }
                    Event::ChannelOpen(id, _) if id == channel && session.is_none() => {
                        let s = TcpStream::connect(bridge)?;
                        s.set_nodelay(true)?;
                        let mut reader = s.try_clone()?;
                        let (to_page, waker) = (to_page.clone(), waker.try_clone()?);
                        thread::Builder::new().name("direct session".into()).spawn(move || {
                            loop {
                                let mut piece = vec![0u8; PIECE];
                                match reader.read(&mut piece) {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        piece.truncate(n);
                                        if to_page.send(piece).is_err() {
                                            break;
                                        }
                                        let _ = waker.send_to(&[0], wake_at);
                                    }
                                }
                            }
                            let _ = to_page.send(Vec::new()); // the session ended: an empty piece says so
                            let _ = waker.send_to(&[0], wake_at);
                        })?;
                        if let Some(mut c) = rtc.channel(channel) {
                            c.set_buffered_amount_low_threshold(HIGH / 4);
                        }
                        session = Some(s);
                        say!("🤝", Good, "Met the page: straight, through both routers");
                    }
                    Event::ChannelData(d) if d.id == channel => {
                        if let Some(s) = session.as_mut() {
                            if s.write_all(&d.data).is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Event::ChannelClose(id) if id == channel => {
                        say!("👋", Quiet, "The page left");
                        return Ok(());
                    }
                    _ => {}
                },
            }
        };

        // the session to the page: what fits in the data channel now; the rest waits in the socket of the session
        if session.is_some() {
            loop {
                let piece = match pending.take() {
                    Some(p) => p,
                    None => match pieces.try_recv() {
                        Ok(p) => p,
                        Err(_) => break,
                    },
                };
                if piece.is_empty() {
                    return Ok(()); // the session ended
                }
                let Some(mut c) = rtc.channel(channel) else { break };
                if c.buffered_amount() > HIGH || !c.write(true, &piece).map_err(io::Error::other)? {
                    pending = Some(piece);
                    break;
                }
            }
        }

        // till the page comes, its NAT is knocked at every second: a byte to each of its addresses keeps this machine's NAT open
        // to them - the first check of the page then gets in
        if !met {
            if started.elapsed() > PATIENCE {
                say!("⌛", Warn, "The page did not come in {} minutes: ask for a new line", PATIENCE.as_secs() / 60);
                return Ok(());
            }
            if nudged.elapsed() >= Duration::from_secs(1) {
                for a in page {
                    let _ = udp.send_to(&[0xFF], a);
                }
                nudged = Instant::now();
            }
        }

        let now = Instant::now();
        let wait = timeout.saturating_duration_since(now).clamp(Duration::from_millis(1), Duration::from_millis(if met { 1000 } else { 500 }));
        udp.set_read_timeout(Some(wait))?;
        buf.resize(2048, 0);
        let input = match udp.recv_from(&mut buf) {
            Ok((_, from)) if from == woken => continue, // the session has bytes for the page
            Ok((_, from)) if stun.contains(&from) => continue, // a late answer of a STUN server: none of the meeting's
            Ok((n, source)) => {
                buf.truncate(n);
                match Receive::new(Protocol::Udp, source, local, &buf) {
                    Ok(r) => Input::Receive(Instant::now(), r),
                    Err(_) => continue, // not WebRTC: a knock of our own coming back, a stray datagram
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::ConnectionReset) => Input::Timeout(Instant::now()),
            Err(e) => return Err(e),
        };
        rtc.handle_input(input).map_err(io::Error::other)?;
    }
}

// ── the STUN servers ──

/// Looks up the names of `STUN` all at once: the address of each found within `RESOLVING`, in the order of `STUN`.
fn resolve() -> Vec<(&'static str, SocketAddr)> {
    let (found, of) = mpsc::channel();
    for (i, name) in STUN.into_iter().enumerate() {
        let found = found.clone();
        let _ = thread::Builder::new().name("stun lookup".into()).spawn(move || {
            if let Some(a) = name.to_socket_addrs().ok().and_then(|mut a| a.find(SocketAddr::is_ipv4)) {
                let _ = found.send((i, a));
            }
        });
    }
    drop(found);
    let until = Instant::now() + RESOLVING;
    let mut named = Vec::with_capacity(STUN.len());
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match of.recv_timeout(left) {
            Ok((i, a)) => named.push((i, a)),
            Err(_) => break, // all looked up, or the time is out
        }
    }
    named.sort_unstable_by_key(|(i, _)| *i);
    named.into_iter().map(|(i, a)| (STUN[i], a)).collect()
}

/// The address the internet sees this computer at, as the nearest STUN server that answers tells it; None - none answered.
pub fn public_ip() -> Option<IpAddr> {
    let servers: Vec<SocketAddr> = resolve().into_iter().map(|(_, a)| a).collect();
    let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    ask(&udp, &servers, PROBING).first().map(|(_, seen, _)| seen.ip())
}

/// What the router of this computer lets in from the internet, as found at its start ( `filtering` ).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Router {
    /// What comes from an address this computer wrote to, from any port of it, or anything at all: a page in any network meets it -
    /// the machine knocks at the address of the page.
    Open,
    /// Only what comes from the very address and port it wrote to, or a new port of its own for each address ( a symmetric NAT ): a
    /// page whose network gives it a new port for each address too - a phone in a mobile network - does not meet it.
    Picky,
    /// Not found: fewer than two servers that answer from another address of theirs answered.
    Unknown,
}

/// Asks every server of `STUN` at the start of the machine: the ones that answered, the nearest first, and what the router lets
/// in. None answered - all that have an address, to be asked again at each meeting: the network may come back.
fn probe() -> (Vec<SocketAddr>, Router) {
    let named = resolve();
    let servers: Vec<SocketAddr> = named.iter().map(|(_, a)| *a).collect();
    let all = servers.clone();
    let testing = thread::spawn(move || UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_or(Router::Unknown, |udp| filtering(&udp, &all)));
    let answers = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map(|udp| ask(&udp, &servers, PROBING)).unwrap_or_default();
    let mut router = testing.join().unwrap_or(Router::Unknown);
    if answers.is_empty() {
        say!("⚠️", Warn, "None of the {} public STUN servers answers: only a page in this network can meet this computer", STUN.len());
        return (servers, Router::Unknown);
    }
    // one IP, a port of its own for each server: the NAT is symmetric - only a page behind a NAT that is not meets it
    if answers.iter().any(|(_, a, _)| a.ip() == answers[0].1.ip() && a.port() != answers[0].1.port()) {
        router = Router::Picky;
        say!("⚠️", Warn, "The router of this computer is strict ( a symmetric NAT ): way 4 meets a page at home or in an office, not a phone in a mobile network - for that one, way 3");
    } else if router == Router::Picky {
        say!("⚠️", Warn, "The router of this computer lets in only what it asked for: way 4 meets a page at home or in an office, not a phone in a mobile network - for that one, way 3");
    } else if router == Router::Open {
        say!("🌐", Quiet, "The router of this computer lets in the pages it calls: way 4 meets a page in any network");
    }
    (answers.into_iter().map(|(i, ..)| servers[i]).collect(), router)
}

/// How long the router is tested at the start.
const FILTERING: Duration = Duration::from_millis(1500);

/// What the router lets in, by the servers of STUN that answer from another address of theirs ( RFC 5780 ): each is asked as is,
/// to answer from another port, and from another address and port. An answer from elsewhere that comes in: the router lets in
/// what it did not ask for - `Open`. Two servers that tell another address of theirs answered as is, none from elsewhere: it lets
/// in only what it asked for - `Picky`. A server that does not do what is asked answers from where it was asked: no evidence.
/// The transaction id: byte 18 - how it was asked, byte 19 - the index of the server.
fn filtering(udp: &UdpSocket, servers: &[SocketAddr]) -> Router {
    let n = servers.len().min(256);
    let mut request = [0u8; 28];
    request[..2].copy_from_slice(&1u16.to_be_bytes()); // Binding Request
    request[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    request[20..24].copy_from_slice(&[0x00, 0x03, 0x00, 0x04]); // CHANGE-REQUEST, its flags in byte 27
    if n == 0 || getrandom::fill(&mut request[8..18]).is_err() || udp.set_read_timeout(Some(Duration::from_millis(50))).is_err() {
        return Router::Unknown;
    }
    let mut able = [false; 256]; // answered as is, and told another address of its own
    let started = Instant::now();
    let mut sent = started - RESEND;
    let mut b = [0u8; 512];
    while started.elapsed() < FILTERING {
        if sent.elapsed() >= RESEND {
            for (i, s) in servers[..n].iter().enumerate() {
                request[19] = i as u8;
                // as is; from another port; from another address and port
                for (how, flags) in [(0u8, 0u8), (1, 0x02), (2, 0x06)] {
                    request[18] = how;
                    request[3] = if how == 0 { 0 } else { 8 };
                    request[27] = flags;
                    let _ = udp.send_to(&request[..if how == 0 { 20 } else { 28 }], s);
                }
            }
            sent = Instant::now();
        }
        let Ok((len, from)) = udp.recv_from(&mut b) else { continue }; // the time out, or an ICMP of a closed port ( Windows )
        let i = b[19] as usize;
        if len < 20 || b[..2] != [0x01, 0x01] || b[8..18] != request[8..18] || i >= n {
            continue;
        }
        if b[18] != 0 && from != servers[i] {
            return Router::Open;
        }
        if b[18] == 0 && from == servers[i] && elsewhere(&b[..len]) {
            able[i] = true;
        }
    }
    if able.iter().filter(|a| **a).count() >= 2 { Router::Picky } else { Router::Unknown }
}

/// Whether a Binding Success Response tells another address of its server: OTHER-ADDRESS ( 0x802C ), or CHANGED-ADDRESS ( 0x0005 )
/// of an old server.
fn elsewhere(b: &[u8]) -> bool {
    let mut at = 20;
    while at + 4 <= b.len() {
        let kind = u16::from_be_bytes([b[at], b[at + 1]]);
        if kind == 0x802C || kind == 0x0005 {
            return true;
        }
        at += 4 + (u16::from_be_bytes([b[at + 2], b[at + 3]]) as usize).div_ceil(4) * 4;
    }
    false
}

/// The addresses the internet sees `udp` at, by the servers that answered at the start: in the order they answer now, each once,
/// `SEEN` at most; none - none answered.
fn reflexive(udp: &UdpSocket, stun: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut seen = Vec::with_capacity(SEEN);
    for (_, a, _) in ask(udp, stun, ASKING) {
        if seen.len() < SEEN && !seen.contains(&a) {
            seen.push(a);
        }
    }
    seen
}

/// Binding requests of STUN from `udp` to `servers` - again every `RESEND` to the ones still silent - until each answered or
/// `window` passed. Each answer in the order they came: the index of its server, the address it saw `udp` at, how long it took.
/// The last byte of the transaction id is the index of the server: 256 servers at most.
fn ask(udp: &UdpSocket, servers: &[SocketAddr], window: Duration) -> Vec<(usize, SocketAddr, Duration)> {
    let n = servers.len().min(256);
    let mut answers = Vec::with_capacity(n);
    let mut request = [0u8; 20];
    request[..2].copy_from_slice(&1u16.to_be_bytes()); // Binding Request, no attributes
    request[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    if n == 0 || getrandom::fill(&mut request[8..19]).is_err() || udp.set_read_timeout(Some(Duration::from_millis(50))).is_err() {
        return answers;
    }
    let mut answered = [false; 256];
    let started = Instant::now();
    let mut sent = started - RESEND;
    let mut b = [0u8; 512];
    while answers.len() < n && started.elapsed() < window {
        if sent.elapsed() >= RESEND {
            for (i, s) in servers[..n].iter().enumerate().filter(|(i, _)| !answered[*i]) {
                request[19] = i as u8;
                let _ = udp.send_to(&request, s);
            }
            sent = Instant::now();
        }
        let Ok((len, from)) = udp.recv_from(&mut b) else { continue }; // the time out, or an ICMP of a closed port ( Windows )
        let i = b[19] as usize;
        if len < 20 || b[..2] != [0x01, 0x01] || b[8..19] != request[8..19] || i >= n || answered[i] || servers[i] != from {
            continue;
        }
        if let Some(a) = mapped(&b[..len]) {
            answered[i] = true;
            answers.push((i, a, started.elapsed()));
        }
    }
    answers
}

/// The magic cookie of STUN: in each message, and the mask of XOR-MAPPED-ADDRESS.
const MAGIC: u32 = 0x2112_A442;

/// The IPv4 address a Binding Success Response says it saw: its XOR-MAPPED-ADDRESS ( 0x0020 ), else the MAPPED-ADDRESS ( 0x0001 )
/// of an old server.
fn mapped(b: &[u8]) -> Option<SocketAddr> {
    let mut plain = None;
    let mut at = 20;
    while at + 4 <= b.len() {
        let kind = u16::from_be_bytes([b[at], b[at + 1]]);
        let len = u16::from_be_bytes([b[at + 2], b[at + 3]]) as usize;
        let v = &b[at + 4..(at + 4 + len).min(b.len())];
        if (kind == 0x0020 || kind == 0x0001) && v.len() >= 8 && v[1] == 0x01 {
            let port = u16::from_be_bytes([v[2], v[3]]);
            let ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
            if kind == 0x0020 {
                return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip ^ MAGIC)), port ^ (MAGIC >> 16) as u16));
            }
            plain = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port));
        }
        at += 4 + len.div_ceil(4) * 4;
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lines_go_and_come_back() {
        let card = Card {
            ufrag:      "aB3x".into(),
            pwd:        "Qw4rT9yU1iO0pLkJhGfDsA".into(),
            print:      [7; 32],
            candidates: vec!["203.0.113.7:51234".parse().unwrap(), "[2001:db8::1]:40000".parse().unwrap()],
            token:      "K7QM2XHP9RTA4WZD8CNE".into(),
            picky:      true,
        };
        let answer = card.line(true);
        assert!(answer.starts_with("rd1a."));
        assert_eq!(Card::of(&answer, true), Some(card.clone()));
        assert_eq!(Card::of(&answer, false), None);
        let offer = card.line(false);
        assert_eq!(Card::of(&offer, false).map(|c| (c.token, c.picky)), Some((String::new(), false)));
        // an answer of a machine before the flags: not picky
        let bytes = unbase64url(&answer["rd1a.".len()..]).unwrap();
        let old = format!("rd1a.{}", base64url(&bytes[..bytes.len() - 1]));
        assert_eq!(Card::of(&old, true).map(|c| c.picky), Some(false));
        assert_eq!(Card::of("rd1a.!!!", true), None);
        assert!(answer.len() < 160, "{}", answer.len());
    }

    #[test]
    fn the_address_a_stun_server_saw() {
        // 203.0.113.7:51234 under the cookie, after an attribute of another kind; the plain one of an old server
        let mut b = vec![0x01, 0x01, 0, 24, 0x21, 0x12, 0xA4, 0x42];
        b.extend_from_slice(&[9; 12]);
        b.extend_from_slice(&[0x80, 0x22, 0, 3, b'a', b'b', b'c', 0]); // SOFTWARE, padded
        let port = 51234u16 ^ 0x2112;
        let ip = u32::from(Ipv4Addr::new(203, 0, 113, 7)) ^ MAGIC;
        b.extend_from_slice(&[0, 0x20, 0, 8, 0, 0x01]);
        b.extend_from_slice(&port.to_be_bytes());
        b.extend_from_slice(&ip.to_be_bytes());
        assert_eq!(mapped(&b), Some("203.0.113.7:51234".parse().unwrap()));
        let mut old = b[..20].to_vec();
        old.extend_from_slice(&[0, 0x01, 0, 8, 0, 0x01, 0xC8, 0x22, 198, 51, 100, 9]);
        assert_eq!(mapped(&old), Some("198.51.100.9:51234".parse().unwrap()));
        assert_eq!(mapped(&b[..20]), None);
        assert!(!elsewhere(&b));
        let mut told = b.clone();
        told.extend_from_slice(&[0x80, 0x2C, 0, 8, 0, 0x01, 0x0D, 0x97, 198, 51, 100, 10]); // OTHER-ADDRESS 198.51.100.10:3479
        assert!(elsewhere(&told));
    }
}
