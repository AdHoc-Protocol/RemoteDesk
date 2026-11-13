// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! What the bytes of a connection go through: chosen here, run in the native library, in one call
//! a buffer each way.
//!
//! ```text
//! Chain::default().tls("host", Some("http/1.1"))
//! ```
//!
//! The Noise of a connection is not chosen here: `[Noise]` of the description of its protocol declares it, and the generated code
//! hands it to the connection as it seats the protocol layer ( [`ExternalConnection::secure`] ); the library puts it inside TLS.
//!
//! [`ExternalConnection::secure`]: crate::lib::network::TCP::external_connection::ExternalConnection::secure
//!
//! The frames of a WebSocket join in by themselves: a WebSocket connection has them. On the way out
//! each layer only writes down what is to be done to this buffer, the last one sends it all into
//! the library; on the way in that call opens TLS, cuts the frames and opens Noise, and the
//! connection gets the plaintext. Control frames, and whatever is not plain data, stay with the
//! decoder of the WebSocket.
//!
//! Needs the native library at run time ([`crate::lib::crypto`]); a connection with no chain never
//! touches it.

use std::ffi::{c_void, CString};
use std::io;
use std::sync::Arc;

use super::crypto::{api, Api, Mem};
use super::network::buf::Buf;
use super::network::engine::{self, Loop, Slot};
use super::noise::{NoiseNK, NoiseStream, OnPeer, HEADER as NOISE_HEADER, KEY, PATTERN_IK, PATTERN_KK, PATTERN_NK, PATTERN_XX, RECORD, STREAM_PROLOGUE};

const TLS_HEADER: usize = 5;
const TLS_TAG: usize = 16;
const TLS_MAX_DATA: usize = 16384;
/// Past the data: the header, the type byte and the tag.
const TLS_OVERHEAD: usize = TLS_HEADER + 1 + TLS_TAG;
/// The longest record a peer may send.
const TLS_MAX_RECORD: usize = TLS_HEADER + 16384 + 256;
/// The longest record of the connection's own.
const CONTROL_RECORD: usize = TLS_OVERHEAD + 5;
/// What `adhoc_tls_handshake` wants free to write into.
const FLIGHT_ROOM: usize = 16645;

/// What a buffer of the engine has past the size asked for: a TLS record's header, type and tag, a
/// record's of Noise, a frame's header.
pub(crate) const HEADERS: usize = TLS_OVERHEAD + 18 + 14 + 2;

/// A record of Noise cut by the end of the plaintext, and a whole TLS record behind it.
pub(crate) const RECORD_BUFFER: usize = (NOISE_HEADER + RECORD + 16) + TLS_MAX_RECORD;

// The shared memory of a chain: see PORTING.md.
const TLS: usize = 0;
const RX_CTX: usize = 8;
const RX_N: usize = 16;
const FLAGS: usize = 24;
const WS_STATE: usize = 28;
const WS_LEFT: usize = 32;
const WS_MASK: usize = 36;
const WS_MASK_POS: usize = 40;
const NOISE_BYTES: usize = 52;
const NOISE_COUNT: usize = 56;
const TLS_FLAGS: usize = 60;
const OPS: usize = 64;
const PIECES: usize = 68;
const OP: usize = 72;
const OP_SIZE: usize = 32;
const NOISE_END: usize = 328;
const PIECE: usize = 332;
/// What fits the long of `adhoc_chain_rx`: no more is read at once.
const OFFSET: usize = (1 << 21) - 1;

pub(crate) const OP_NOISE: i32 = 0;
pub(crate) const OP_MASK: i32 = 1;
const OP_TLS: i32 = 2;

const F_WS: i32 = 1;
const F_WS_CLIENT: i32 = 2;
const F_NOISE: i32 = 4;
const F_IN_PIECES: i32 = 8;

pub(crate) const WS_NEW_FRAME: i32 = 0;
pub(crate) const WS_DATA: i32 = 1;
pub(crate) const WS_OTHER: i32 = 2;

#[inline(always)]
unsafe fn get(mem: *mut u8, at: usize) -> i32 {
    (mem.add(at) as *const i32).read()
}

#[inline(always)]
unsafe fn put(mem: *mut u8, at: usize, v: i32) {
    (mem.add(at) as *mut i32).write(v)
}

#[inline(always)]
unsafe fn put64(mem: *mut u8, at: usize, v: i64) {
    (mem.add(at) as *mut i64).write(v)
}

/// One thing to be done to this buffer, in the memory `mem` of a chain.
#[inline]
pub(crate) unsafe fn op(mem: *mut u8, kind: i32, address: i64, len: usize, a: i64, b: i64) {
    let n = get(mem, OPS);
    let at = OP + n as usize * OP_SIZE;
    put(mem, at, kind);
    put(mem, at + 4, len as i32);
    put64(mem, at + 8, address);
    put64(mem, at + 16, a);
    put64(mem, at + 24, b);
    put(mem, OPS, n + 1);
}

/// Decides whether the chain of certificates a server has shown, the leaf first, is to be trusted;
/// the host name is checked apart.
pub type Trust = Arc<dyn Fn(&[&[u8]]) -> bool + Send + Sync>;

/// With the roots of the system: whether a certificate is also checked for being revoked. Off by default: the check goes to the
/// network during the handshake, and a certificate with no answer about it fails. Set before the first connection.
pub static CHECK_REVOCATION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static TRUST: parking_lot::RwLock<Option<Trust>> = parking_lot::RwLock::new(None);
static CONTEXT: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

/// What every TLS client asks about the certificates of its server. `None`, as it is at first: the
/// roots of the system (Windows; elsewhere nothing is trusted until this is set).
pub fn set_trust(trust: Option<Trust>) {
    *TRUST.write() = trust;
}

/// Called from the handshake: nothing may escape into native code.
unsafe extern "C" fn verify(_: *mut c_void, count: i32, der: *const *const u8, len: *const i32) -> i32 {
    std::panic::catch_unwind(|| {
        let mut certs: Vec<&[u8]> = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            certs.push(std::slice::from_raw_parts(*der.add(i), *len.add(i) as usize));
        }
        let given = TRUST.read().clone();
        match given {
            Some(t) => t(&certs),
            None => system_roots(&certs),
        }
    })
    .unwrap_or(false) as i32
}

#[cfg(windows)]
fn system_roots(certs: &[&[u8]]) -> bool {
    use super::crypto::{library_symbol, open_library};
    use std::mem::transmute as cast;
    type Open = unsafe extern "system" fn(usize, u32, usize, u32, *const c_void) -> *mut c_void;
    type Add = unsafe extern "system" fn(*mut c_void, u32, *const u8, u32, u32, *mut c_void) -> i32;
    type Create = unsafe extern "system" fn(u32, *const u8, u32) -> *const c_void;
    type Chain = unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut c_void, *const Para, u32, *const c_void, *mut *const Context) -> i32;
    type Free = unsafe extern "system" fn(*const c_void) -> i32;
    type Close = unsafe extern "system" fn(*mut c_void, u32) -> i32;
    #[repr(C)]
    struct Para {
        size: u32,
        usage_kind: u32,
        usages: u32,
        usage: *const c_void,
    }
    #[repr(C)]
    struct Context {
        size: u32,
        error: u32,
        info: u32,
    }
    const ASN: u32 = 1; // X509_ASN_ENCODING
    const MEMORY: usize = 2; // CERT_STORE_PROV_MEMORY
    const ADD_ALWAYS: u32 = 4;
    if certs.is_empty() {
        return false;
    }
    // SAFETY: the functions of crypt32, as its header has them; found only when a client asks
    unsafe {
        let lib = open_library(std::path::Path::new("crypt32.dll"));
        if lib.is_null() {
            return false;
        }
        let symbols = [
            library_symbol(lib, "CertOpenStore\0"),
            library_symbol(lib, "CertAddEncodedCertificateToStore\0"),
            library_symbol(lib, "CertCreateCertificateContext\0"),
            library_symbol(lib, "CertGetCertificateChain\0"),
            library_symbol(lib, "CertFreeCertificateChain\0"),
            library_symbol(lib, "CertFreeCertificateContext\0"),
            library_symbol(lib, "CertCloseStore\0"),
        ];
        if symbols.iter().any(|s| s.is_null()) {
            return false;
        }
        let (open, add, create, chain, free_chain, free_cert, close): (Open, Add, Create, Chain, Free, Free, Close) = (
            cast(symbols[0]),
            cast(symbols[1]),
            cast(symbols[2]),
            cast(symbols[3]),
            cast(symbols[4]),
            cast(symbols[5]),
            cast(symbols[6]),
        );
        let store = open(MEMORY, 0, 0, 0, std::ptr::null());
        for c in &certs[1..] {
            add(store, ASN, c.as_ptr(), c.len() as u32, ADD_ALWAYS, std::ptr::null_mut());
        }
        let leaf = create(ASN, certs[0].as_ptr(), certs[0].len() as u32);
        let mut ok = false;
        if !leaf.is_null() {
            let para = Para {
                size: std::mem::size_of::<Para>() as u32,
                usage_kind: 0,
                usages: 0,
                usage: std::ptr::null(),
            };
            let mut context: *const Context = std::ptr::null();
            let flags = if CHECK_REVOCATION.load(std::sync::atomic::Ordering::Relaxed) { 0x2000_0000 } else { 0 }; // CERT_CHAIN_REVOCATION_CHECK_CHAIN
            if chain(std::ptr::null_mut(), leaf, std::ptr::null(), store, &para, flags, std::ptr::null(), &mut context) != 0 {
                ok = (*context).error == 0;
                free_chain(context as *const c_void);
            }
            free_cert(leaf);
        }
        close(store, 0);
        ok
    }
}

#[cfg(not(windows))]
fn system_roots(_: &[&[u8]]) -> bool {
    false
}

/// The choice of what lies between the socket and the protocol layer of a connection.
#[derive(Clone)]
pub struct Chain {
    tls_host: Option<CString>,
    tls_alpn: Option<Vec<u8>>,
    tls_server: i64,
    pub(crate) pattern: i32,
    static_secret: Option<[u8; KEY]>,
    /// The public key of `static_secret`, computed once for all the handshakes of this chain.
    static_public: std::sync::OnceLock<[u8; KEY]>,
    pub(crate) peer: Option<[u8; KEY]>,
    pub(crate) on_peer: Option<OnPeer>,
    tx_limit: i64,
}

impl Default for Chain {
    fn default() -> Self {
        Self {
            tls_host: None,
            tls_alpn: None,
            tls_server: 0,
            pattern: -1,
            static_secret: None,
            static_public: std::sync::OnceLock::new(),
            peer: None,
            on_peer: None,
            tx_limit: 0,
        }
    }
}

impl Chain {
    /// TLS 1.3 under everything. `host`: what the server's certificate has to carry. `alpn`: the
    /// protocol to ask for.
    pub fn tls(mut self, host: &str, alpn: Option<&str>) -> Self {
        self.tls_host = Some(CString::new(host).expect("a host name has no zero byte"));
        self.tls_alpn = alpn.map(|a| a.as_bytes().to_vec());
        self
    }

    /// The client's end with no TLS: what is inside stays.
    pub(crate) fn without_tls(mut self) -> Self {
        self.tls_host = None;
        self.tls_alpn = None;
        self
    }

    /// TLS 1.3 under everything, the server's end: its certificate chain and key, PEM. One context
    /// per call, for all the connections of the chain and as long as the process.
    pub fn tls_server(mut self, certificate_chain: &[u8], key: &[u8]) -> Self {
        // SAFETY: the bounds go with the pointers
        self.tls_server = unsafe { (api().tls_server_context)(certificate_chain.as_ptr(), certificate_chain.len() as i32, key.as_ptr(), key.len() as i32) };
        assert!(self.tls_server != 0, "The certificate chain and the key of TLS: they are not PEM, or do not match.");
        self
    }

    /// Noise NK, the client's end: it knows the server.
    pub(crate) fn noise_nk(self, server_public: &[u8; KEY]) -> Self {
        self.noise(PATTERN_NK, None, Some(server_public))
    }

    pub(crate) fn noise_nk_server(self, secret: &[u8; KEY]) -> Self {
        self.noise(PATTERN_NK, Some(secret), None)
    }

    /// Noise IK, the client's end: it knows the server, and proves its own key in the first message.
    pub(crate) fn noise_ik(self, server_public: &[u8; KEY], my_secret: &[u8; KEY]) -> Self {
        self.noise(PATTERN_IK, Some(my_secret), Some(server_public))
    }

    pub(crate) fn noise_ik_server(self, secret: &[u8; KEY]) -> Self {
        self.noise(PATTERN_IK, Some(secret), None)
    }

    /// Noise XX: neither end knows the other beforehand, each proves its key;
    /// [`on_peer`](Self::on_peer) decides.
    pub(crate) fn noise_xx(self, my_secret: &[u8; KEY]) -> Self {
        self.noise(PATTERN_XX, Some(my_secret), None)
    }

    /// Noise KK: both know each other beforehand.
    pub(crate) fn noise_kk(self, my_secret: &[u8; KEY], their_public: &[u8; KEY]) -> Self {
        self.noise(PATTERN_KK, Some(my_secret), Some(their_public))
    }

    /// Noise of `pattern` with both keys of this side: its own secret and the public key of the other side. Which of them a
    /// handshake takes its role decides ( [`noise_stream`](Self::noise_stream) ): the generated code gives both, not knowing which
    /// end of the connection it is.
    pub(crate) fn noise_keys(self, pattern: i32, secret: &[u8; KEY], peer: &[u8; KEY]) -> Self {
        self.noise(pattern, Some(secret), Some(peer))
    }

    fn noise(mut self, pattern: i32, secret: Option<&[u8; KEY]>, peer: Option<&[u8; KEY]>) -> Self {
        self.pattern = pattern;
        self.static_secret = secret.copied();
        self.static_public = std::sync::OnceLock::new();
        self.peer = peer.copied();
        self
    }

    /// Hears the key the other side has proved; false - the connection ends. For the patterns that
    /// bring it.
    pub(crate) fn on_peer(mut self, on_peer: impl Fn(&[u8; KEY]) -> bool + Send + Sync + 'static) -> Self {
        self.on_peer = Some(Arc::new(on_peer));
        self
    }

    /// A chain a host or a client chose in code: TLS alone. Noise is of the protocol layer.
    pub(crate) fn tls_only(&self) {
        assert!(self.pattern < 0, "Noise of a connection is set by [Noise] of its description, not in code");
    }

    /// After how many records this end renews its key of TLS and asks the peer to renew its own
    /// (KeyUpdate); 0, by default - when the library sees fit. For tests, mostly.
    pub fn tls_tx_limit(mut self, records: i64) -> Self {
        self.tx_limit = records;
        self
    }

    /// A Noise stream of this chain, for a new socket.
    pub(crate) fn noise_stream(&self, initiator: bool, wake: Option<Box<dyn FnMut() + Send>>) -> NoiseStream {
        NoiseStream::of(self.handshake(initiator, STREAM_PROLOGUE), initiator, wake, self.on_peer.clone())
    }

    /// The handshake of this chain's pattern for the end `initiator` names, bound to `prologue`. Of the keys given it takes what
    /// the pattern has that end know: NK - the initiator the peer's only, the responder its own secret only; IK - the initiator
    /// both, the responder its own secret only; XX - the own secret only; KK - both.
    pub(crate) fn handshake(&self, initiator: bool, prologue: &[u8]) -> NoiseNK {
        let (secret, peer) = match self.pattern {
            PATTERN_NK if initiator => (None, self.peer.as_ref()),
            PATTERN_NK | PATTERN_XX => (self.static_secret.as_ref(), None),
            PATTERN_IK if !initiator => (self.static_secret.as_ref(), None),
            _ => (self.static_secret.as_ref(), self.peer.as_ref()),
        };
        let public = secret.map(|s| self.static_public.get_or_init(|| super::x25519::public_key(s)));
        NoiseNK::of(self.pattern, initiator, prologue, secret, public, peer, None)
    }

    /// The key the other side proved in `h`, a handshake that is done, is let in: by [`on_peer`](Self::on_peer), when the
    /// pattern brings a key at all.
    pub(crate) fn accepts(&self, h: &NoiseNK) -> bool {
        let key = h.peer_key();
        key == [0u8; KEY] || self.on_peer.as_ref().is_none_or(|f| f(&key))
    }

    /// This end of this chain has TLS.
    pub(crate) fn has_tls_for(&self, server: bool) -> bool {
        if server {
            self.tls_server != 0
        } else {
            self.tls_host.is_some()
        }
    }

    /// A new connection of TLS for this end of this chain; 0 - this chain has no TLS for it.
    fn tls_connection(&self, a: &'static Api, server: bool) -> io::Result<i64> {
        if server {
            // SAFETY: the context of this chain
            return Ok(if self.tls_server == 0 { 0 } else { unsafe { (a.tls_server)(self.tls_server) } });
        }
        let Some(host) = &self.tls_host else { return Ok(0) };
        // SAFETY: a context once a process; the name and the protocol go with their lengths
        let conn = unsafe {
            let context = *CONTEXT.get_or_init(|| (a.tls_context)());
            let alpn = self.tls_alpn.as_deref().unwrap_or(&[]);
            (a.tls_client)(context, host.as_ptr(), alpn.as_ptr(), alpn.len() as i32, verify, std::ptr::null_mut())
        };
        if conn == 0 {
            return Err(io::Error::other("no connection of TLS"));
        }
        Ok(conn)
    }
}

/// A chain on one socket: what lies between the socket and the source and the sink of the
/// connection, under any framing. From the pool of the loop, for as long as the socket.
pub(crate) struct Link {
    a: &'static Api,
    mem: Mem,
    /// The Noise stream of the connection: the chain touches it on the loop of the socket, between its receive and its transmit.
    noise: Option<Arc<super::network::noise_adapter::Shared>>,
    tls: i64,
    handshaking: bool,
    hello: bool,
    closed: bool,
    needs_control: bool,
    /// What the TLS handshake has to send, gone when it ends.
    flight: Option<Buf>,
    flight_at: usize,
    flight_end: usize,
    /// The bytes of a TLS record left in the buffer, with no plaintext after them.
    held: usize,
    // What waits for the next read lies where it came, in the same buffer, past the plaintext that
    // goes up first; the next read moves it to the front. A record of Noise cut by the end of the
    // plaintext: no call of its own for it. A TLS record not all here.
    carry_at: usize,
    carry_len: usize,
    tail_at: usize,
    tail_len: usize,
    carried: usize,
    restored: usize,
    /// A record is authenticated whole, so all of it has to lie somewhere, and it is the peer who
    /// says how long it is. A buffer of the engine that holds it does. For a longer one a whole-record
    /// buffer of the loop goes in the place of the receive buffer's storage, which waits here.
    want_big: bool,
    small: Option<Buf>,
    pieces: usize,
    piece: usize,
    pieces_end: usize,
    pieces_limit: usize,
    /// Where the data of the run in hand end; behind them, in the last run, what is left for the
    /// decoder of the WebSocket.
    pub(crate) piece_end: usize,
}

// SAFETY: on the loop of its connection alone
unsafe impl Send for Link {}

impl Link {
    pub(crate) fn new() -> Box<Self> {
        let a = api();
        // SAFETY: a call with no arguments
        let mem = Mem::new(unsafe { (a.chain_size)() } as usize);
        Box::new(Self {
            a,
            mem,
            noise: None,
            tls: 0,
            handshaking: false,
            hello: false,
            closed: false,
            needs_control: false,
            flight: None,
            flight_at: 0,
            flight_end: 0,
            held: 0,
            carry_at: 0,
            carry_len: 0,
            tail_at: 0,
            tail_len: 0,
            carried: 0,
            restored: 0,
            want_big: false,
            small: None,
            pieces: 0,
            piece: 0,
            pieces_end: 0,
            pieces_limit: 0,
            piece_end: 0,
        })
    }

    /// On a new socket: `server` - its end of `chain`; `noise` - the stream of the connection.
    pub(crate) unsafe fn attach(&mut self, lp: &Loop, server: bool, chain: Option<&Chain>, noise: Option<Arc<super::network::noise_adapter::Shared>>) -> io::Result<()> {
        self.closed = false;
        if let Some(c) = chain {
            self.tls = c.tls_connection(self.a, server)?;
        }
        if self.tls != 0 {
            if let Some(limit) = chain.map(|c| c.tx_limit).filter(|&l| l != 0) {
                (self.a.tls_tx_limit)(self.tls, limit);
            }
            self.handshaking = true;
            self.hello = false;
            self.flight = Some(lp.take_record());
        }
        if let Some(n) = &noise {
            n.stream().link(self.mem.ptr(), lp);
        }
        self.noise = noise;
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn has_tls(&self) -> bool {
        self.tls != 0
    }

    /// TLS has not got through yet: the session has not started.
    #[inline(always)]
    pub(crate) fn handshaking(&self) -> bool {
        self.handshaking
    }

    /// Something waits in the buffer for the next read: the buffer stays.
    #[inline(always)]
    fn keeps(&self) -> bool {
        self.carry_len != 0 || self.tail_len != 0
    }

    /// The most that is read into a buffer of `len` bytes.
    #[inline(always)]
    pub(crate) fn window(len: usize) -> usize {
        len.min(OFFSET)
    }

    /// A record of `length` bytes is on its way, and the next read has to add to what waits: when
    /// that does not fit, a whole one.
    #[inline]
    unsafe fn fit(&mut self, p: *mut Slot, length: usize) {
        if self.small.is_none() && engine::rbuf(p).len() < length {
            self.want_big = true;
        }
    }

    #[inline]
    unsafe fn tls_length(p: *mut Slot, at: usize, have: usize) -> usize {
        if have < TLS_HEADER {
            return 0;
        }
        TLS_HEADER + ((engine::rbyte(p, at + 3) as usize) << 8 | engine::rbyte(p, at + 4) as usize)
    }

    /// Before a read: the storage of the receive buffer as it has to be for it. A whole-record
    /// buffer goes in for the record on its way, with what waits; one that is empty goes back.
    pub(crate) unsafe fn before_read(&mut self, p: *mut Slot) {
        let rb = engine::rbuf(p);
        if self.want_big {
            self.want_big = false;
            let lp = engine::lp(p);
            let big = lp.take_record();
            let keeps = self.keeps();
            let from = rb.as_ptr();
            let mut to = big;
            if keeps {
                to[..self.carry_len].copy_from_slice(std::slice::from_raw_parts(from.add(self.carry_at), self.carry_len));
                to[self.carry_len..self.carry_len + self.tail_len].copy_from_slice(std::slice::from_raw_parts(from.add(self.tail_at), self.tail_len));
                self.carry_at = 0;
                self.tail_at = self.carry_len;
            } else {
                to[..(*p).r_end].copy_from_slice(std::slice::from_raw_parts(from, (*p).r_end));
            }
            self.small = Some(rb.swap(to));
        } else {
            self.big_done(p);
        }
    }

    /// The whole-record buffer is empty: it goes back, the storage of the connection comes back.
    #[inline]
    pub(crate) unsafe fn big_done(&mut self, p: *mut Slot) {
        if self.small.is_some() && (*p).r_end == 0 && !self.keeps() {
            let big = engine::rbuf(p).swap(self.small.take().unwrap());
            engine::lp(p).give_record(big);
        }
    }

    /// Fills the transmit buffer in hand, whatever is to go next: the handshake, a record of the
    /// connection's own, the data through the chain. false - nothing to send.
    pub(crate) unsafe fn transmit(&mut self, p: *mut Slot) -> io::Result<bool> {
        let mem = self.mem.ptr();
        put(mem, OPS, 0);
        if self.tls == 0 {
            let filled = engine::fill_framed(p);
            if get(mem, OPS) != 0 {
                self.run()?;
            }
            return Ok(filled);
        }
        if self.handshaking && !self.hello {
            self.hello = true;
            self.handshake(std::ptr::null_mut(), 0)?;
        }
        if self.flight_at < self.flight_end {
            // in as many buffers as it takes
            let buf = (*p).tbuf.as_mut().unwrap();
            let n = (self.flight_end - self.flight_at).min(buf.len());
            buf[..n].copy_from_slice(&self.flight.as_ref().unwrap()[self.flight_at..self.flight_at + n]);
            self.flight_at += n;
            (*p).t_at = 0;
            (*p).t_end = n;
            return Ok(true);
        }
        if self.handshaking {
            (*p).held = true; // a graceful close waits for the handshake: there is nothing to close before it
            return Ok(false);
        }
        if let Some(f) = self.flight.take() {
            engine::lp(p).give_record(f);
        }
        if self.needs_control {
            self.needs_control = false; // first: the next one sets it again
            let buf = (*p).tbuf.as_mut().unwrap();
            let n = (self.a.tls_control)(self.tls, buf.as_mut_ptr() as i64);
            if n < 0 {
                return Err(self.error());
            }
            (*p).t_at = 0;
            (*p).t_end = n as usize;
            return Ok(0 < n);
        }
        if !engine::fill_framed(p) {
            return Ok(false);
        }
        // The framing left its bytes in [t_at, t_end), the room of the header before them.
        let (at, n) = ((*p).t_at, (*p).t_end - (*p).t_at);
        if n == 0 {
            return Ok(false);
        }
        let record = (*p).tbuf.as_mut().unwrap().as_mut_ptr().add(at - TLS_HEADER);
        op(mem, OP_TLS, record as i64, n, 0, 0);
        let r = self.run()?;
        self.needs_control = get(mem, TLS_FLAGS) & 2 != 0;
        (*p).t_at = at - TLS_HEADER;
        (*p).t_end = (*p).t_at + r;
        Ok(true)
    }

    /// The payload of a frame, to be masked with the rest of this buffer's chain.
    #[inline]
    pub(crate) unsafe fn op_mask(&mut self, address: *mut u8, len: usize, key: u32) {
        op(self.mem.ptr(), OP_MASK, address as i64, len, key as i64, 0);
    }

    #[inline]
    unsafe fn run(&mut self) -> io::Result<usize> {
        let r = (self.a.chain_tx)(self.mem.at());
        if r < 0 {
            return Err(io::Error::other("seal"));
        }
        Ok(r as usize)
    }

    #[cold]
    unsafe fn error(&self) -> io::Error {
        let e = (self.a.tls_error)(self.tls);
        io::Error::other(if e.is_null() {
            "TLS".to_string()
        } else {
            std::ffi::CStr::from_ptr(e).to_string_lossy().into_owned()
        })
    }

    /// What came goes through the handshake: how much of it was taken.
    unsafe fn handshake(&mut self, input: *mut u8, len: usize) -> io::Result<usize> {
        let flight = self.flight.as_mut().unwrap();
        if self.flight_at != 0 {
            flight.copy_within(self.flight_at..self.flight_end, 0);
            self.flight_end -= self.flight_at;
            self.flight_at = 0;
        }
        let room = flight.len() - self.flight_end;
        if room < FLIGHT_ROOM {
            return Err(io::Error::other("the handshake of TLS has too much to send"));
        }
        let r = (self.a.tls_handshake)(self.tls, input as i64, len as i32, flight.as_mut_ptr().add(self.flight_end) as i64, room as i32);
        self.flight_end += (r & 0xFF_FFFF) as usize;
        let status = r >> 48;
        if status == 2 {
            return Err(self.error());
        }
        if status == 0 {
            self.handshaking = false;
            put64(self.mem.ptr(), TLS, self.tls);
        }
        Ok((r >> 24 & 0xFF_FFFF) as usize)
    }

    /// Where what the framing left in the buffer ends: what [`receiving`](Self::receiving) put
    /// there before a read that brought nothing is not of it.
    #[inline(always)]
    pub(crate) fn at(&self, end: usize) -> usize {
        end - self.restored
    }

    /// Before a read: what was held back goes into the buffer first.
    pub(crate) unsafe fn receiving(&mut self, p: *mut Slot) {
        let base = engine::rbuf(p).as_ptr();
        if !self.keeps() {
            if self.restored == 0 && (*p).r_end == 0 {
                if let Some(n) = &self.noise {
                    if (*p).ws.as_ref().is_none_or(|w| w.chain_state() != WS_OTHER) {
                        // what the slow way holds of a record goes in front of this read: the chain
                        // opens it and all behind it in one call, and the record buffer goes back
                        self.carried = n.stream().give_back(std::slice::from_raw_parts_mut(base, Self::window(engine::rbuf(p).len())));
                        self.restored = self.carried;
                        (*p).r_end = self.carried;
                    }
                }
            }
            return;
        }
        // To the front, what comes first first: nothing above it is needed any more.
        let to = (*p).r_end;
        if self.carry_len != 0 {
            std::ptr::copy(base.add(self.carry_at), base.add(to), self.carry_len);
        }
        if self.tail_len != 0 {
            std::ptr::copy(base.add(self.tail_at), base.add(to + self.carry_len), self.tail_len);
        }
        self.carried = self.carry_len;
        self.restored = self.carry_len + self.tail_len;
        (*p).r_end = to + self.restored;
        self.carry_len = 0;
        self.tail_len = 0;
    }

    /// `[0, r_end)` is what is to be read: `at` bytes the framing left there, then what came from the
    /// socket. Ok(true) - `[r_at, r_end)` is what to take up; Ok(false) - nothing to take up, the
    /// buffer is ready for the next read.
    pub(crate) unsafe fn received(&mut self, p: *mut Slot, mut at: usize) -> io::Result<bool> {
        self.restored = 0;
        let limit = (*p).r_end;
        at -= self.held; // what lies behind the plaintext is the part of a record held from the last read
        self.held = 0;
        if !self.handshaking {
            return self.open(p, at, limit);
        }
        let base = engine::rbuf(p).as_ptr();
        let taken = self.handshake(base, limit)?;
        if self.flight_at < self.flight_end {
            engine::wake_on_loop(p);
        }
        let rest = limit - taken;
        if taken != 0 {
            std::ptr::copy(base.add(taken), base, rest);
        }
        (*p).r_at = 0;
        (*p).r_end = rest;
        if self.handshaking {
            // a part of a record, or none: it stays where it is and the next read appends
            self.held = rest;
            self.fit(p, Self::tls_length(p, 0, rest));
            return Ok(false);
        }
        if (*p).ws.is_none() {
            engine::session_started(p);
            engine::report(p, Ok(())); // whoever waits for the connect has its answer
            if !engine::active(p) {
                return Ok(false);
            }
        }
        engine::wake_on_loop(p); // the framing can start now
        if !engine::active(p) {
            return Ok(false);
        }
        if rest == 0 {
            return Ok(false);
        }
        self.open(p, 0, rest)
    }

    unsafe fn open(&mut self, p: *mut Slot, at: usize, limit: usize) -> io::Result<bool> {
        let mem = self.mem.ptr();
        // what the layers above know, where the native code reads it
        let mut flags = 0;
        let mut framing = WS_NEW_FRAME;
        if let Some(w) = (*p).ws.as_ref() {
            flags = if w.is_client() { F_WS | F_WS_CLIENT } else { F_WS };
            framing = w.chain_state();
            put(mem, WS_STATE, if at != 0 { WS_OTHER } else { framing });
            let (left, mask, pos) = w.chain_view();
            put(mem, WS_LEFT, left);
            put(mem, WS_MASK, mask);
            put(mem, WS_MASK_POS, pos);
        }
        // Behind the frames only when no decoded payload waits; with no frames, right from the start.
        let plain = at == 0 && framing != WS_OTHER;
        let mut aligned = false;
        if plain {
            if let Some(n) = &self.noise {
                let n = n.stream();
                if n.aligned() {
                    aligned = true;
                    put64(mem, RX_CTX, n.rx_context());
                    put64(mem, RX_N, n.rx_number() as i64);
                    flags |= F_NOISE;
                }
            }
            // In one piece for what reads the buffer from its very start: what the framing left, the
            // upgrade, the decoder.
            flags |= F_IN_PIECES;
        }
        put(mem, FLAGS, flags);
        let from = self.carried;
        self.carried = 0;
        let base = engine::rbuf(p).as_ptr();
        let r = (self.a.chain_rx)(self.mem.at(), base, (at + from) as i32, limit as i32);
        if r < 0 {
            self.needs_control = true;
            return Err(self.error());
        }
        let consumed = (r >> 42) as usize;
        let mut raw_end = (r >> 21) as usize & OFFSET;
        let mut ws_end = r as usize & OFFSET;
        if self.tls != 0 {
            let f = get(mem, TLS_FLAGS);
            self.needs_control = f & 2 != 0;
            if self.needs_control {
                engine::wake_on_loop(p);
                if !engine::active(p) {
                    return Ok(false);
                }
            }
        }
        // Only with nothing behind it for the decoder: that would have to come first.
        let whole = if aligned { get(mem, NOISE_END) } else { -1 };
        let whole = if whole < 0 { ws_end } else { whole as usize };
        if aligned && ws_end == raw_end && whole < ws_end {
            self.carry_at = whole;
            self.carry_len = ws_end - whole;
            // Under TLS a whole TLS record may come behind it; without, what is left of its own.
            let length = if self.tls != 0 {
                RECORD_BUFFER
            } else {
                2 + 16
                    + if self.carry_len < 2 {
                        0
                    } else {
                        engine::rbyte(p, whole) as usize | (engine::rbyte(p, whole + 1) as usize) << 8
                    }
            };
            self.fit(p, length);
            ws_end = whole;
            raw_end = whole;
        }
        self.pieces = get(mem, PIECES) as usize;
        self.piece = 0;
        self.pieces_end = ws_end;
        self.pieces_limit = raw_end;
        let first_end = if self.pieces < 2 { ws_end } else { get(mem, PIECE + 4) as usize };
        if plain {
            if let Some(w) = (*p).ws.as_mut() {
                w.chain_decoded(get(mem, WS_STATE), get(mem, WS_LEFT), get(mem, WS_MASK), get(mem, WS_MASK_POS), first_end);
            }
        }
        let rest = limit - consumed;
        self.fit(p, Self::tls_length(p, consumed, rest));
        let start = get(mem, PIECE) as usize;
        let nothing = self.pieces < 2 && raw_end == start; // no run, or an empty one
        if nothing && self.carry_len == 0 {
            // no plaintext at all: what is left stays right behind what the framing left, the next
            // read appends
            std::ptr::copy(base.add(consumed), base.add(at), rest);
            (*p).r_at = 0;
            (*p).r_end = at + rest;
            self.held = rest;
            return Ok(false);
        }
        if 0 < rest {
            // plaintext goes up first: the piece of the next record stays behind it, untouched, and
            // the buffer stays
            self.tail_at = consumed;
            self.tail_len = rest;
        }
        if aligned {
            if let Some(n) = &self.noise {
                n.stream().opened_ahead(get(mem, NOISE_BYTES) as usize, get(mem, NOISE_COUNT) as u64);
            }
        }
        if nothing {
            // all of it waits for the next read
            (*p).r_at = 0;
            (*p).r_end = 0;
            return Ok(false);
        }
        self.piece_end = first_end;
        (*p).r_at = start;
        (*p).r_end = if self.pieces < 2 { raw_end } else { first_end };
        Ok(true)
    }

    /// The run in hand is taken up: `[r_at, r_end)` is at the next one. false - no more.
    #[inline]
    pub(crate) unsafe fn next_piece(&mut self, p: *mut Slot) -> bool {
        let mem = self.mem.ptr();
        loop {
            self.piece += 1;
            if self.pieces <= self.piece {
                return false;
            }
            let last = self.piece + 1 == self.pieces;
            let at = get(mem, PIECE + self.piece * 8) as usize;
            let end = if last { self.pieces_end } else { get(mem, PIECE + self.piece * 8 + 4) as usize };
            let limit = if last { self.pieces_limit } else { end };
            if at == limit {
                continue;
            }
            (*p).r_at = at;
            (*p).r_end = limit;
            self.piece_end = end;
            return true;
        }
    }

    /// The FIN is about to go: close_notify, when the socket takes it at once. What is closing does
    /// not wait.
    pub(crate) unsafe fn closing(&mut self, stream: &mio::net::TcpStream) {
        if self.tls == 0 || self.handshaking || self.closed {
            return;
        }
        self.closed = true;
        let mut alert = [0u8; CONTROL_RECORD];
        (self.a.tls_close_notify)(self.tls);
        let n = (self.a.tls_control)(self.tls, alert.as_mut_ptr() as i64);
        if 0 < n {
            let _ = io::Write::write(&mut &*stream, &alert[..n as usize]);
        }
    }

    /// The connection is about to be aborted for a failure of this chain: the last it has to say.
    pub(crate) unsafe fn failed(&mut self, stream: &mio::net::TcpStream) {
        if self.flight_at < self.flight_end {
            let _ = io::Write::write(&mut &*stream, &self.flight.as_ref().unwrap()[self.flight_at..self.flight_end]);
            self.flight_at = self.flight_end;
        } else if self.tls != 0 && self.needs_control && !self.handshaking {
            self.needs_control = false;
            let mut alert = [0u8; CONTROL_RECORD];
            let n = (self.a.tls_control)(self.tls, alert.as_mut_ptr() as i64);
            if 0 < n {
                let _ = io::Write::write(&mut &*stream, &alert[..n as usize]);
            }
        }
    }

    /// The socket is gone: all clean, for the next connection.
    pub(crate) unsafe fn close(&mut self, p: *mut Slot) {
        let lp = engine::lp(p);
        if let Some(f) = self.flight.take() {
            lp.give_record(f);
        }
        if let Some(small) = self.small.take() {
            if !(*p).rb.is_null() {
                lp.give_record(engine::rbuf(p).swap(small));
            }
        }
        if self.tls != 0 {
            (self.a.tls_free)(self.tls);
            self.tls = 0;
        }
        if let Some(n) = self.noise.take() {
            let n = n.stream();
            n.release();
            n.link(std::ptr::null_mut(), std::ptr::null());
        }
        std::ptr::write_bytes(self.mem.ptr(), 0, self.mem.len());
        self.handshaking = false;
        self.hello = false;
        self.needs_control = false;
        self.want_big = false;
        self.flight_at = 0;
        self.flight_end = 0;
        self.held = 0;
        self.carry_at = 0;
        self.carry_len = 0;
        self.tail_at = 0;
        self.tail_len = 0;
        self.carried = 0;
        self.restored = 0;
        self.pieces = 0;
        self.piece = 0;
        self.pieces_end = 0;
        self.pieces_limit = 0;
        self.piece_end = 0;
    }
}

/// The keys of the Envelope stages of a connection ( `[Envelope]` on a pack ): what the packs it sends are sealed and signed with,
/// what the packs it takes are opened and checked with. Set before the packs go or come; the transmitter and the receiver of the
/// connection read them at the first bytes of each pack, with no lock.
#[derive(Default)]
pub struct Envelope {
    pub(crate) tx: arc_swap::ArcSwapOption<Pack>,
    pub(crate) rx: arc_swap::ArcSwapOption<Pack>,
}

impl Envelope {
    /// What the packs this side sends are sealed and signed with: [`Pack::pair`], [`Pack::signed_by`].
    pub fn transmitter(&self, pack: Pack) {
        self.tx.store(Some(Arc::new(pack)));
    }

    /// What the packs this side takes are opened and checked with: [`Pack::pair`], [`Pack::signer_of`].
    pub fn receiver(&self, pack: Pack) {
        self.rx.store(Some(Arc::new(pack)));
    }
}

/// A pack on its own, what the session's transport has nothing to do with: signed by its author
/// (E7), sealed under the key of the pair (E6), any of the two, in one call. On the wire: `[id 4]
/// [number 8]`, in clear and authenticated, then the text, the signature of 64 bytes, the tag of 16.
/// What is chosen must be the same on both ends.
pub struct Pack {
    /// The secret of signing at 0, the public key at 64, the context of the pair at 128.
    state: Mem,
    pub(crate) ctx: i64,
    pub(crate) secret: i64,
    pub(crate) public: i64,
    /// The number of the next pack: a random point of the 64-bit space, counted up.
    pub(crate) number: std::sync::atomic::AtomicU64,
    pub(crate) id: i32,
}

impl Default for Pack {
    fn default() -> Self {
        Self::new()
    }
}

impl Pack {
    pub const HEADER: usize = 12;
    pub const SIGNATURE: usize = 64;
    pub const TAG: usize = 16;

    pub fn new() -> Self {
        // SAFETY: a call with no arguments
        let state = Mem::new(128 + unsafe { (api().aead_size)() } as usize);
        Self {
            state,
            ctx: 0,
            secret: 0,
            public: 0,
            number: std::sync::atomic::AtomicU64::new(super::chacha20::Core::next_seq()),
            id: 0,
        }
    }

    /// Sealed under the key of this pair, for what it is used: both ends get the same one, and the
    /// key and its base nonce stay inside.
    pub fn pair(mut self, my_secret: &[u8; KEY], their_public: &[u8; KEY], purpose: &str) -> Self {
        let a = api();
        let (mut key, mut iv, mut id) = ([0u8; 32], [0u8; 12], 0i32);
        // SAFETY: the sizes the call takes; the context fits the memory
        unsafe {
            assert!(
                (a.pair_key)(
                    key.as_mut_ptr(),
                    iv.as_mut_ptr(),
                    &mut id,
                    my_secret.as_ptr(),
                    their_public.as_ptr(),
                    purpose.as_ptr(),
                    purpose.len() as i32
                ) != 0,
                "A key of a low order."
            );
            self.ctx = self.state.at() + 128;
            assert!((a.aead_init)(0, self.ctx as *mut u8, key.as_ptr(), 32) != 0, "aead");
        }
        super::noise::wipe(&mut key);
        self.id = id;
        self
    }

    /// Signed by this key: 64 bytes of secret, see [`crate::lib::crypto::ed25519_keypair`].
    pub fn signed_by(mut self, signing_secret: &[u8; 64]) -> Self {
        // SAFETY: inside the state
        unsafe { std::ptr::copy_nonoverlapping(signing_secret.as_ptr(), self.state.ptr(), 64) };
        self.secret = self.state.at();
        self
    }

    /// Must be signed by the owner of this key.
    pub fn signer_of(mut self, signing_public: &[u8; 32]) -> Self {
        // SAFETY: inside the state
        unsafe { std::ptr::copy_nonoverlapping(signing_public.as_ptr(), self.state.ptr().add(64), 32) };
        self.public = self.state.at() + 64;
        self
    }

    /// The number of the next pack: each one its own.
    #[inline]
    pub(crate) fn next(&self) -> u64 {
        self.number.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// What a pack of `len` bytes of text takes.
    pub fn size(&self, len: usize) -> usize {
        Self::HEADER + len + if self.secret != 0 { Self::SIGNATURE } else { 0 } + if self.ctx != 0 { Self::TAG } else { 0 }
    }

    /// The text at `buf[from + HEADER]` of `len` bytes, with room for [`size`](Self::size) in `buf`:
    /// sealed in place. The whole length.
    pub fn seal(&mut self, buf: &mut [u8], from: usize, len: usize) -> usize {
        assert!(from + self.size(len) <= buf.len(), "the room of a pack");
        let n = self.next();
        buf[from..from + 4].copy_from_slice(&self.id.to_le_bytes());
        buf[from + 4..from + 12].copy_from_slice(&n.to_le_bytes());
        // SAFETY: the bytes are checked above
        let r = unsafe { (api().pack_seal)(self.ctx, n as i64, self.secret, buf.as_mut_ptr().add(from) as i64, Self::HEADER as i32, len as i32) };
        assert!(0 <= r, "seal");
        r as usize
    }

    /// The pack of `total` bytes at `buf[from]`, opened in place. The length of the text at `from +
    /// HEADER`; `None` - forged, or not of its signer.
    pub fn open(&self, buf: &mut [u8], from: usize, total: usize) -> Option<usize> {
        if total < Self::HEADER || buf.len() < from + total {
            return None;
        }
        let n = u64::from_le_bytes(buf[from + 4..from + 12].try_into().unwrap());
        // SAFETY: the bytes are checked above
        let r = unsafe { (api().pack_open)(self.ctx, n as i64, self.public, buf.as_mut_ptr().add(from) as i64, Self::HEADER as i32, total as i32) };
        (0 <= r).then_some(r as usize)
    }
}
