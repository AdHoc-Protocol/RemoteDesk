// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! WebSocket framing and upgrade negotiation over the TCP engine (RFC 6455).
//!
//! Server and client share a frame state machine and control-frame handling. The client initiates
//! the upgrade and masks outgoing payloads; the server validates and answers it.
//!
//! Incoming payloads are unmasked in the receive buffer. A refused tail remains there until the
//! sink is ready, without decrypting or unmasking it twice. Outgoing frames reserve header space
//! before serializing payload directly into the transmit buffer.

#![allow(dangerous_implicit_autorefs)] // the slots and the state of a loop are reached through raw pointers on purpose: a handler up the
                                       // stack may hold them

use std::sync::atomic::Ordering;

use super::super::engine::{self, active, Slot};
use super::external_connection::{Event, ExternalConnection};
use std::sync::Arc;

pub mod client;
pub use client::Client;

/// The least buffer a WebSocket works with: its upgrade request and its 101 go out whole, each in
/// one buffer.
pub const MIN_BUFFER: usize = 256;

/// The engine of the WebSocket host `name` has the buffers a WebSocket needs.
pub(crate) fn fits(engine: &super::super::engine::Engine, name: &str) -> std::io::Result<()> {
    if engine.buffer_size < MIN_BUFFER {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "A WebSocket needs buffers of {MIN_BUFFER} bytes at least: its upgrade request and its 101 go out whole, each in one buffer. The buffers of the engine of {name} are {} bytes.",
                engine.buffer_size
            ),
        ));
    }
    Ok(())
}

mod state {
    pub const HANDSHAKE: u8 = 0;
    pub const NEW_FRAME: u8 = 1;
    pub const PAYLOAD_LENGTH_BYTE: u8 = 2;
    pub const PAYLOAD_LENGTH_BYTES: u8 = 3;
    pub const XOR0: u8 = 4;
    pub const XOR3: u8 = 7;
    pub const DATA: u8 = 8; // unmasking the payload of a data frame (BINARY, TEXT, CONTINUATION)
    pub const CONTROL: u8 = 9; // collecting the payload of a PING or CLOSE, answered once it is complete
    pub const DISCARD: u8 = 10; // skipping the payload of a PONG
}

mod op {
    pub const BINARY: u8 = 0x02;
    pub const CLOSE: u8 = 0x08;
    pub const PING: u8 = 0x09;
    pub const PONG: u8 = 0x0A;
}

const FIN: u8 = 0x80;
const MASK: u8 = 0x80;
const LEN: u8 = 0x7F;

/// The GUID of the handshake (RFC 6455 §1.3).
const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const UPGRADE: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nServer: AdHoc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: ";

/// A control frame: its opcode and payload (at most 125 bytes).
#[derive(Clone)]
pub struct Control {
    pub op: u8,
    pub len: u8,
    pub data: [u8; 125],
}

impl Control {
    pub fn new(op: u8, payload: &[u8]) -> Self {
        let mut c = Control {
            op,
            len: payload.len().min(125) as u8,
            data: [0; 125],
        };
        c.data[..c.len as usize].copy_from_slice(&payload[..c.len as usize]);
        c
    }

    /// A CLOSE with a status code and a reason, cut to its first 123 bytes at a character boundary:
    /// the reason of a CLOSE is UTF-8, a cut sequence would make the peer fail the connection (RFC
    /// 6455 §5.5.1).
    pub fn close(code: u16, why: &str) -> Self {
        let mut c = Control {
            op: op::CLOSE,
            len: 2,
            data: [0; 125],
        };
        c.data[0] = (code >> 8) as u8;
        c.data[1] = code as u8;
        let w = why.as_bytes();
        let mut n = w.len().min(123);
        while !why.is_char_boundary(n) {
            n -= 1;
        }
        c.data[2..2 + n].copy_from_slice(&w[..n]);
        c.len += n as u8;
        c
    }

    pub fn ping(payload: &[u8]) -> Self {
        Self::new(op::PING, payload)
    }

    #[inline]
    pub(crate) fn is_close(&self) -> bool {
        self.op == op::CLOSE
    }
}

/// The WebSocket state of a connection. Loop-thread only.
pub struct Codec {
    client: bool,
    state: u8,
    opcode: u8,
    byte: u8,
    length_bytes: u8,
    frame_left: u64,
    mask: u32,
    mask_pos: u32,
    /// Every byte of the receive buffer before it has been through the decoder; between the read
    /// position and it lies only payload the sink has not taken yet.
    decoded: usize,
    /// Client: its upgrade request, and how much of it is out. Server: how much of its 101 is out -
    /// the 101 is made from `accept` straight into the transmit buffer, nothing is kept for it.
    hs: Vec<u8>,
    /// Server: the key of the request was read, its 101 is due.
    answer: bool,
    /// The answer to a PING or a CLOSE of the peer, assembled while its payload comes in: made with
    /// the first such frame, a connection that never hears one holds none.
    reply: Option<Box<Control>>,
    reply_ready: bool,
    sent_closing_frame: bool,
    /// The peer started the closing handshake: once our answer is out, the TCP connection closes.
    peer_closing: bool,
    /// The `Sec-WebSocket-Accept`: the one the server has to answer this client with; the one this
    /// server answers with.
    accept: [u8; 28],
    /// Client: what the lines of the server's answer read so far told (`HEARD_*`).
    heard: u8,
    /// Client: the masking keys, xorshift32; zero - masking is off.
    keys: u32,
}

impl Codec {
    /// The server end: it waits for the upgrade request.
    pub fn server() -> Box<Codec> {
        Box::new(Self::new(false))
    }

    /// The client end: its upgrade request for `host` and `target` is the first thing it sends.
    /// `masking` off leaves the key zero - frames stay well-formed and the pass over the payload is
    /// skipped.
    pub fn client(host: &str, target: &str, masking: bool) -> Box<Codec> {
        let mut ws = Box::new(Self::new(true));
        let mut seed = seed();
        let mut nonce = [0u8; 16];
        for b in nonce.iter_mut() {
            seed = xorshift(seed);
            *b = seed as u8;
        }
        let mut key = [0u8; 24];
        base64(&nonce, &mut key);
        let mut hs = Vec::with_capacity(160 + host.len() + target.len());
        hs.extend_from_slice(b"GET ");
        hs.extend_from_slice(if target.is_empty() { b"/" } else { target.as_bytes() });
        hs.extend_from_slice(b" HTTP/1.1\r\nHost: ");
        hs.extend_from_slice(host.as_bytes());
        hs.extend_from_slice(b"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: ");
        hs.extend_from_slice(&key);
        hs.extend_from_slice(b"\r\n\r\n");
        ws.hs = hs;
        accept_of(&key, &mut ws.accept);
        ws.keys = if masking { xorshift(seed) | 1 } else { 0 };
        ws
    }

    fn new(client: bool) -> Self {
        Codec {
            client,
            state: state::HANDSHAKE,
            opcode: 0,
            byte: 0,
            length_bytes: 0,
            frame_left: 0,
            mask: 0,
            mask_pos: 0,
            decoded: 0,
            hs: Vec::new(),
            answer: false,
            reply: None,
            reply_ready: false,
            sent_closing_frame: false,
            peer_closing: false,
            accept: [0; 28],
            heard: 0,
            keys: 0,
        }
    }

    pub(crate) fn dispose(self: Box<Self>, _l: &engine::Loop) {}

    #[inline(always)]
    pub(crate) fn is_client(&self) -> bool {
        self.client
    }

    /// Where the decoder stands, in the words of the chain: at a new frame, in the data of one, or
    /// anywhere else - that goes through [`decode`].
    #[inline(always)]
    pub(crate) fn chain_state(&self) -> i32 {
        match self.state {
            state::NEW_FRAME => crate::lib::chain::WS_NEW_FRAME,
            state::DATA => crate::lib::chain::WS_DATA,
            _ => crate::lib::chain::WS_OTHER,
        }
    }

    /// What is left of the frame, its key, where in the key it stands.
    #[inline(always)]
    pub(crate) fn chain_view(&self) -> (i32, i32, i32) {
        (self.frame_left as i32, self.mask as i32, self.mask_pos as i32)
    }

    /// The chain has cut the frames: payload up to `end`, and this is where the decoder stands
    /// after them.
    #[inline]
    pub(crate) fn chain_decoded(&mut self, chain_state: i32, left: i32, mask: i32, mask_pos: i32, end: usize) {
        self.state = if chain_state == crate::lib::chain::WS_NEW_FRAME { state::NEW_FRAME } else { state::DATA };
        self.frame_left = left as u64;
        self.mask = mask as u32;
        self.mask_pos = mask_pos as u32;
        self.decoded = end;
    }

    #[inline]
    fn next_key(&mut self) -> u32 {
        if self.keys == 0 {
            return 0;
        }
        self.keys = xorshift(self.keys);
        self.keys
    }
}

#[inline]
fn xorshift(mut k: u32) -> u32 {
    k ^= k << 13;
    k ^= k >> 17;
    k ^= k << 5;
    k
}

fn seed() -> u32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let a = &t as *const u64 as u64;
    let mut z = t ^ a.rotate_left(32) ^ 0x9E37_79B9_7F4A_7C15;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    let s = (z ^ (z >> 31)) as u32;
    if s == 0 {
        1
    } else {
        s
    }
}

#[inline]
unsafe fn ws<'a>(p: *mut Slot) -> &'a mut Codec {
    (*p).ws.as_mut().unwrap()
}

#[inline]
unsafe fn fire(p: *mut Slot, e: Event) {
    (*p).ext.fire(e);
}

// receiving

/// One drain pass over the receive buffer: until the upgrade is done the HTTP of the handshake,
/// then frames - every payload span offered to the sink as soon as it is unmasked.
pub(crate) unsafe fn receive(p: *mut Slot) {
    if ws(p).state == state::HANDSHAKE {
        let done = if ws(p).client { upgraded(p) } else { upgrade(p) };
        if !done {
            (*p).pending_tail = false; // the request is incomplete (kept at the front of the buffer) or refused
            return;
        }
    }
    loop {
        let pos = (*p).r_at;
        let decoded = ws(p).decoded;
        if pos < decoded {
            // decoded payload the sink has not taken yet
            let r = match (*p).sink.as_mut() {
                Some(s) => {
                    // a region that runs to the end of the bytes received may take the storage over: the frame
                    // headers before it are parsed, its payload unmasked in place, nothing behind it is the
                    // connection's; frames behind it - the storage is the connection's
                    engine::rbuf(p).set_filled(if decoded == (*p).r_end && (*p).link.is_null() { decoded } else { 0 });
                    s.on_block(engine::rbuf(p), pos, decoded) // unmasked in place
                }
                None => Some(decoded - pos),
            };
            if !active(p) {
                return;
            }
            match r {
                None => {
                    (*p).pending_tail = false;
                    return engine::reject(p);
                }
                Some(0) => return, // refused: parked here, the next pass offers the same bytes
                Some(n) => {
                    (*p).r_at = pos + n;
                    continue;
                }
            }
        }
        if (*p).r_at == (*p).r_end {
            if engine::run_over(p) {
                ws(p).decoded = (*(*p).link).piece_end; // the decoded data of the next run of the chain
                continue;
            }
            ws(p).decoded = 0; // all taken
            return;
        }
        if !decode(p) {
            // the connection is closing: nothing after this frame is read
            if active(p) {
                (*p).r_at = 0;
                (*p).r_end = 0;
                ws(p).decoded = 0;
            }
            (*p).pending_tail = false;
            return;
        }
    }
}

/// Server: the HTTP upgrade. True once the request is complete and answered, the read position at
/// whatever the client pipelined behind it; false while it is incomplete (complete lines are parsed
/// and dropped, the partial one moves to the front and the next read appends to it) or when it
/// failed.
unsafe fn upgrade(p: *mut Slot) -> bool {
    let limit = (*p).r_end;
    let buf = engine::rbytes(p, 0, engine::rbuf(p).len()); // the handshake
                                                           // FORWARD for the end of the header: a client may pipeline frames behind it, the first CRLFCRLF
                                                           // is the header's. A blank line at the very start ends it as well: the lines before it went
                                                           // already.
    let bare = 2 <= limit && buf[0] == b'\r' && buf[1] == b'\n';
    let mut i: isize = if bare { -2 } else { 0 };
    while i <= limit as isize - 4 {
        if (bare && i == -2) || (0 <= i && &buf[i as usize..i as usize + 4] == b"\r\n\r\n") {
            let end = i.max(0) as usize;
            if !ws(p).answer {
                parse_key(ws(p), &buf[..end]);
            }
            if !ws(p).answer {
                fire(p, Event::WEBSOCKET_HANDSHAKE_FAILURE);
                (*p).ext.failure(&crate::lib::network::failure::protocol(format!(
                    "Sec-WebSocket-Key not found in header. Handshake failed: {}",
                    String::from_utf8_lossy(&buf[..end])
                )));
                engine::disconnect(p, Event::THIS_CLOSE_ABRUPTLY);
                return false;
            }
            ws(p).state = state::NEW_FRAME;
            engine::wake_on_loop(p); // the runner sends the 101 first, then whatever was queued during the handshake
            if !active(p) {
                return false;
            }
            (*p).ext.upgrading.store(false, Ordering::Release); // the session opens with the upgrade
            fire(p, Event::WEBSOCKET_REMOTE_CONNECTED);
            if !active(p) {
                return false;
            }
            (*p).r_at = (i + 4) as usize;
            ws(p).decoded = (*p).r_at;
            return true;
        }
        i += 1;
    }
    // only complete header lines are parsed
    let Some(nl) = buf[..limit].iter().rposition(|&b| b == b'\n') else {
        if limit == buf.len() {
            fire(p, Event::WEBSOCKET_HANDSHAKE_FAILURE);
            (*p).ext
                .failure(&crate::lib::network::failure::protocol("A line of the upgrade request does not fit the buffer. Handshake failed."));
            engine::disconnect(p, Event::THIS_CLOSE_ABRUPTLY); // one header line longer than the buffer
        }
        return false;
    };
    if !ws(p).answer {
        parse_key(ws(p), &buf[..nl + 1]);
    }
    buf.copy_within(nl + 1..limit, 0); // the partial line to the front; the next read appends to it
    (*p).r_at = 0;
    (*p).r_end = limit - (nl + 1);
    false
}

/// Finds `Sec-WebSocket-Key` among header lines and stages the 101 that answers it. A value that is
/// not a key stages nothing: the end of the request then fails the handshake.
fn parse_key(ws: &mut Codec, lines: &[u8]) {
    let at = crate::lib::adhoc::utils::boyer_moore_case_insensitive(lines, &SEC_WEBSOCKET_KEY);
    if at < 0 {
        return;
    }
    let mut k = at as usize + SEC_WEBSOCKET_KEY.len(); // the byte after the name
    while k < lines.len() && (lines[k] == b' ' || lines[k] == b'\t') {
        k += 1;
    }
    let start = k;
    while k < lines.len() && lines[k] != b'\r' && lines[k] != b'\n' {
        k += 1;
    }
    let mut key = &lines[start..k];
    while let [rest @ .., b' ' | b'\t'] = key {
        key = rest;
    }
    if key.is_empty() || 125 < key.len() {
        return;
    }
    accept_of(key, &mut ws.accept);
    ws.answer = true;
}

/// The header names looked for, as Boyer-Moore patterns: made once.
static SEC_WEBSOCKET_KEY: std::sync::LazyLock<Vec<u32>> = std::sync::LazyLock::new(|| crate::lib::adhoc::utils::boyer_moore_pattern(b"Sec-WebSocket-Key:"));
static SEC_WEBSOCKET_ACCEPT: std::sync::LazyLock<Vec<u32>> = std::sync::LazyLock::new(|| crate::lib::adhoc::utils::boyer_moore_pattern(b"Sec-WebSocket-Accept:"));

/// The lines of the server's answer read: some, the first a `101`.
const HEARD_LINES: u8 = 1;
/// `Sec-WebSocket-Accept` among them, the answer to this client's key.
const HEARD_ACCEPT: u8 = 2;

/// Client: the server's answer to the upgrade request, read line by line as the server reads the
/// request - only one line has to fit the buffer. True once it is complete and right, the read
/// position at whatever the server sent behind it; false while it is incomplete or when it is not
/// the answer asked for.
unsafe fn upgraded(p: *mut Slot) -> bool {
    let limit = (*p).r_end;
    let buf = engine::rbytes(p, 0, engine::rbuf(p).len()); // the handshake
                                                           // a blank line at the very start ends the answer: the lines before it went already
    let bare = ws(p).heard & HEARD_LINES != 0 && 2 <= limit && buf[0] == b'\r' && buf[1] == b'\n';
    let mut i: isize = if bare { -2 } else { 0 };
    while i <= limit as isize - 4 {
        if (bare && i == -2) || (0 <= i && &buf[i as usize..i as usize + 4] == b"\r\n\r\n") {
            let end = if bare { 0 } else { i as usize + 2 };
            if let Err(why) = answer_lines(ws(p), &buf[..end]) {
                return refused(p, why);
            }
            if ws(p).heard & HEARD_LINES == 0 {
                return refused(p, "the server did not switch protocols");
            }
            if ws(p).heard & HEARD_ACCEPT == 0 {
                return refused(p, "no Sec-WebSocket-Accept in the server's answer");
            }
            ws(p).state = state::NEW_FRAME;
            (*p).ext.upgrading.store(false, Ordering::Release); // the session opens with the upgrade
            fire(p, Event::WEBSOCKET_THIS_CONNECT);
            if !active(p) {
                return false;
            }
            if (*p).report.is_some() {
                engine::session_started(p); // the wait for the 101 is over
                engine::report(p, Ok(())); // the session has started: whoever waits for the connect has its answer
            }
            if !active(p) {
                return false;
            }
            engine::wake_on_loop(p); // whatever was queued during the handshake
            if !active(p) {
                return false;
            }
            (*p).r_at = (i + 4) as usize;
            ws(p).decoded = (*p).r_at;
            return true;
        }
        i += 1;
    }
    // only complete lines are read
    let Some(nl) = buf[..limit].iter().rposition(|&b| b == b'\n') else {
        if limit == buf.len() {
            return refused(p, "a line of the server's answer does not fit the buffer");
        }
        return false;
    };
    if let Err(why) = answer_lines(ws(p), &buf[..nl + 1]) {
        return refused(p, why);
    }
    buf.copy_within(nl + 1..limit, 0); // the partial line to the front; the next read appends to it
    (*p).r_at = 0;
    (*p).r_end = limit - (nl + 1);
    false
}

/// Complete lines of the server's answer: the first of them all must be the status line of a `101`;
/// `Sec-WebSocket-Accept`, wherever it comes, must answer this client's key.
fn answer_lines(ws: &mut Codec, lines: &[u8]) -> Result<(), &'static str> {
    if lines.is_empty() {
        return Ok(());
    }
    if ws.heard & HEARD_LINES == 0 {
        // "HTTP/1.1 101..."
        if lines.len() < 12 || lines[0] != b'H' || lines[4] != b'/' || lines[8] != b' ' || &lines[9..12] != b"101" {
            return Err("the server did not switch protocols");
        }
        ws.heard |= HEARD_LINES;
    }
    let at = crate::lib::adhoc::utils::boyer_moore_case_insensitive(lines, &SEC_WEBSOCKET_ACCEPT);
    if 0 <= at {
        let mut k = at as usize + SEC_WEBSOCKET_ACCEPT.len(); // the byte after the name
        while k < lines.len() && (lines[k] == b' ' || lines[k] == b'\t') {
            k += 1;
        }
        if lines.len() < k + 28 || lines[k..k + 28] != ws.accept {
            return Err("Sec-WebSocket-Accept is not the answer to this client's key");
        }
        ws.heard |= HEARD_ACCEPT;
    }
    Ok(())
}

unsafe fn refused(p: *mut Slot, why: &str) -> bool {
    fire(p, Event::WEBSOCKET_HANDSHAKE_FAILURE); // why goes to the handler with the failed connect (`dial`)
    engine::report(p, Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, why.to_string())));
    engine::disconnect(p, Event::THIS_CLOSE_ABRUPTLY);
    false
}

/// A protocol violation: its event, then `why` to the handler of the host, the connection torn
/// down.
unsafe fn fail(p: *mut Slot, why: &str) -> bool {
    fire(p, Event::WEBSOCKET_PROTOCOL_ERROR);
    (*p).ext.failure(&crate::lib::network::failure::protocol(why));
    engine::disconnect(p, Event::THIS_CLOSE_ABRUPTLY);
    false
}

/// Advances the frame automaton from the read position. Header bytes are consumed as they come; a
/// data payload is unmasked in place and left before `decoded` for [`receive`] to offer; control
/// payloads are collected, a PONG's is skipped. False once the connection is being torn down.
unsafe fn decode(p: *mut Slot) -> bool {
    let end = (*p).r_end;
    loop {
        let w = ws(p);
        match w.state {
            state::NEW_FRAME => {
                if (*p).r_at == end {
                    return true;
                }
                let b = engine::rbyte(p, (*p).r_at);
                (*p).r_at += 1;
                w.decoded = (*p).r_at;
                w.byte = b;
                if b & 0x70 != 0 {
                    return fail(p, "Protocol Error: RSV bits set with no negotiated extension.");
                }
                w.opcode = b & 0x0F;
                if (0x2 < w.opcode && w.opcode < 0x8) || 0xA < w.opcode {
                    return fail(p, &format!("Protocol Error: reserved opcode 0x{:x}", w.opcode));
                }
                if 0x8 <= w.opcode && b & FIN == 0 {
                    return fail(p, "Protocol Error: fragmented control frame (FIN=0)");
                }
                w.state = state::PAYLOAD_LENGTH_BYTE;
            }
            state::PAYLOAD_LENGTH_BYTE => {
                if (*p).r_at == end {
                    return true;
                }
                let b = engine::rbyte(p, (*p).r_at);
                (*p).r_at += 1;
                w.decoded = (*p).r_at;
                if (b & MASK == 0) != w.client {
                    return fail(
                        p,
                        if w.client {
                            "Protocol Error: Server frame is masked."
                        } else {
                            "Protocol Error: Client frame is not masked."
                        },
                    );
                }
                if 0x8 <= w.opcode && 125 < (b & LEN) {
                    return fail(p, "Protocol Error: control frame payload > 125 bytes");
                }
                w.length_bytes = 0;
                w.frame_left = (b & LEN) as u64;
                if 125 < w.frame_left {
                    w.length_bytes = if w.frame_left == 126 { 2 } else { 8 };
                    w.frame_left = 0;
                }
                w.state = state::PAYLOAD_LENGTH_BYTES;
            }
            state::PAYLOAD_LENGTH_BYTES => {
                while 0 < w.length_bytes {
                    if (*p).r_at == end {
                        return true;
                    }
                    // RFC 6455 5.2: the 64-bit form can carry a length past what a frame here may
                    // be: 0x7F_FF_FF is the largest that survives one more byte
                    if 0x7F_FF_FF < w.frame_left {
                        return fail(p, "Protocol Error: payload length exceeds Integer.MAX_VALUE");
                    }
                    w.frame_left = (w.frame_left << 8) | engine::rbyte(p, (*p).r_at) as u64;
                    (*p).r_at += 1;
                    w.decoded = (*p).r_at;
                    w.length_bytes -= 1;
                }
                w.mask = 0;
                if w.client {
                    if !header_done(p) {
                        return false;
                    }
                    continue;
                }
                w.state = state::XOR0;
            }
            state::XOR0..=state::XOR3 => {
                loop {
                    if (*p).r_at == end {
                        return true;
                    }
                    w.mask |= (engine::rbyte(p, (*p).r_at) as u32) << ((w.state - state::XOR0) << 3);
                    (*p).r_at += 1;
                    w.decoded = (*p).r_at;
                    if w.state == state::XOR3 {
                        break;
                    }
                    w.state += 1;
                }
                w.mask_pos = 0;
                if !header_done(p) {
                    return false;
                }
            }
            state::DATA => {
                if (*p).r_at == end {
                    return true;
                }
                let pos = (*p).r_at;
                let span = (w.frame_left.min((end - pos) as u64)) as usize;
                if !w.client {
                    unmask(w, engine::rbytes(p, pos, pos + span));
                }
                w.decoded = pos + span;
                w.frame_left -= span as u64;
                if w.frame_left == 0 {
                    w.state = state::NEW_FRAME;
                }
                return true;
            }
            state::CONTROL => {
                if (*p).r_at == end {
                    return true;
                }
                let pos = (*p).r_at;
                let span = (w.frame_left.min((end - pos) as u64)) as usize;
                let buf = engine::rbytes(p, pos, pos + span);
                if !w.client {
                    unmask(w, buf);
                }
                let r = w.reply.as_mut().unwrap(); // made by header_done
                let at = r.len as usize;
                let n = span.min(125 - at);
                r.data[at..at + n].copy_from_slice(&buf[..n]); // the payload of a PING or CLOSE: echoed
                r.len += n as u8;
                (*p).r_at += span;
                w.decoded = (*p).r_at;
                w.frame_left -= span as u64;
                if 0 < w.frame_left {
                    return true;
                }
                w.state = state::NEW_FRAME;
                control_done(p);
                if !active(p) {
                    return false;
                }
            }
            state::DISCARD => {
                if (*p).r_at == end {
                    return true;
                }
                let span = (w.frame_left.min((end - (*p).r_at) as u64)) as usize;
                (*p).r_at += span;
                w.decoded = (*p).r_at;
                w.frame_left -= span as u64;
                if 0 < w.frame_left {
                    return true;
                }
                w.state = state::NEW_FRAME;
            }
            _ => return false, // HANDSHAKE: nothing is decoded before the upgrade
        }
    }
}

/// The frame header is complete: the payload state, and a frame with no payload handled at once.
/// False - the connection is closing.
unsafe fn header_done(p: *mut Slot) -> bool {
    let w = ws(p);
    match w.opcode {
        op::PING => {
            let r = w.reply.get_or_insert_with(|| Box::new(Control { op: 0, len: 0, data: [0; 125] }));
            r.op = op::PONG; // the PONG echoes the PING's payload
            r.len = 0;
            w.reply_ready = false;
        }
        op::CLOSE => {
            if w.sent_closing_frame {
                // the peer echoed our CLOSE: the closing handshake is done, the TCP one follows -
                // the peer's FIN closes
                fire(p, Event::WEBSOCKET_THIS_CLOSE_GRACEFUL);
                if !active(p) {
                    return false;
                }
                (*p).ext.ws_closing.store(false, Ordering::Relaxed); // Reset graceful-close state before starting another session.
                                                                     // the FIN goes from the runner, once our CLOSE is written: sent_closing_frame is set
                                                                     // when the CLOSE is put in the transmit buffer, and a FIN before the write would cut
                                                                     // it off
                ws(p).peer_closing = true;
                engine::wake_on_loop(p);
                return false;
            }
            w.peer_closing = true;
            let r = w.reply.get_or_insert_with(|| Box::new(Control { op: 0, len: 0, data: [0; 125] }));
            r.op = op::CLOSE; // the answer echoes the peer's status
            r.len = 0;
            w.reply_ready = false;
        }
        op::PONG => {
            w.state = if w.frame_left == 0 { state::NEW_FRAME } else { state::DISCARD };
            fire(p, Event::WEBSOCKET_PONG);
            return active(p);
        }
        _ => {
            // data frames: BINARY, TEXT, CONTINUATION
            if w.frame_left == 0 {
                w.state = state::NEW_FRAME;
                fire(p, Event::WEBSOCKET_EMPTY_FRAME);
                return active(p);
            }
            w.state = state::DATA;
            return true;
        }
    }
    if w.frame_left == 0 {
        w.state = state::NEW_FRAME;
        control_done(p);
        return active(p);
    }
    w.state = state::CONTROL;
    true
}

/// A PING or a CLOSE is complete: announced, and its answer goes out.
unsafe fn control_done(p: *mut Slot) {
    let ping = ws(p).opcode == op::PING;
    fire(p, if ping { Event::WEBSOCKET_PING } else { Event::WEBSOCKET_REMOTE_CLOSE_GRACEFUL });
    if !active(p) {
        return;
    }
    ws(p).reply_ready = true;
    engine::wake_on_loop(p);
}

/// Unmasks in place, continuing the key from `mask_pos`.
#[inline]
fn unmask(w: &mut Codec, buf: &mut [u8]) {
    w.mask_pos = xor_key(buf, w.mask, w.mask_pos);
}

/// XORs `buf` with the key `m`, its byte `pos & 3` first: the position after it. Eight bytes at a
/// time once the key is aligned.
#[inline]
fn xor_key(buf: &mut [u8], m: u32, mut pos: u32) -> u32 {
    let mut i = 0;
    let n = buf.len();
    while i < n && pos & 3 != 0 {
        buf[i] ^= (m >> ((pos & 3) << 3)) as u8;
        i += 1;
        pos += 1;
    }
    xor8(&mut buf[i..], m); // whole words: the position stays aligned
    i += (n - i) / 8 * 8;
    while i < n {
        buf[i] ^= (m >> ((pos & 3) << 3)) as u8;
        i += 1;
        pos += 1;
    }
    pos & 3
}

/// XORs the whole 8-byte words of `buf` with the key repeated - the first key byte lowest, as the
/// bytes lie.
#[inline]
fn xor8(buf: &mut [u8], m: u32) {
    let k = (m as u64) | ((m as u64) << 32);
    let k = u64::from_le(k);
    let words = buf.len() / 8;
    let ptr = buf.as_mut_ptr();
    for j in 0..words {
        // SAFETY: inside `buf`, unaligned access
        unsafe {
            let q = ptr.add(j * 8) as *mut u64;
            q.write_unaligned(q.read_unaligned() ^ k);
        }
    }
}

// transmitting

/// Fills the transmit buffer: the handshake first and on its own, then - after the upgrade - a
/// control frame and a data frame with what the source serializes behind the room left for their
/// headers.
pub(crate) unsafe fn fill(p: *mut Slot) -> bool {
    (*p).held = false;
    let w = ws(p);
    let (lo, hi) = ((*p).t_lo, (*p).t_hi);
    let buf = &mut (*p).tbuf.as_mut().unwrap()[lo..hi];
    if w.client && !w.hs.is_empty() {
        // the upgrade request, whole, in the one buffer: a target too long for it fails the connect
        let n = w.hs.len();
        if buf.len() < n {
            let e = std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("the upgrade request of {n} bytes does not fit the buffer of {} bytes", buf.len()),
            );
            if (*p).report.is_some() {
                engine::report(p, Err(e)); // the connect hears why (the handler too, then the teardown), as `refused` does
            } else {
                (*p).ext.failure(&e);
            }
            engine::disconnect(p, Event::THIS_CLOSE_ABRUPTLY);
            return false;
        }
        buf[..n].copy_from_slice(&w.hs);
        w.hs = Vec::new();
        (*p).t_at = lo;
        (*p).t_end = lo + n;
        return true;
    }
    if w.answer && w.state != state::HANDSHAKE {
        // the 101, written where it goes: the buffer holds it whole (MIN_BUFFER)
        let at = UPGRADE.len();
        buf[..at].copy_from_slice(UPGRADE);
        buf[at..at + 28].copy_from_slice(&w.accept);
        buf[at + 28..at + 32].copy_from_slice(b"\r\n\r\n");
        w.answer = false;
        (*p).t_at = lo;
        (*p).t_end = lo + at + 32;
        return true;
    }
    if w.state == state::HANDSHAKE {
        return false; // no frames before the upgrade
    }
    let mut ctl: Option<Control> = None;
    if (*p).ext.ws_urgent_set.load(Ordering::Acquire) {
        (*p).ext.ws_urgent_set.store(false, Ordering::Release);
        ctl = (*p).ext.ws_urgent.lock().take();
    }
    let mut from_reply = false;
    if ctl.is_none() && w.reply_ready {
        from_reply = true;
    }
    let key4 = if w.client { 4 } else { 0 };
    let masked = if w.client { MASK } else { 0 };
    let ctl_len = match (&ctl, from_reply) {
        (Some(c), _) => c.len as usize + 2 + key4,
        (None, true) => w.reply.as_ref().map_or(0, |r| r.len as usize) + 2 + key4,
        _ => 0,
    };
    // nothing after a CLOSE (RFC 6455 §5.5.1): a CLOSE goes alone, and once it is out no data frame
    // follows
    let close_now = match (&ctl, from_reply) {
        (Some(c), _) => c.is_close(),
        (None, true) => w.reply.as_ref().is_some_and(|r| r.is_close()),
        _ => false,
    };
    if w.sent_closing_frame || close_now {
        (*p).held = false; // drained as far as the runner goes: a closing handshake goes on
        if ctl_len == 0 || close_now && w.sent_closing_frame {
            // CLOSE once: the answer to a peer's CLOSE that crossed ours does not go
            if from_reply {
                w.reply_ready = false;
            }
            return false;
        }
        put_control(w, buf, 0, ctl.as_ref());
        (*p).t_at = lo;
        (*p).t_end = lo + ctl_len;
        return true;
    }
    let start = ctl_len + 10 + key4;
    // a small buffer with a long control frame: the control frame goes alone, the data in the next
    // buffer
    let n = if buf.len() <= start {
        -1
    } else {
        match (*p).source.as_mut() {
            Some(s) => s.read(&mut buf[start..]), // after the space left for the headers: nothing is moved
            None => -1,
        }
    };
    if !active(p) {
        return false;
    }
    (*p).held = n == 0;
    let w = ws(p);
    if 0 < n {
        let len = n as usize;
        let mut begin = start;
        if w.client {
            let key = w.next_key();
            if (*p).link.is_null() {
                mask(&mut buf[start - 4..start + len], key);
            } else {
                // goes with the rest of this buffer's chain, behind its seal of Noise
                buf[start - 4..start].copy_from_slice(&key.to_le_bytes());
                if key != 0 {
                    (*(*p).link).op_mask(buf.as_mut_ptr().add(start), len, key);
                }
            }
            begin -= 4;
        }
        if len < 126 {
            begin -= 2;
            buf[begin] = FIN | op::BINARY;
            buf[begin + 1] = masked | len as u8;
        } else if len < 0x1_0000 {
            begin -= 4;
            buf[begin] = FIN | op::BINARY;
            buf[begin + 1] = masked | 126;
            buf[begin + 2] = (len >> 8) as u8;
            buf[begin + 3] = len as u8;
        } else {
            begin -= 10;
            buf[begin] = FIN | op::BINARY;
            buf[begin + 1] = masked | 127;
            buf[begin + 2..begin + 10].copy_from_slice(&(len as u64).to_be_bytes());
        }
        if 0 < ctl_len {
            begin -= ctl_len;
            put_control(w, buf, begin, ctl.as_ref());
        }
        (*p).t_at = lo + begin;
        (*p).t_end = lo + start + len;
        return true;
    }
    if ctl_len == 0 {
        return false;
    }
    put_control(w, buf, 0, ctl.as_ref());
    (*p).t_at = lo;
    (*p).t_end = lo + ctl_len;
    true
}

/// A control frame at `at`: as it is from a server, with a key and masked from a client. `None` -
/// the reply.
fn put_control(w: &mut Codec, buf: &mut [u8], at: usize, urgent: Option<&Control>) {
    let c = match urgent {
        Some(c) => c.clone(),
        None => {
            w.reply_ready = false;
            (**w.reply.as_ref().unwrap()).clone()
        }
    };
    if c.op == op::CLOSE {
        w.sent_closing_frame = true;
    }
    let len = c.len as usize;
    buf[at] = FIN | c.op;
    if w.client {
        buf[at + 1] = MASK | c.len;
        buf[at + 6..at + 6 + len].copy_from_slice(&c.data[..len]);
        let key = w.next_key();
        mask(&mut buf[at + 2..at + 6 + len], key);
    } else {
        buf[at + 1] = c.len;
        buf[at + 2..at + 2 + len].copy_from_slice(&c.data[..len]);
    }
}

/// Writes the key into the first four bytes of `frame` - first key byte lowest - and masks the rest
/// in place with it.
#[inline]
fn mask(frame: &mut [u8], m: u32) {
    frame[..4].copy_from_slice(&m.to_le_bytes());
    if m == 0 {
        return;
    }
    let body = &mut frame[4..];
    xor8(body, m);
    let n = body.len();
    let mut i = n / 8 * 8;
    while i < n {
        body[i] ^= (m >> ((i & 3) << 3)) as u8;
        i += 1;
    }
}

/// The source is empty: the closing handshake goes on.
pub(crate) unsafe fn on_drained(p: *mut Slot) {
    let w = ws(p);
    if w.peer_closing && w.sent_closing_frame {
        // the peer's CLOSE is answered and the answer is out: the TCP connection closes (RFC 6455
        // §7.1.1)
        w.peer_closing = false;
        return engine::shutdown_output(p);
    }
    // a CLOSE out already (ours, or the answer to the peer's) is the closing handshake under way:
    // no second one
    if engine::closing(p) && w.state != state::HANDSHAKE && !w.sent_closing_frame && !(*p).ext.ws_closing.swap(true, Ordering::AcqRel) {
        // the CLOSE asked for (close_graceful), or 1000 for a negative timeout
        let c = (*p).ext.ws_close.lock().take().unwrap_or_else(|| Control::close(1000, "Normal Closure"));
        *(*p).ext.ws_urgent.lock() = Some(c);
        (*p).ext.ws_urgent_set.store(true, Ordering::Release);
        engine::wake_on_loop(p);
    }
}

// the handshake's arithmetic

/// `base64(sha1(key + GUID))`: 28 characters.
fn accept_of(key: &[u8], out: &mut [u8; 28]) {
    let mut h = Sha1::new();
    h.update(key);
    h.update(GUID);
    base64(&h.finish(), out);
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(src: &[u8], dst: &mut [u8]) {
    let mut o = 0;
    let mut chunks = src.chunks_exact(3);
    for c in &mut chunks {
        let b = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        dst[o] = B64[(b >> 18) as usize & 63];
        dst[o + 1] = B64[(b >> 12) as usize & 63];
        dst[o + 2] = B64[(b >> 6) as usize & 63];
        dst[o + 3] = B64[b as usize & 63];
        o += 4;
    }
    let r = chunks.remainder();
    match r.len() {
        1 => {
            let b = (r[0] as u32) << 16;
            dst[o] = B64[(b >> 18) as usize & 63];
            dst[o + 1] = B64[(b >> 12) as usize & 63];
            dst[o + 2] = b'=';
            dst[o + 3] = b'=';
        }
        2 => {
            let b = (r[0] as u32) << 16 | (r[1] as u32) << 8;
            dst[o] = B64[(b >> 18) as usize & 63];
            dst[o + 1] = B64[(b >> 12) as usize & 63];
            dst[o + 2] = B64[(b >> 6) as usize & 63];
            dst[o + 3] = b'=';
        }
        _ => {}
    }
}

/// SHA-1, for the handshake only.
struct Sha1 {
    h: [u32; 5],
    block: [u8; 64],
    fill: usize,
    total: u64,
}

impl Sha1 {
    fn new() -> Self {
        Sha1 {
            h: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            block: [0; 64],
            fill: 0,
            total: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        while !data.is_empty() {
            let n = (64 - self.fill).min(data.len());
            self.block[self.fill..self.fill + n].copy_from_slice(&data[..n]);
            self.fill += n;
            data = &data[n..];
            if self.fill == 64 {
                let b = self.block;
                self.compress(&b);
                self.fill = 0;
            }
        }
    }

    fn finish(mut self) -> [u8; 20] {
        let bits = self.total * 8;
        self.update(&[0x80]);
        while self.fill != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 20];
        for (i, v) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut bb, mut c, mut d, mut e] = self.h;
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((bb & c) | (!bb & d), 0x5A827999),
                20..=39 => (bb ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((bb & c) | (bb & d) | (c & d), 0x8F1BBCDC),
                _ => (bb ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
            e = d;
            d = c;
            c = bb.rotate_left(30);
            bb = a;
            a = t;
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(bb);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
    }
}

/// A WebSocket connection: the connection - its events, its timeouts - and what only a WebSocket
/// says: a PING, a CLOSE with a status. Its frames go ahead of the data.
#[derive(Clone)]
pub struct WebSocket {
    pub state: Arc<ExternalConnection>,
}

impl std::ops::Deref for WebSocket {
    type Target = Arc<ExternalConnection>;
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl WebSocket {
    /// A PING frame; the payload at most 125 bytes (RFC 6455 §5.5).
    pub fn ping(&self, payload: Option<&[u8]>) -> Result<(), &'static str> {
        let p = payload.unwrap_or(&[]);
        if 125 < p.len() {
            return Err("PING payload exceeds 125 bytes (RFC 6455 §5.5)");
        }
        if self.state.ws_control(Control::ping(p)) {
            Ok(())
        } else {
            Err("WebSocket connection torn down")
        }
    }

    /// Closes gracefully with a CLOSE frame of a status code and a reason - its first 123 bytes,
    /// cut at a character boundary (RFC 6455 §5.5.1): what is queued goes out first, then the CLOSE
    /// - after a CLOSE no data frame may follow, a CLOSE ahead of the data would lose it; the
    /// peer's answer closes the connection. Once: a CLOSE asked for already makes it a no-op.
    pub fn close_graceful(&self, code: u16, reason: &str) -> Result<(), &'static str> {
        if !self.state.is_open() {
            return Err("WebSocket connection torn down");
        }
        {
            let mut staged = self.state.ws_close.lock();
            if staged.is_some() || self.state.ws_closing.load(Ordering::Acquire) {
                return Ok(());
            }
            *staged = Some(Control::close(code, reason));
        }
        self.state.close_gracefully(); // the queue drains, then on_drained sends it
        Ok(())
    }

    /// Graceful close with 1000 ("Normal Closure").
    #[inline]
    pub fn close(&self) -> Result<(), &'static str> {
        self.close_graceful(1000, "AdHoc server closing")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reason past 123 bytes is cut where a character begins: a two-byte letter is not split.
    #[test]
    fn the_reason_of_a_close_is_cut_at_a_character() {
        let why = "я".repeat(70); // 140 bytes
        let c = Control::close(1000, &why);
        assert_eq!(c.len as usize, 2 + 122);
        assert!(std::str::from_utf8(&c.data[2..c.len as usize]).is_ok());
    }

    #[test]
    fn the_accept_of_the_rfc_example() {
        let mut out = [0u8; 28];
        accept_of(b"dGhlIHNhbXBsZSBub25jZQ==", &mut out);
        assert_eq!(&out, b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn unmask_continues_the_key_across_spans() {
        let key = 0x0403_0201u32;
        let plain: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let mut masked = plain.clone();
        for (i, b) in masked.iter_mut().enumerate() {
            *b ^= (key >> ((i & 3) * 8)) as u8;
        }
        for cut in [0usize, 1, 3, 5, 9, 17, 500, 999] {
            let mut w = Codec::new(false);
            w.mask = key;
            w.mask_pos = 0;
            let mut m = masked.clone();
            let (a, b) = m.split_at_mut(cut);
            unmask(&mut w, a);
            unmask(&mut w, b);
            assert_eq!(m, plain, "cut {cut}");
        }
    }
}
