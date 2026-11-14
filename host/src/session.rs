// The session of the machine: the WebSocket it listens on, the static key of its Noise, the login by the token, the viewers.
//
// Several viewers may be in at once: every one sees the screen, one controls it - its input, the fitted resolution, the microphone,
// the clipboard and the files to the machine are taken, those of the others are not. The first one in controls; `TakeControl` passes
// the control at once; when the one that controls leaves, the one in longest takes it. Each change is told to every viewer: `Audience`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};
use crate::lib::adhoc::connection::external::Event;
use crate::lib::network::TCP::ExternalConnection;

/// Failed logins in a row before the machine takes none for `LOCKOUT`.
const MAX_FAILURES: u8 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// The most viewers in at once: the next one is refused, `Reason::FULL`.
const MAX_VIEWERS: usize = 8;

/// What the machine is started with.
struct Config {
    token:     String,
    host_name: String,
    secret:    [u8; 32], // X25519: the static key of Noise
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// What the host was started with, for a start of it as administrator: the same address, key and token.
static LAUNCH: OnceLock<(SocketAddr, PathBuf, bool)> = OnceLock::new();

/// The meeting point of SYN Mirror the host meets its viewer through, as given: `--meet`, `--meet-password`.
static MEET: OnceLock<(String, String)> = OnceLock::new();

/// Started with `--direct`: started again as administrator, it is so again.
static DIRECT: OnceLock<()> = OnceLock::new();

/// The arguments of the host started again: the page logs in to it with the token it has.
pub fn relaunch_args() -> Vec<String> {
    let (listen, key, software) = LAUNCH.get().expect("the host is started");
    let mut args = vec!["--listen".into(), listen.to_string(), "--key".into(), key.display().to_string()];
    if let Some(c) = CONFIG.get() {
        args.extend(["--token".into(), c.token.clone()]);
    }
    if *software {
        args.push("--software".into());
    }
    if let Some((point, password)) = MEET.get() {
        args.extend(["--meet".into(), point.clone(), "--meet-password".into(), password.clone()]);
        // access is granted already: the host started again knocks at the port given before, and the viewer meets it there
        if let Some(port) = crate::meet::granted() {
            args.extend(["--meet-given".into(), port.to_string()]);
        }
    }
    if DIRECT.get().is_some() {
        args.push("--direct".into());
    }
    args
}

/// The connections seated now, with the transport each sits on.
static SEATED: Mutex<Vec<(Weak<Connection>, Weak<ExternalConnection>)>> = Mutex::new(Vec::new());

/// A viewer that is in: its connection and the name it gave.
struct In {
    conn: Arc<Connection>,
    name: String,
}

/// The viewers in, in the order they came: the first has watched longest.
static AUDIENCE: Mutex<Vec<In>> = Mutex::new(Vec::new());

/// The viewer that controls the machine: read by every pack of input, without a lock; changed under the lock of `AUDIENCE`.
static CONTROLLER: ArcSwapOption<Connection> = ArcSwapOption::const_empty();

/// Failed logins in a row, and since when the machine takes none.
static FAILURES: Mutex<(u8, Option<Instant>)> = Mutex::new((0, None));

/// The secret key of Noise of this machine: the region "The keys of Noise" of protocol/gen/session/connection.rs takes it.
pub fn noise_secret() -> Option<[u8; 32]> { CONFIG.get().map(|c| c.secret) }

/// The viewer that controls the machine now.
pub fn controller() -> Option<Arc<Connection>> { CONTROLLER.load_full() }

/// `conn` is of the viewer that controls the machine: its input is taken.
pub fn controls(conn: &Connection) -> bool { CONTROLLER.load().as_deref().is_some_and(|c| std::ptr::eq(c, conn)) }

/// The `Arc` of the viewer in on `conn`.
pub fn viewer(conn: &Connection) -> Option<Arc<Connection>> {
    AUDIENCE.lock().unwrap().iter().find(|v| std::ptr::eq(Arc::as_ptr(&v.conn), conn)).map(|v| v.conn.clone())
}

pub fn run() {
    let mut listen: SocketAddr = "0.0.0.0:8765".parse().unwrap();
    let mut token = std::env::var("RDP_TOKEN").ok().filter(|t| !t.is_empty());
    let mut key_file = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("host.key"))).unwrap_or_else(|| PathBuf::from("host.key"));

    let mut software = false;
    let mut meet: Option<String> = None;
    let mut meet_password = std::env::var("RDP_MEET_PASSWORD").ok().filter(|p| !p.is_empty());
    let mut meet_given: Option<u16> = None;
    let mut direct = false;
    // as given; started by a double click, the answer of the menu ( src/menu.rs )
    let mut args = crate::menu::arguments().into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => listen = args.next().and_then(|v| v.parse().ok()).expect("--listen <address:port>"),
            "--token" => token = args.next(),
            "--key" => key_file = PathBuf::from(args.next().expect("--key <file>")),
            "--software" => {
                software = true;
                crate::os::video::software_only();
            }
            "--meet" => meet = Some(args.next().expect("--meet [<password>@]<meeting point>[:<port of requests>]")),
            "--meet-password" => meet_password = args.next(),
            // the host started again as administrator: the port of meeting given before this start
            "--meet-given" => meet_given = Some(args.next().and_then(|v| v.parse().ok()).expect("--meet-given <port>")),
            "--direct" => direct = true,
            _ => {
                eprintln!("rdp-host [--listen 0.0.0.0:8765] [--token <token>] [--key <file>] [--software] [--meet [<password>@]<meeting point>[:40000] [--meet-password <8 characters>] | --direct]\n  the token: --token, else RDP_TOKEN, else a new one each start\n  --software: the software encoder of H.264, not the one of the GPU\n  --meet: the viewer connects through this meeting point of SYN Mirror; after : its port of requests when it is not 40000; its password: before @, as the page of SYN Mirror copies it, else --meet-password, else RDP_MEET_PASSWORD\n  --direct: the viewer connects straight to this computer over WebRTC: its line and the answer go by hand");
                std::process::exit(2);
            }
        }
    }
    if direct && meet.is_some() {
        eprintln!("--direct and --meet: one of them - both read the console");
        std::process::exit(2);
    }
    if direct {
        let _ = DIRECT.set(());
    }
    let meet = meet.map(|given| {
        // the password in the line of the meeting point, else --meet-password
        let password = crate::meet::Meet::given(&given).0.map(String::from).or(meet_password).unwrap_or_default();
        let Ok(bytes) = <[u8; 8]>::try_from(password.as_bytes()) else {
            eprintln!("the password of the meeting point is 8 characters: --meet <password>@<meeting point>, else --meet-password, else RDP_MEET_PASSWORD");
            std::process::exit(2);
        };
        let Some((point, asks)) = crate::meet::Meet::point(&given) else {
            eprintln!("--meet {given}: a name or an address, and after : the port of requests, such as router.example.org:40000");
            std::process::exit(2);
        };
        let _ = MEET.set((given, password));
        crate::meet::Meet { point, password: bytes, asks }
    });

    let key_file = std::path::absolute(&key_file).unwrap_or(key_file); // a start as administrator begins in another folder
    let _ = LAUNCH.set((listen, key_file.clone(), software));
    let token = token.unwrap_or_else(new_token);
    let secret = load_or_make_key(&key_file);
    let host_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| String::from("machine"));
    // what the line carries after #: the token, and the check of the key of the machine - the page checks the key against it before
    // the token goes, so the line alone tells the page it is this machine
    let pass = format!("{token}-{}", key_check(&crate::lib::x25519::public_key(&secret)));
    let _ = CONFIG.set(Config { token: token.clone(), host_name, secret });

    actor0::start::on_receive_d::Viewer_Hello.add(on_hello);
    actor0::working::on_receive_d::Viewer_TakeControl.add(on_take_control);
    crate::gen::session::OnEvent.add(on_event);
    crate::video::register();
    crate::input::register();
    crate::marks::register();
    crate::clipboard::register();
    crate::files::register();
    crate::audio::register();
    crate::resize::register();
    crate::elevate::register();

    // the frames of the video are large: a buffer of 64 KiB takes a frame in fewer pieces
    crate::lib::network::engine::Engine::configure_shared(std::thread::available_parallelism().map(|n| n.get().min(4)).unwrap_or(2), 1 << 16);

    // started again as administrator, the host before may hold the port a moment longer: a few tries
    let mut tries = 0;
    let server = loop {
        let built = crate::lib::network::TCP::server::Builder::new("rdp-host", listen)
            .web_socket()
            .on_failure(crate::lib::network::failure::print_console())
            .on_connect(Arc::new(seat))
            .build();
        match built {
            Ok(server) => break server,
            Err(e) if tries < 50 && e.kind() == std::io::ErrorKind::AddrInUse => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => panic!("cannot listen on {listen}: {e}"),
        }
    };

    if crate::elevate::elevated_now() {
        say!("🛡️", News, "As administrator: windows run as administrator take the input too");
    }
    // way 4: the data channels of the pages come to a server of their own on 127.0.0.1 - the bytes of the session as they are,
    // no WebSocket - and its sessions are the ones of the other ways
    let bridge = direct.then(|| {
        crate::lib::network::TCP::server::Builder::new("rdp-host direct", SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)))
            .on_failure(crate::lib::network::failure::print_console())
            .on_connect(Arc::new(seat))
            .build()
            .expect("the server of the direct way on 127.0.0.1")
    });
    match meet {
        Some(meet) => crate::meet::start(meet, server.clone(), meet_given, pass), // the token and the check go in its line for the viewer
        None if direct => {
            let bridge = bridge.as_ref().and_then(|b| b.local_addr().ok()).expect("the address of the direct way");
            crate::direct::start(bridge, token, pass, listen);
        }
        None => {
            // the line for the viewer: all it needs, the address, the port and after / the token and the check, in one piece. Not #:
            // WeChat takes it for a tag, and copies the line cut
            let port = listen.port();
            let in_network = here(listen).map(|ip| format!("{ip}:{port}/{pass}"));
            let unknown_here = format!("<address of this computer>:{port}/{pass}");
            let unknown_public = format!("<public address>:{port}/{pass}");
            /// The one line of the way chosen: in a frame and in the clipboard when its address is known; else the line to fill in.
            fn one(line: Option<&str>, unknown: &str) {
                match line {
                    Some(line) => crate::console::line_to_send(line),
                    None => {
                        crate::console::line_to_send(unknown);
                        say!("✏️", Warn, "Put the address of this computer in place of <…> before you send it");
                    }
                }
            }
            match crate::menu::chosen() {
                Some(crate::menu::Way::Here) => one(in_network.as_deref(), &unknown_here),
                // from the internet: the address it sees this computer at is asked of STUN
                Some(crate::menu::Way::Internet) => {
                    let from_internet = crate::direct::public_ip().map(|ip| format!("{ip}:{port}/{pass}"));
                    one(from_internet.as_deref(), &unknown_public);
                }
                // started from a console or a script: both lines, each for where it goes
                _ => {
                    say!("📨", News, "Send one of these lines to whoever connects:");
                    say!("🏠", Good, "{}   in this network", in_network.as_deref().unwrap_or(&unknown_here));
                    if listen.ip().is_unspecified() {
                        say!("🌍", Good, "{unknown_public}   from the internet: port {port} forwarded to this computer");
                    }
                }
            }
            say!("👀", Wait, "Waiting for whoever connects…");
        }
    }

    let _bridge = bridge; // lives as long as the program
    loop {
        std::thread::park(); // the connections live on the threads of the transport
    }
}

/// The session of a socket of a page: seated, kept among the ones of the machine.
fn seat(ext: &Arc<ExternalConnection>) -> (Box<dyn crate::lib::adhoc::BytesDst>, Box<dyn crate::lib::adhoc::BytesSrc>) {
    let (connection, sink, source) = Connection::seat(ext);
    let mut seated = SEATED.lock().unwrap();
    seated.retain(|(c, _)| c.strong_count() > 0);
    seated.push((Arc::downgrade(&connection), Arc::downgrade(ext)));
    (sink, source)
}

/// A login: the token, at most `MAX_VIEWERS` viewers, no more than `MAX_FAILURES` wrong tokens in a row.
fn on_hello(hello: &mut Viewer_Hello, conn: &Connection, _actor: &Actor0) {
    let config = CONFIG.get().expect("the machine is configured");
    let name = hello.client_name.clone().unwrap_or_default();
    let from = address_of(conn).map(|a| a.to_string()).unwrap_or_default();

    let deny = |reason: Reason, attempts_left: Option<u8>| {
        let mut denied = Machine_Denied::new(0);
        denied.set_reason(reason);
        match attempts_left {
            Some(n) => denied.set_attempts_left(n),
            None => denied.attempts_left_to_null(),
        }
        actor0::granting::transmitter::send_machine_denied(denied, conn);
    };

    {
        let mut failures = FAILURES.lock().unwrap();
        if let Some(since) = failures.1 {
            if since.elapsed() < LOCKOUT {
                say!("⛔", Bad, "{name} from {from} is refused: too many wrong tokens, nobody gets in for a minute");
                return deny(Reason::HOST_OVERLOAD, None);
            }
            *failures = (0, None);
        }
    }

    if !same(hello.token.as_deref().unwrap_or_default().as_bytes(), config.token.as_bytes()) {
        let mut failures = FAILURES.lock().unwrap();
        failures.0 += 1;
        let left = MAX_FAILURES.saturating_sub(failures.0);
        if left == 0 {
            failures.1 = Some(Instant::now());
        }
        say!("⛔", Bad, "{name} from {from} is refused: a wrong token, {left} attempt(s) left");
        return deny(Reason::AUTHENTICATION_FAILED, Some(left));
    }

    let Some(this) = arc_of(conn) else { return deny(Reason::UNEXPECTED, None) };
    *FAILURES.lock().unwrap() = (0, None);
    {
        let mut audience = AUDIENCE.lock().unwrap();
        if audience.len() >= MAX_VIEWERS {
            drop(audience);
            say!("⛔", Bad, "{name} from {from} is refused: {MAX_VIEWERS} viewers are in already");
            return deny(Reason::FULL, None);
        }
        // under the lock: no `Audience` of another change goes before the welcome - the state of the login sends none
        let mut welcome = Machine_Welcome::default();
        welcome.host_name = Some(config.host_name.clone());
        welcome.set_administrator(crate::elevate::elevated_now());
        welcome.set_phone(cfg!(target_os = "android")); // its page shows Back, Home, Recents and types text
        actor0::granting::transmitter::send_machine_welcome(welcome, conn);
        audience.push(In { conn: this.clone(), name: name.clone() });
        // the viewer that comes in controls: it came to work - the one that controlled watches, and takes it back with a press
        if CONTROLLER.swap(Some(this.clone())).is_some() {
            crate::input::release_all();
            crate::audio::mic_off();
        }
        say!("✅", Good, "{name} from {from} is in and controls · {} watching", audience.len());
        announce(&audience);
    }
    crate::video::start(this.clone(), name);
    crate::audio::start(this);
}

/// The viewer takes the control: what the one before held goes up, its microphone stops.
fn on_take_control(_: &mut Viewer_TakeControl, conn: &Connection, _: &Actor0) {
    let audience = AUDIENCE.lock().unwrap();
    let Some(taker) = audience.iter().find(|v| std::ptr::eq(Arc::as_ptr(&v.conn), conn)) else { return };
    if controls(conn) {
        return;
    }
    CONTROLLER.store(Some(taker.conn.clone()));
    crate::input::release_all();
    crate::audio::mic_off();
    say!("🎮", News, "{} takes the control", taker.name);
    announce(&audience);
}

fn on_event(conn: &Connection, e: Event) {
    if !e.is_close() {
        return;
    }
    crate::video::stop(conn);
    crate::audio::stop(conn);
    crate::files::stop(conn);
    let left = {
        let mut audience = AUDIENCE.lock().unwrap();
        audience.iter().position(|v| std::ptr::eq(Arc::as_ptr(&v.conn), conn)).map(|i| {
            let gone = audience.remove(i);
            let controlled = controls(conn);
            if controlled {
                // the one in longest takes the control; what the one that left held goes up
                CONTROLLER.store(audience.first().map(|v| v.conn.clone()));
                crate::input::release_all();
                crate::audio::mic_off();
            }
            say!("👋", Quiet, "{} left · {} watching", gone.name, audience.len());
            announce(&audience);
            (controlled, audience.is_empty())
        })
    };
    // the mode of a display is changed out of the lock: it takes a while
    if let Some((controlled, nobody)) = left {
        crate::resize::left(conn, controlled, nobody);
    }
    SEATED.lock().unwrap().retain(|(c, _)| c.upgrade().is_some_and(|c| !std::ptr::eq(Arc::as_ptr(&c), conn)));
}

/// Tells every viewer how many watch and who controls, each as it sees it: under the lock of `AUDIENCE`, so they go in the order of the
/// changes.
fn announce(audience: &[In]) {
    let controller = CONTROLLER.load();
    let is_controller = |v: &In| controller.as_ref().is_some_and(|c| Arc::ptr_eq(c, &v.conn));
    let name = audience.iter().find(|v| is_controller(v)).map(|v| v.name.as_str()).unwrap_or_default();
    for v in audience {
        let mut told = Machine_Audience::default();
        told.set_watchers(audience.len() as u8);
        told.set_yours(is_controller(v));
        told.controller = Some(name.to_owned());
        told.set_no_input(!crate::os::input::taken());
        actor0::working::transmitter::send_machine_audience(told, &v.conn);
    }
}

/// The machine began or stopped taking input ( a phone: its user turned the control on or off ): every viewer is told.
#[allow(dead_code)] // of a phone only: a computer always takes it
pub fn input_changed() { announce(&AUDIENCE.lock().unwrap()); }

/// The `Arc` of a connection a handler sees by reference: it was seated by `on_connect`.
fn arc_of(conn: &Connection) -> Option<Arc<Connection>> {
    SEATED.lock().unwrap().iter().filter_map(|(c, _)| c.upgrade()).find(|c| std::ptr::eq(Arc::as_ptr(c), conn))
}

fn address_of(conn: &Connection) -> Option<SocketAddr> {
    SEATED.lock().unwrap().iter().find(|(c, _)| c.upgrade().is_some_and(|c| std::ptr::eq(Arc::as_ptr(&c), conn))).and_then(|(_, e)| e.upgrade()?.remote())
}

/// Equal bytes, in a time that does not depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// The letters and digits of the token and the check: none mistaken for another, 5 bits each.
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// A token of 20 characters, 100 bits, of letters and digits that are not mistaken for one another.
fn new_token() -> String {
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes).expect("the random numbers of the system");
    bytes.iter().map(|b| ALPHABET[(*b & 31) as usize] as char).collect()
}

/// The check of the key of the machine its line carries after the token and `-`: 8 characters of `ALPHABET`, the first 40 bits of the
/// SHA-256 of `public`. Another machine answering at the address would have to find a key of the same 40 bits - while the line is on
/// its way. The page checks it in src/worker/session.ts, as `keyCheck`.
pub fn key_check(public: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(public);
    let bits = u64::from_be_bytes([0, 0, 0, h[0], h[1], h[2], h[3], h[4]]);
    (0..8).rev().map(|i| ALPHABET[(bits >> (i * 5) & 31) as usize] as char).collect()
}

/// The secret key of Noise from `file` ( 64 hex digits ); a new one, written there, when there is none.
fn load_or_make_key(file: &PathBuf) -> [u8; 32] {
    if let Ok(text) = std::fs::read_to_string(file) {
        let text = text.trim();
        if text.len() == 64 {
            let mut key = [0u8; 32];
            if (0..32).all(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map(|b| key[i] = b).is_ok()) {
                return key;
            }
        }
        panic!("{} is not a key of 64 hex digits", file.display());
    }
    let (secret, _) = crate::lib::x25519::keypair();
    std::fs::write(file, secret.iter().map(|b| format!("{b:02x}")).collect::<String>()).unwrap_or_else(|e| panic!("cannot write {}: {e}", file.display()));
    secret
}

/// The address of this computer in its network, for the line of the viewer: the one `listen` names, else the one the system would
/// send from to the internet - a socket of UDP connected, nothing sent. None - no network.
fn here(listen: SocketAddr) -> Option<std::net::IpAddr> {
    if !listen.ip().is_unspecified() {
        return Some(listen.ip());
    }
    here_ip()
}

/// The address the system would send from to the internet: a socket of UDP connected, nothing sent. None - no network.
pub fn here_ip() -> Option<std::net::IpAddr> {
    let udp = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    udp.connect(("192.0.2.1", 9)).ok()?; // TEST-NET-1: only the route is looked up
    Some(udp.local_addr().ok()?.ip()).filter(|ip| !ip.is_unspecified())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check of a key: the same as the page makes of it ( src/worker/session.ts ), and as SHA-256 says.
    #[test]
    fn the_check_of_a_key() {
        assert_eq!(key_check(&core::array::from_fn(|i| i as u8)), "NNG64LMG");
    }
}
