// Way 4, direct: the page and the machine meet with no one between them - a data channel of WebRTC - and the two lines of the
// exchange go by hand, through any messenger.
//
// First the program asks its router to open a port to it ( src/portmap.rs ): when it does, and its address is public, one line
// `ws://…#…` is all, as on way ②, and the two lines below stay for where it does not connect.
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

use std::io::{self, BufRead, Read, Write};
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
/// and - in the answer of the machine - the token.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub ufrag:      String,
    pub pwd:        String,
    pub print:      [u8; 32],
    pub candidates: Vec<SocketAddr>,
    pub token:      String,
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
        Some(Card { ufrag, pwd, print, candidates, token })
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
/// sessions on 127.0.0.1, `token` - in the answer.
pub fn start(bridge: SocketAddr, token: String, listen: SocketAddr) {
    thread::Builder::new()
        .name("direct".into())
        .spawn(move || {
            // one line, if the router opens a port to the program; meanwhile the STUN servers, for the two lines
            let opening = (listen.is_ipv4() && !listen.ip().is_loopback()).then(|| thread::spawn(move || crate::portmap::open(listen.port())));
            let stun: Arc<[SocketAddr]> = probe().into();
            let opened = match opening.map(thread::JoinHandle::join) {
                Some(Ok(Ok(opened))) => {
                    one_line(opened, &token, listen.port());
                    true
                }
                Some(Ok(Err(crate::portmap::Refused::NotPublic(ip)))) => {
                    println!("Direct: the router opened a port, but its own address {ip} is not public - a NAT of the provider before it: two lines then");
                    false
                }
                Some(_) => {
                    println!("Direct: the router opens no port to this computer ( UPnP, NAT-PMP, PCP ): two lines then");
                    false
                }
                None => false, // the program listens on this computer alone: no port of the router leads to it
            };
            println!(
                "Direct: {}paste here the line the page sends ( rd1o.… ), then Enter - Make the line on the page makes it",
                if opened { "where that line does not connect: " } else { "" }
            );
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { return };
                if line.trim().is_empty() {
                    continue;
                }
                let Some(page) = Card::of(&line, false) else {
                    println!("Direct: not a line of the page - it starts with rd1o.");
                    continue;
                };
                let token = token.clone();
                let stun = stun.clone();
                let _ = thread::Builder::new().name("direct meeting".into()).spawn(move || {
                    if let Err(e) = meet(page, bridge, &token, &stun) {
                        println!("Direct: {e}");
                    }
                });
            }
        })
        .expect("a thread of the direct way");
}

/// The router opened a port to the program: the lines of ways ① and ②, the one of the internet with the address the router
/// has, kept while the program runs.
fn one_line(opened: crate::portmap::Opened, token: &str, port: u16) {
    println!("Direct: the router opened port {} to this computer ( {} ) - send one of these lines to whoever connects:", opened.external.port(), opened.how.name());
    if let Some(ip) = crate::session::here_ip() {
        println!("  ws://{ip}:{port}#{token}          in this network");
    }
    let line = format!("ws://{}#{token}", opened.external);
    println!("  {line}          from the internet");
    crate::clipboard::tell_put(&line);
    let token = token.to_owned();
    opened.keep(move |now| {
        let line = format!("ws://{now}#{token}");
        println!("Direct: the router gave another port - this line instead:\n  {line}          from the internet");
        crate::clipboard::tell_put(&line);
    });
}

/// One meeting: the answer printed, the page tried till it comes, the data channel bridged to the session.
fn meet(page: Card, bridge: SocketAddr, token: &str, stun: &[SocketAddr]) -> io::Result<()> {
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
    api.start_dtls(true).map_err(io::Error::other)?;
    api.start_sctp(true);
    let channel = api.create_data_channel(ChannelConfig { label: "rdp".into(), ordered: true, reliability: Reliability::Reliable, negotiated: Some(CHANNEL), protocol: String::new() });
    let creds = api.local_ice_credentials();
    let print: [u8; 32] = api.local_dtls_fingerprint().bytes.as_slice().try_into().map_err(|_| io::Error::other("the fingerprint of DTLS is not SHA-256"))?;

    let mut candidates = seen.clone();
    if let Some(here) = here.filter(|h| !seen.contains(h)) {
        candidates.push(here);
    }
    let answer = Card { ufrag: creds.ufrag, pwd: creds.pass, print, candidates, token: token.into() };
    let line = answer.line(true);
    println!("Direct: send this line to whoever connects, it has the token in it:\n  {line}");
    crate::clipboard::tell_put(&line);
    if seen.is_empty() {
        println!("Direct: no public STUN server answered - only a page in this network can meet this machine");
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
                    Event::IceConnectionStateChange(IceConnectionState::Connected | IceConnectionState::Completed) => met = true,
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) if met => {
                        println!("Direct: the page is gone");
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
                        println!("Direct: met the page");
                    }
                    Event::ChannelData(d) if d.id == channel => {
                        if let Some(s) = session.as_mut() {
                            if s.write_all(&d.data).is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Event::ChannelClose(id) if id == channel => {
                        println!("Direct: the page left");
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
                println!("Direct: the page did not come in {} minutes", PATIENCE.as_secs() / 60);
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

/// Asks every server of `STUN` at the start of the machine: the ones that answered, the nearest first. None answered - all that
/// have an address, to be asked again at each meeting: the network may come back.
fn probe() -> Vec<SocketAddr> {
    let named = resolve();
    let servers: Vec<SocketAddr> = named.iter().map(|(_, a)| *a).collect();
    let answers = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map(|udp| ask(&udp, &servers, PROBING)).unwrap_or_default();
    let Some(&(nearest, _, took)) = answers.first() else {
        println!("Direct: none of the {} public STUN servers answers - only a page in this network can meet this machine", STUN.len());
        return servers;
    };
    println!("Direct: {} of {} public STUN servers answer, the nearest {} in {} ms", answers.len(), STUN.len(), named[nearest].0, took.as_millis());
    // one IP, a port of its own for each server: the NAT is symmetric - only a page behind a NAT that is not meets it
    if answers.iter().any(|(_, a, _)| a.ip() == answers[0].1.ip() && a.port() != answers[0].1.port()) {
        println!("Direct: the NAT of this computer is symmetric - only a page behind a NAT that is not meets it; else way 3");
    }
    answers.into_iter().map(|(i, ..)| servers[i]).collect()
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
        };
        let answer = card.line(true);
        assert!(answer.starts_with("rd1a."));
        assert_eq!(Card::of(&answer, true), Some(card.clone()));
        assert_eq!(Card::of(&answer, false), None);
        let offer = card.line(false);
        assert_eq!(Card::of(&offer, false).map(|c| c.token), Some(String::new()));
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
    }
}
