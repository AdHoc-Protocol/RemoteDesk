// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! The built-in stages of a chain - `[Zstd]`, `[ChaCha20]`, `[Envelope]` - as one: the native library runs all of them in one call a
//! buffer each way, this code only says which and hands it the buffers. What is chosen and what came of a call lies in the memory of the
//! library, read and written here where it lies: the offsets below are those of `Stages` in `adhoc_crypto.c`.
//!
//! A lone `ChaCha20` on the sending side copies nothing: the plaintext is ciphered where it lies, in the buffer of the socket. With
//! `Zstd` or an `Envelope` it goes through the staging of the stages: the serializer writes into it as into any buffer, the call takes
//! it from there into the buffer of the socket.
//!
//! - `[ChaCha20]` on a pack: 12 bytes in clear ahead of it - the id of the key of the session it is ciphered with and the sequence of
//!   its nonce -, each pack deciphered alone with the key it names.
//! - `[Envelope]`: the pack sealed for one reader and signed by its author, as [`crate::lib::chain::Pack`] makes it: `[id 4][number 8]`,
//!   the text, the signature, the tag. Its keys are the [`crate::lib::chain::Envelope`] of the connection, set before the pack goes or
//!   comes. Gathered whole: it is opened only so, and nothing of it reaches the pack before it is.
//!
//! The keys of both are those of the connection the stream goes on ( its `keys`, its `envelope` ), unless the stages are given
//! [`Keys`] of their own: the chain of a tunnel carries the stream of a virtual connection inside the packs of its carrier, and is
//! keyed by the tunnel, never by the carrier, whose keys every relay on the way knows.
//!
//! The library is loaded at the first chain that has one of these stages: a host with none of them never needs it.

use crate::lib::adhoc::connection::keyring::Keyring;
use crate::lib::adhoc::connection::receiver::Rx as In;
use crate::lib::adhoc::connection::transmitter::Tx as Out;
use crate::lib::adhoc::{BytesDst, BytesSrc};
use crate::lib::chain::Envelope;
use crate::lib::crypto::{api, Mem};
use std::sync::Arc;

/// The keys of the stream a run of stages carries, when they are not those of the connection it goes on: the chain of a tunnel. A
/// `ChaCha20` enciphers with the newest key of `keys` and deciphers with the key a stream names there; an `Envelope` seals and opens
/// with `envelope`. Nothing of the connection is looked at then: no key here - the stream is refused, it is not ciphered with another.
#[derive(Clone)]
pub struct Keys {
    pub keys: Arc<Keyring>,
    pub envelope: Option<Arc<Envelope>>,
}

impl Keys {
    pub fn new(keys: Arc<Keyring>, envelope: Option<Arc<Envelope>>) -> Self {
        Self { keys, envelope }
    }

    /// No key and no Envelope: one ring for all, nothing is made.
    pub fn none() -> Self {
        static NONE: std::sync::OnceLock<Arc<Keyring>> = std::sync::OnceLock::new();
        Self {
            keys: NONE.get_or_init(|| Arc::new(Keyring::new())).clone(),
            envelope: None,
        }
    }
}

impl Default for Keys {
    fn default() -> Self {
        Self::none()
    }
}

/// A stream with ChaCha20 and none of the keys the stages are given: the words of the refusal.
pub const NO_OWN_KEY: &str = "A tunnel's chain with ChaCha20 and no key of the tunnel: set a key both ends know in the keys of the tunnel on both ends ( `Tunnel::keys` ), or return a keyring both ends know in the region \"The keys of the chain of the tunnel\". The keys of the carrier are not taken: every relay on the way knows them.";

/// The source of the bursts of a tunnel encrypted by Noise ( `[Noise]` ): the stream of its virtual connection through the stages of
/// its chain, made at the first read - after the handshake, which goes with no stage.
pub struct Staged {
    make: Option<Box<dyn FnOnce() -> Box<dyn BytesSrc> + Send>>,
    made: Option<Box<dyn BytesSrc>>,
}

impl Staged {
    pub fn new(make: impl FnOnce() -> Box<dyn BytesSrc> + Send + 'static) -> Self {
        Self {
            make: Some(Box::new(make)),
            made: None,
        }
    }
}

impl BytesSrc for Staged {
    fn read(&mut self, dst: &mut [u8]) -> isize {
        if self.made.is_none() {
            self.made = self.make.take().map(|m| m());
        }
        self.made.as_mut().map_or(-1, |s| s.read(dst))
    }

    fn at_boundary(&self) -> bool {
        self.made.as_ref().is_none_or(|s| s.at_boundary())
    }
}

/// What a tunnel encrypted by Noise ( `[Noise]` ) gets: opened, through the stages of its chain, to the receiver of its virtual
/// connection - lent for each call ( [`with`](Self::with) ). The stages are made at the first bytes opened - after the handshake.
pub struct Unstaged {
    slot: std::sync::Arc<Slot>,
    make: Option<Box<dyn FnOnce(Box<dyn BytesDst>) -> Box<dyn BytesDst> + Send>>,
    made: Option<Box<dyn BytesDst>>,
}

/// The sink lent for a call.
#[derive(Default)]
struct Slot(std::cell::Cell<Option<std::ptr::NonNull<dyn BytesDst>>>);

// SAFETY: it points only while a call lends it the sink, on the thread of that call
unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

/// The innermost of the stages: the sink lent.
struct Lent(std::sync::Arc<Slot>);

impl Lent {
    #[inline]
    fn get(&self) -> &mut dyn BytesDst {
        // SAFETY: set by `Unstaged::with` for the length of its call, the one call that reaches here
        unsafe { &mut *self.0 .0.get().expect("the sink is lent only for a call").as_ptr() }
    }
}

impl BytesDst for Lent {
    fn on_bytes(&mut self, bytes: &[u8]) -> bool {
        self.get().on_bytes(bytes)
    }
    fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
        self.get().on_slice(bytes)
    }
    fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
        self.get().on_part(bytes)
    }
    fn on_bytes_mut(&mut self, bytes: &mut [u8]) -> bool {
        self.get().on_bytes_mut(bytes)
    }
    fn graceful_close(&self) -> bool {
        self.get().graceful_close()
    }
    fn broken(&self) -> bool {
        self.get().broken()
    }
    fn wants_more(&self) -> bool {
        self.get().wants_more()
    }
    fn retry(&mut self) {
        self.get().retry()
    }
}

impl Unstaged {
    /// `make` puts the stages over the sink it is given: the innermost first.
    pub fn new(make: impl FnOnce(Box<dyn BytesDst>) -> Box<dyn BytesDst> + Send + 'static) -> Self {
        Self {
            slot: Default::default(),
            make: Some(Box::new(make)),
            made: None,
        }
    }

    /// `f` with the stages over `sink`: what they give goes there.
    pub fn with<R>(&mut self, sink: &mut (dyn BytesDst + 'static), f: impl FnOnce(&mut Self) -> R) -> R {
        self.slot.0.set(Some(std::ptr::NonNull::from(sink)));
        let r = f(self);
        self.slot.0.set(None);
        r
    }

    #[inline]
    fn stages(&mut self) -> &mut dyn BytesDst {
        if self.made.is_none() {
            let lent: Box<dyn BytesDst> = Box::new(Lent(self.slot.clone()));
            self.made = self.make.take().map(|m| m(lent));
        }
        &mut **self.made.as_mut().expect("made once")
    }
}

impl BytesDst for Unstaged {
    fn on_bytes(&mut self, bytes: &[u8]) -> bool {
        self.stages().on_bytes(bytes)
    }
    fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
        self.stages().on_slice(bytes)
    }
    fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
        self.stages().on_part(bytes)
    }
    fn on_bytes_mut(&mut self, bytes: &mut [u8]) -> bool {
        self.stages().on_bytes_mut(bytes)
    }
    fn graceful_close(&self) -> bool {
        self.made.as_ref().is_some_and(|m| m.graceful_close())
    }
    fn broken(&self) -> bool {
        self.made.as_ref().is_some_and(|m| m.broken())
    }
    fn wants_more(&self) -> bool {
        self.made.as_ref().is_none_or(|m| m.wants_more())
    }
    fn retry(&mut self) {
        if let Some(m) = self.made.as_mut() {
            m.retry()
        }
    }
}
const NO_OWN_ENVELOPE: &str = "A tunnel's chain with Envelope and no keys of it: set the envelope of the virtual connection of the tunnel ( chain::Envelope ), or return one in the region \"The keys of the chain of the tunnel\".";

/// The kinds of the stages, as the native library knows them.
pub const ZSTD: i32 = 1;
pub const CHACHA20: i32 = 2;
/// ChaCha20 under the keys of the session: a pack names its own key.
pub const CHACHA20_PACK: i32 = 3;
pub const ENVELOPE: i32 = 4;

const COUNT: usize = 0;
const TX: usize = 4;
const KIND: usize = 8;
const LEVEL: usize = 24;
const ENV_CTX: usize = 40;
const ENV_KEY: usize = 48;
const ENV_N: usize = 56;
const ENV_ID: usize = 64;
const KEYS: usize = 68;
const RING: usize = 72;
const SEQ: usize = 264;
const BUF: usize = 272;
const CAP: usize = 280;
const LEN: usize = 288;
const AT: usize = 292;
const DONE: usize = 296;
const TAKEN: usize = 300;
const OPEN: usize = 304;
const PHASE: usize = 308;
const EXT: usize = 312;
const NEED: usize = 316;
const HEADER: usize = 12;
const MAX_KEYS: usize = 4;
const MIN_ROOM: usize = 256;
/// What a call gives when the staging is too small: the room it wants is in `NEED`.
const NO_ROOM: i32 = -3;
/// What a transport stage takes from the upper layer at a time.
const PLAIN: usize = 1 << 14;

/// One run of built-in stages, leaf first - a compressor, then a cipher -: their kinds and levels. Zstd's level is its level, an
/// Envelope's is its flags: 1 - sealed, 2 - signed; 0 for ChaCha20. No second stage - `kind2` 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub kind1: i32,
    pub level1: i32,
    pub kind2: i32,
    pub level2: i32,
}

/// The memory of the library: what is chosen, what a call did, its codecs behind them; and the staging.
struct Native {
    mem: Mem,
    /// The staging: the library reads and writes it where it lies, its address and room in `BUF` and `CAP`.
    staging: Vec<u8>,
    /// A lone ChaCha20: no staging, the bytes ciphered where they lie.
    in_place: bool,
    /// Bits of the kinds chosen.
    kinds: u32,
    /// Of an Envelope: 1 - sealed, 2 - signed.
    envelope: i32,
}

impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: the memory is this one's; the library frees its codecs, not the staging ( `EXT` )
        unsafe { (api().stages_free)(self.mem.at()) };
    }
}

impl Native {
    fn new(tx: bool, run: Run) -> Self {
        // SAFETY: a call with no arguments
        let size = unsafe { (api().stages_size)() } as usize;
        let mut s = Self {
            mem: Mem::new(size),
            staging: Vec::new(),
            in_place: false,
            kinds: 0,
            envelope: 0,
        };
        s.set_i(TX, tx as i32);
        s.set_i(EXT, 1);
        let n = if run.kind2 == 0 { 1 } else { 2 };
        s.set_i(COUNT, n);
        s.set_i(KIND, run.kind1);
        s.set_i(LEVEL, run.level1);
        s.set_i(KIND + 4, run.kind2);
        s.set_i(LEVEL + 4, run.level2);
        s.kinds = 1 << run.kind1 | if run.kind2 == 0 { 0 } else { 1 << run.kind2 };
        s.envelope = if run.kind1 == ENVELOPE {
            run.level1
        } else if run.kind2 == ENVELOPE {
            run.level2
        } else {
            0
        };
        s.in_place = n == 1 && (run.kind1 == CHACHA20 || run.kind1 == CHACHA20_PACK);
        if s.has(ZSTD) {
            s.room(128 * 1024 + 64); // what Zstd decompresses into at once
        }
        s.rearm();
        s
    }

    #[inline(always)]
    fn at(&self) -> i64 {
        self.mem.at()
    }

    #[inline(always)]
    fn i(&self, at: usize) -> i32 {
        // SAFETY: an offset of the struct, aligned
        unsafe { (self.mem.ptr().add(at) as *const i32).read() }
    }

    #[inline(always)]
    fn set_i(&mut self, at: usize, v: i32) {
        // SAFETY: an offset of the struct, aligned
        unsafe { (self.mem.ptr().add(at) as *mut i32).write(v) }
    }

    #[inline(always)]
    fn l(&self, at: usize) -> i64 {
        // SAFETY: an offset of the struct, aligned
        unsafe { (self.mem.ptr().add(at) as *const i64).read() }
    }

    #[inline(always)]
    fn set_l(&mut self, at: usize, v: i64) {
        // SAFETY: an offset of the struct, aligned
        unsafe { (self.mem.ptr().add(at) as *mut i64).write(v) }
    }

    #[inline(always)]
    fn has(&self, kind: i32) -> bool {
        self.kinds & 1 << kind != 0
    }

    /// A new stream: nothing of the last one is left.
    fn rearm(&mut self) {
        let b = if self.i(TX) == 1 && self.has(ENVELOPE) && !self.has(ZSTD) { HEADER as i32 } else { 0 };
        self.set_i(LEN, b);
        self.set_i(AT, b);
        self.set_i(DONE, 0);
        self.set_i(TAKEN, 0);
        self.set_i(OPEN, 0);
        self.set_i(PHASE, 0);
    }

    /// Room for `need` bytes in the staging, what was there kept.
    fn room(&mut self, need: usize) {
        if need <= self.staging.len() {
            return;
        }
        let n = need.max(4096).max(self.staging.len() * 2);
        self.staging.resize(n, 0);
        let p = self.staging.as_mut_ptr() as i64;
        self.set_l(BUF, p);
        self.set_i(CAP, n as i32);
    }

    fn key(&mut self, i: usize, k: &crate::lib::adhoc::connection::keyring::Key) {
        self.set_i(RING + 48 * i, k.id as i32);
        // SAFETY: the slot `i` of the ring: 4 bytes of id, 32 of key, 12 of nonce
        unsafe {
            std::ptr::copy_nonoverlapping(k.key.as_ptr(), self.mem.ptr().add(RING + 48 * i + 4), 32);
            std::ptr::copy_nonoverlapping(k.iv.as_ptr(), self.mem.ptr().add(RING + 48 * i + 36), 12);
        }
    }

    /// The key and nonce of a stream's own ChaCha20.
    fn stream_key(&mut self, key: &[u8; 32], nonce: &[u8; 12]) {
        self.key(0, &crate::lib::adhoc::connection::keyring::Key::new(0, *key, *nonce));
        self.set_i(KEYS, 1);
        self.set_l(SEQ, 0);
    }

    /// A pack's keys, at its first bytes: the newest key of the session and a fresh sequence; the Envelope of the transmitter. The
    /// session's - `dst`'s - unless the stages have keys of their own ( `own` ).
    fn keyed_tx(&mut self, dst: &mut Out<'_>, own: Option<&Keys>) {
        if self.has(CHACHA20_PACK) {
            let k = match own {
                None => dst
                    .keys
                    .current()
                    .expect("A pack chain with ChaCha20 on a connection without keys: add a key to its transmitter's and receiver's keys."),
                Some(o) => o.keys.current().expect(NO_OWN_KEY),
            };
            self.key(0, &k);
            self.set_i(KEYS, 1);
            self.set_l(SEQ, crate::lib::chacha20::Core::next_seq() as i64);
        }
        if self.has(ENVELOPE) {
            let flags = self.envelope;
            let e = match own {
                None => dst
                    .envelope
                    .as_ref()
                    .and_then(|e| e.tx.load_full())
                    .expect("A pack chain with Envelope and no keys of it: set the connection's envelope ( chain::Envelope::transmitter: chain::Pack pair, signed_by ) before the pack goes."),
                Some(o) => o.envelope.as_ref().and_then(|e| e.tx.load_full()).expect(NO_OWN_ENVELOPE),
            };
            assert!(flags & 1 == 0 || e.ctx != 0, "The Envelope of the pack is to be sealed - its pair is not set ( chain::Pack::pair )");
            assert!(
                flags & 2 == 0 || e.secret != 0,
                "The Envelope of the pack is to be signed - the key to sign with is not set ( chain::Pack::signed_by )"
            );
            let (ctx, secret, n, id) = (if flags & 1 != 0 { e.ctx } else { 0 }, if flags & 2 != 0 { e.secret } else { 0 }, e.next(), e.id);
            self.set_l(ENV_CTX, ctx);
            self.set_l(ENV_KEY, secret);
            self.set_l(ENV_N, n as i64);
            self.set_i(ENV_ID, id);
        }
    }

    /// A pack's keys on the receiving side: the session's newest ones - the pack names its own -; the Envelope of the receiver. The
    /// session's - `src`'s - unless the stages have keys of their own ( `own` ).
    fn keyed_rx(&mut self, src: &In<'_>, own: Option<&Keys>) {
        if self.has(CHACHA20_PACK) {
            let ring = own.map_or(&*src.keys, |o| &*o.keys);
            let mut n = 0;
            while n < MAX_KEYS {
                let Some(k) = ring.newest(n) else { break };
                self.key(n, &k);
                n += 1;
            }
            self.set_i(KEYS, n as i32);
        }
        if self.has(ENVELOPE) {
            let flags = self.envelope;
            let e = match own {
                None => src
                    .envelope
                    .as_ref()
                    .and_then(|e| e.rx.load_full())
                    .expect("A pack chain with Envelope and no keys of it: set the connection's envelope ( chain::Envelope::receiver: chain::Pack pair, signer_of ) before the pack comes."),
                Some(o) => o.envelope.as_ref().and_then(|e| e.rx.load_full()).expect(NO_OWN_ENVELOPE),
            };
            assert!(flags & 1 == 0 || e.ctx != 0, "The Envelope of the pack is sealed - its pair is not set ( chain::Pack::pair )");
            assert!(
                flags & 2 == 0 || e.public != 0,
                "The Envelope of the pack is signed - whose key it is signed by is not set ( chain::Pack::signer_of )"
            );
            let (ctx, public, id) = (if flags & 1 != 0 { e.ctx } else { 0 }, if flags & 2 != 0 { e.public } else { 0 }, e.id);
            self.set_l(ENV_CTX, ctx);
            self.set_l(ENV_KEY, public);
            self.set_i(ENV_ID, id);
        }
    }

    /// The keystream over `len` bytes at `p`, in place: a lone ChaCha20.
    #[inline]
    fn xor(&mut self, p: *mut u8, len: usize) {
        if 0 < len {
            // SAFETY: the bytes are the caller's, writable
            unsafe { (api().stages_xor)(self.at(), p, len as i32) };
        }
    }

    #[inline]
    fn tx(&mut self, out: *mut u8, room: usize, end: bool) -> i32 {
        // SAFETY: `room` writable bytes at `out`
        unsafe { (api().stages_tx)(self.at(), out, room as i32, end as i32) }
    }

    /// `in_` is written only by a cipher among the stages: deciphered where it lies.
    #[inline]
    fn rx(&mut self, in_: *mut u8, len: usize, end: bool) -> i32 {
        // SAFETY: `len` bytes at `in_`, writable when a cipher is chosen
        unsafe { (api().stages_rx)(self.at(), in_, len as i32, end as i32) }
    }

    /// A cipher among more stages: it deciphers the bytes it is given where they lie.
    #[inline]
    fn writes_input(&self) -> bool {
        (self.has(CHACHA20) || self.has(CHACHA20_PACK)) && !self.in_place
    }
}

// The stages of a pack or a field: driven by the chain of the connection ( `chunked` ).

/// The sending side of a run of built-in stages in a chain of a pack or a field.
pub(crate) struct Tx {
    s: Native,
    eof: bool,
    hdr: [u8; HEADER],
    /// The bytes of the header out; -1 - the key is to be taken.
    hdr_put: isize,
    /// The keys of the stream it carries, when they are not the connection's ( [`Keys`] ).
    own: Option<Keys>,
}

impl Tx {
    pub(crate) fn new(run: Run) -> Self {
        let mut t = Self {
            s: Native::new(true, run),
            eof: false,
            hdr: [0; HEADER],
            hdr_put: HEADER as isize,
            own: None,
        };
        t.rearm();
        t
    }

    /// The keys of the stream it carries, in place of the connection's: a tunnel's ( [`Keys`] ). Until it is armed again: a stream
    /// armed with none takes the connection's.
    #[inline]
    pub(crate) fn keys(&mut self, keys: &Keys) {
        self.own = Some(keys.clone());
    }

    pub(crate) fn rearm(&mut self) {
        self.s.rearm();
        self.own = None;
        self.eof = false;
        self.hdr_put = if self.s.in_place && self.s.has(CHACHA20_PACK) { -1 } else { HEADER as isize };
    }

    /// The bytes of the stages nearer the leaf, through this run, into the window of `dst`. `true` - the stream is out.
    pub(crate) fn get_bytes<F: FnMut(&mut Out<'_>) -> bool>(&mut self, dst: &mut Out<'_>, mut inner: F) -> bool {
        if self.s.in_place {
            // a lone ChaCha20: the key id and the sequence in clear, then the plaintext ciphered where it lies
            if self.hdr_put < 0 {
                self.s.keyed_tx(dst, self.own.as_ref());
                self.hdr[..4].copy_from_slice(&self.s.i(RING).to_le_bytes());
                self.hdr[4..].copy_from_slice(&self.s.l(SEQ).to_le_bytes());
                self.hdr_put = 0;
            }
            while (self.hdr_put as usize) < HEADER {
                if dst.byte_max <= dst.byte_ {
                    return false;
                }
                dst.buffer[dst.byte_] = self.hdr[self.hdr_put as usize];
                dst.byte_ += 1;
                self.hdr_put += 1;
            }
            let from = dst.byte_;
            let done = inner(dst);
            let to = dst.byte_;
            // SAFETY: the bytes the leaf wrote, in the window
            self.s.xor(unsafe { dst.buffer.as_mut_ptr().add(from) }, to - from);
            if done {
                self.s.set_i(PHASE, 0);
                self.hdr_put = if self.s.has(CHACHA20_PACK) { -1 } else { HEADER as isize };
            }
            return done;
        }
        if self.s.i(PHASE) == 0 && self.s.i(LEN) == self.s.i(AT) && !self.eof {
            self.s.keyed_tx(dst, self.own.as_ref());
        }
        loop {
            let mut wrote = false;
            let mut len = self.s.i(LEN) as usize;
            if len == self.s.i(AT) as usize && !self.eof {
                // the stages took all: the serializer writes more
                if !self.s.has(ENVELOPE) || self.s.has(ZSTD) {
                    len = 0;
                    self.s.set_i(LEN, 0);
                    self.s.set_i(AT, 0);
                }
                self.s.room(len + MIN_ROOM);
                let to = {
                    let mut sub = dst.sub(&mut self.s.staging[..]);
                    sub.byte_ = len;
                    self.eof = inner(&mut sub);
                    sub.byte_
                };
                wrote = len < to;
                self.s.set_i(LEN, to as i32);
            }
            let room = dst.byte_max - dst.byte_;
            // SAFETY: the window of `dst`
            let r = self.s.tx(unsafe { dst.buffer.as_mut_ptr().add(dst.byte_) }, room, self.eof);
            if r == NO_ROOM {
                let need = self.s.i(NEED) as usize;
                self.s.room(need);
                continue;
            }
            let done = self.s.i(DONE) as usize;
            dst.byte_ += done;
            assert!(0 <= r, "The stages of a chain failed: out of memory, or no key.");
            if r == 1 {
                self.eof = false;
                return true;
            }
            if dst.byte_max <= dst.byte_ || !wrote && done == 0 && !self.eof && self.s.i(LEN) == self.s.i(AT) {
                return false; // the window is full, or the serializer has nothing now
            }
        }
    }
}

/// The receiving side of a run of built-in stages in a chain of a pack or a field. The end of the stream is the call with an empty
/// window: an Envelope opens then, and only then gives what it holds.
pub(crate) struct Rx {
    s: Native,
    /// The plaintext the stages nearer the leaf did not take yet: `staging[left..left_to]`.
    left: usize,
    left_to: usize,
    /// The pack is forged, or of a key the session has not: the rest of its stream is taken unread.
    discard: bool,
    hdr: [u8; HEADER],
    hdr_got: usize,
    /// The bytes of the wire, writable, for a cipher among more stages: the window is the connection's and read-only. From `wpos`
    /// on not taken yet, the first of them deciphered already as the library counts them.
    wire: Vec<u8>,
    wpos: usize,
    /// The keys of the stream it carries, when they are not the connection's ( [`Keys`] ).
    own: Option<Keys>,
}

impl Rx {
    pub(crate) fn new(run: Run) -> Self {
        let mut r = Self {
            s: Native::new(false, run),
            left: 0,
            left_to: 0,
            discard: false,
            hdr: [0; HEADER],
            hdr_got: HEADER,
            wire: Vec::new(),
            wpos: 0,
            own: None,
        };
        r.rearm();
        r
    }

    /// The keys of the stream it carries, in place of the connection's: a tunnel's ( [`Keys`] ). Until it is armed again: a stream
    /// armed with none takes the connection's.
    #[inline]
    pub(crate) fn keys(&mut self, keys: &Keys) {
        self.own = Some(keys.clone());
    }

    pub(crate) fn rearm(&mut self) {
        self.s.rearm();
        self.own = None;
        self.left = 0;
        self.left_to = 0;
        self.discard = false;
        self.hdr_got = if self.s.in_place && self.s.has(CHACHA20_PACK) { 0 } else { HEADER };
        self.wire.clear();
        self.wpos = 0;
    }

    /// Hands the stages nearer the leaf what the staging holds: `false` - they did not take all of it.
    fn give<F: FnMut(&mut In<'_>)>(&mut self, src: &mut In<'_>, inner: &mut F) -> bool {
        if self.left == self.left_to {
            return true;
        }
        let taken = {
            let mut sub = src.sub(&self.s.staging[..self.left_to]);
            sub.byte_ = self.left;
            inner(&mut sub);
            sub.byte_
        };
        self.left = taken;
        src.error.is_none() && self.left == self.left_to
    }

    /// The pack is dropped: what comes of it is taken unread.
    fn drop_pack(&mut self, src: &mut In<'_>) {
        self.discard = true;
        src.drop_received = true;
        src.byte_ = src.byte_max;
    }

    /// The window of `src` through this run, the plaintext to the stages nearer the leaf.
    pub(crate) fn put_bytes<F: FnMut(&mut In<'_>)>(&mut self, src: &mut In<'_>, mut inner: F) {
        if self.discard {
            src.byte_ = src.byte_max;
            return;
        }
        if self.s.in_place {
            return self.lone(src, &mut inner);
        }
        if !self.give(src, &mut inner) {
            return; // they take the rest first
        }
        let end = src.byte_max <= src.byte_;
        if self.s.i(PHASE) == 0 && self.s.i(TAKEN) == 0 {
            self.s.keyed_rx(src, self.own.as_ref());
        }
        let copy = self.s.writes_input();
        if copy {
            // what the stages did not take before goes first, as it is
            self.wire.drain(..self.wpos);
            self.wpos = 0;
            self.wire.extend_from_slice(&src.buffer[src.byte_..src.byte_max]);
            src.byte_ = src.byte_max;
        }
        loop {
            let r = if copy {
                let n = self.wire.len() - self.wpos;
                // SAFETY: the bytes of `wire` from `wpos`
                let p = unsafe { self.wire.as_mut_ptr().add(self.wpos) };
                self.s.rx(p, n, end)
            } else {
                // SAFETY: no cipher among the stages: the library only reads the window
                let p = unsafe { src.buffer.as_ptr().add(src.byte_) } as *mut u8;
                self.s.rx(p, src.byte_max - src.byte_, end)
            };
            if r == NO_ROOM {
                let need = self.s.i(NEED) as usize;
                self.s.room(need);
                continue;
            }
            let taken = self.s.i(TAKEN) as usize;
            if copy {
                self.wpos += taken;
            } else {
                src.byte_ += taken;
            }
            if r < 0 {
                return self.drop_pack(src); // forged, not of its signer, or of a key this session has not
            }
            self.left = self.s.i(AT) as usize;
            self.left_to = self.s.i(LEN) as usize;
            if !self.give(src, &mut inner) || r == 1 {
                return;
            }
            let more = if copy { self.wpos < self.wire.len() } else { src.byte_ < src.byte_max };
            if !end && !more || end && self.left_to == 0 {
                return;
            }
        }
    }

    /// A lone ChaCha20: the header in clear, then the window deciphered into the staging - it is the connection's and read-only.
    fn lone<F: FnMut(&mut In<'_>)>(&mut self, src: &mut In<'_>, inner: &mut F) {
        if self.hdr_got < HEADER {
            while self.hdr_got < HEADER {
                if src.byte_max <= src.byte_ {
                    return;
                }
                self.hdr[self.hdr_got] = src.buffer[src.byte_];
                src.byte_ += 1;
                self.hdr_got += 1;
            }
            let id = u32::from_le_bytes(self.hdr[..4].try_into().unwrap());
            let seq = i64::from_le_bytes(self.hdr[4..].try_into().unwrap());
            match self.own.as_ref().map_or(&*src.keys, |o| &*o.keys).find(id) {
                Some(k) => {
                    self.s.key(0, &k);
                    self.s.set_i(KEYS, 1);
                    self.s.set_l(SEQ, seq);
                    self.s.set_i(PHASE, 0);
                }
                None => return self.drop_pack(src), // a key this session has not: the pack is dropped, the connection goes on
            }
        }
        loop {
            if !self.give(src, inner) {
                return;
            }
            let n = (src.byte_max - src.byte_).min(MIN_ROOM * 16);
            if n == 0 {
                return;
            }
            self.s.room(n);
            self.s.staging[..n].copy_from_slice(&src.buffer[src.byte_..src.byte_ + n]);
            src.byte_ += n;
            let p = self.s.staging.as_mut_ptr();
            self.s.xor(p, n);
            self.left = 0;
            self.left_to = n;
        }
    }
}

// The stages of the stream of a connection: over its transport.

pub mod transmitter {
    use super::*;

    /// The source of a connection through a run of built-in stages: `Zstd`, a `ChaCha20` of a stream's own key, both. The outermost
    /// gives the wire. Where the upper layer has nothing now, what the codec holds goes out: the end of a frame, the peer decodes each
    /// burst whole.
    pub struct Stage<S: BytesSrc> {
        inner: S,
        s: Native,
        /// Plain bytes went into the stages since the last drain: they go out once the upper layer runs dry.
        dirty: bool,
        /// The plain bytes taken from the upper layer so far.
        pub plain_bytes: u64,
        /// The bytes given to the transport so far.
        pub wire_bytes: u64,
    }

    impl<S: BytesSrc> Stage<S> {
        /// `key` and `nonce` - of a ChaCha20 in the run, 32 and 12 bytes: the runtime arguments of the stage.
        pub fn new(inner: S, run: Run, key: Option<(&[u8; 32], &[u8; 12])>) -> Self {
            let mut s = Native::new(true, run);
            assert!(
                !s.has(ENVELOPE) && !s.has(CHACHA20_PACK),
                "An Envelope, and a ChaCha20 of the session's keys, take packs: not the stream of a connection."
            );
            if let Some((k, n)) = key {
                s.stream_key(k, n);
            }
            if !s.in_place {
                s.room(PLAIN);
            }
            Self {
                inner,
                s,
                dirty: false,
                plain_bytes: 0,
                wire_bytes: 0,
            }
        }
    }

    impl<S: BytesSrc> BytesSrc for Stage<S> {
        fn read(&mut self, dst: &mut [u8]) -> isize {
            if self.s.in_place {
                // encrypted in place, in the transport's buffer: length-preserving
                let n = self.inner.read(dst);
                if 0 < n {
                    self.s.xor(dst.as_mut_ptr(), n as usize);
                    self.plain_bytes += n as u64;
                    self.wire_bytes += n as u64;
                }
                return n;
            }
            let cap = dst.len();
            let (mut put, mut last, mut asked) = (0usize, 0isize, false);
            while put < cap {
                if self.s.i(LEN) == self.s.i(AT) {
                    // all taken: the upper layer writes more
                    self.s.set_i(LEN, 0);
                    self.s.set_i(AT, 0);
                    asked = true;
                    last = self.inner.read(&mut self.s.staging[..]);
                    if 0 < last {
                        self.s.set_i(LEN, last as i32);
                        self.plain_bytes += last as u64;
                        self.dirty = true;
                    }
                }
                let drain = asked && last < 1 && self.dirty; // the upper layer has nothing now: what the codec holds goes out
                let at = self.s.i(AT);
                // SAFETY: the rest of `dst`
                let r = self.s.tx(unsafe { dst.as_mut_ptr().add(put) }, cap - put, drain);
                assert!(0 <= r, "The stages of a connection failed.");
                let done = self.s.i(DONE) as usize;
                put += done;
                if r == 1 {
                    self.dirty = false;
                }
                if asked && last < 1 && !self.dirty || r == 0 && done == 0 && at == self.s.i(AT) && !drain && self.s.i(LEN) != at {
                    break; // dry, or the codec takes nothing more now
                }
            }
            self.wire_bytes += put as u64;
            if 0 < put {
                put as isize
            } else {
                last
            }
        }

        /// Another source may take the transport only between frames, and when the upper layer is between packs.
        #[inline]
        fn at_boundary(&self) -> bool {
            (self.s.in_place || !self.dirty && self.s.i(LEN) == self.s.i(AT)) && self.inner.at_boundary()
        }
    }
}

pub mod receiver {
    use super::*;

    /// The sink of a connection through a run of built-in stages, the pair of [`super::transmitter::Stage`]: the wire first.
    pub struct Stage<K: BytesDst> {
        pub(crate) inner: K,
        s: Native,
        /// `staging[at..len)` - the plaintext the sink did not take yet.
        at: usize,
        len: usize,
        /// The last call filled the staging: the codec may hold more of what it took.
        more: bool,
        /// The bytes at the front of the next offer the stages have taken already: a byte left with the transport so that it offers
        /// again. Of a lone ChaCha20: the bytes at the front of the next offer deciphered already.
        skip: usize,
        /// Read-only bytes and a cipher: copied here, deciphered here; from `held_at` on what the stages did not take yet.
        held: Vec<u8>,
        held_at: usize,
        /// The bytes of the wire taken so far.
        pub wire_bytes: u64,
        /// The plain bytes the sink took so far.
        pub plain_bytes: u64,
    }

    impl<K: BytesDst> Stage<K> {
        /// `key` and `nonce` - of a ChaCha20 in the run, 32 and 12 bytes: the runtime arguments of the stage.
        pub fn new(inner: K, run: Run, key: Option<(&[u8; 32], &[u8; 12])>) -> Self {
            let mut s = Native::new(false, run);
            assert!(
                !s.has(ENVELOPE) && !s.has(CHACHA20_PACK),
                "An Envelope, and a ChaCha20 of the session's keys, take packs: not the stream of a connection."
            );
            if let Some((k, n)) = key {
                s.stream_key(k, n);
            }
            Self {
                inner,
                s,
                at: 0,
                len: 0,
                more: false,
                skip: 0,
                held: Vec::new(),
                held_at: 0,
                wire_bytes: 0,
                plain_bytes: 0,
            }
        }

        /// The plaintext the sink did not take yet: `false` - it still holds some.
        fn give(&mut self) -> Option<bool> {
            if self.at < self.len {
                let n = self.inner.on_slice(&mut self.s.staging[self.at..self.len])?;
                self.at += n;
                self.plain_bytes += n as u64;
            }
            Some(self.at == self.len)
        }

        /// `n` bytes at `p` through the stages, as far as the sink takes the plaintext: how many of them were taken. Writable when a
        /// cipher is among the stages. The bytes not taken are offered again, the first of them deciphered already as the library
        /// counts them.
        fn feed(&mut self, p: *mut u8, n: usize) -> Option<usize> {
            let mut took = 0;
            loop {
                if !self.give()? || n <= took && !self.more {
                    return Some(took);
                }
                // SAFETY: the bytes from `took` on of the `n` at `p`
                let r = self.s.rx(unsafe { p.add(took) }, n - took, false);
                if r == NO_ROOM {
                    let need = self.s.i(NEED) as usize;
                    self.s.room(need);
                    continue;
                }
                if r < 0 {
                    return None; // the stream is broken
                }
                let t = self.s.i(TAKEN) as usize;
                took += t;
                self.wire_bytes += t as u64;
                (self.at, self.len) = (self.s.i(AT) as usize, self.s.i(LEN) as usize);
                self.more = self.len == self.s.staging.len();
                if t == 0 && self.at == self.len {
                    self.more = false;
                    return Some(took); // nothing more now
                }
            }
        }

        /// What `held` still has, through the stages: `false` - the sink holds the plaintext of some of it.
        fn flush_held(&mut self) -> Option<bool> {
            if self.held_at < self.held.len() || self.more {
                let mut held = std::mem::take(&mut self.held);
                let n = held.len() - self.held_at;
                let r = if self.s.in_place {
                    // SAFETY: the bytes of `held` from `held_at`
                    let slice = unsafe { std::slice::from_raw_parts_mut(held.as_mut_ptr().add(self.held_at), n) };
                    self.lone(slice)
                } else {
                    // SAFETY: the bytes of `held` from `held_at`
                    self.feed(unsafe { held.as_mut_ptr().add(self.held_at) }, n)
                };
                self.held = held;
                self.held_at += r?;
                if self.held_at < self.held.len() {
                    return Some(false);
                }
                self.held.clear();
                self.held_at = 0;
            }
            self.give()
        }

        /// Read-only bytes and a cipher: copied, deciphered in the copy; all of them taken once what was held before is through.
        fn copied(&mut self, bytes: &[u8]) -> Option<usize> {
            if !self.flush_held()? {
                return Some(0);
            }
            self.held.extend_from_slice(bytes);
            self.flush_held()?;
            Some(bytes.len())
        }

        /// A lone ChaCha20: deciphered where they lie, once - a refused tail stays deciphered for the next offer.
        fn lone(&mut self, bytes: &mut [u8]) -> Option<usize> {
            if self.skip < bytes.len() {
                // SAFETY: the bytes from `skip` on
                let p = unsafe { bytes.as_mut_ptr().add(self.skip) };
                self.s.xor(p, bytes.len() - self.skip);
                self.wire_bytes += (bytes.len() - self.skip) as u64;
                self.skip = bytes.len();
            }
            let n = self.inner.on_slice(bytes)?;
            self.skip = bytes.len() - n;
            self.plain_bytes += n as u64;
            Some(n)
        }

        /// `n` bytes at `p` from the transport through the stages: writable when a cipher is among them.
        fn offered(&mut self, p: *mut u8, n: usize) -> Option<usize> {
            if !self.flush_held()? {
                return Some(0);
            }
            let skip = self.skip.min(n);
            self.skip -= skip;
            // SAFETY: the bytes from `skip` on of the `n` at `p`
            let took = skip + self.feed(unsafe { p.add(skip) }, n - skip)?;
            if (self.at < self.len || self.more) && 0 < took && took == n {
                // the sink holds plaintext and the transport nothing: a byte stays with it, so that it offers again
                self.skip = 1;
                return Some(took - 1);
            }
            Some(took)
        }
    }

    impl<K: BytesDst> BytesDst for Stage<K> {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.on_part(bytes) == Some(bytes.len())
        }

        /// The transport's own bytes: deciphered where they lie.
        fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
            if self.s.in_place {
                if !self.flush_held()? {
                    return Some(0);
                }
                return self.lone(bytes);
            }
            self.offered(bytes.as_mut_ptr(), bytes.len())
        }

        fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
            if self.s.in_place || self.s.writes_input() {
                return self.copied(bytes); // a cipher writes what it deciphers
            }
            // no cipher: the library only reads them
            self.offered(bytes.as_ptr() as *mut u8, bytes.len())
        }

        fn on_bytes_mut(&mut self, bytes: &mut [u8]) -> bool {
            self.on_slice(bytes) == Some(bytes.len())
        }

        fn graceful_close(&self) -> bool {
            self.inner.graceful_close()
        }

        fn broken(&self) -> bool {
            self.inner.broken()
        }

        fn wants_more(&self) -> bool {
            self.inner.wants_more()
        }

        /// What the stages hold goes on, then what the sink holds.
        fn retry(&mut self) {
            let _ = self.flush_held();
            self.inner.retry()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Src(Vec<Vec<u8>>, bool);

    impl BytesSrc for Src {
        /// The parts one after another; `.1` - the source runs dry between them once.
        fn read(&mut self, dst: &mut [u8]) -> isize {
            let Some(p) = self.0.first_mut() else { return -1 };
            let n = p.len().min(dst.len());
            dst[..n].copy_from_slice(&p[..n]);
            p.drain(..n);
            if p.is_empty() {
                self.0.remove(0);
                if self.1 {
                    self.1 = false;
                    return n as isize;
                }
            }
            n as isize
        }
    }

    /// Takes at most `room` bytes a call and refuses every `refuse`-th call.
    struct Sink(Vec<u8>, usize, usize, usize);

    impl BytesDst for Sink {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.0.extend_from_slice(bytes);
            true
        }

        fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
            self.3 += 1;
            if 0 < self.2 && self.3 % self.2 == 0 {
                return Some(0);
            }
            let n = bytes.len().min(self.1);
            self.0.extend_from_slice(&bytes[..n]);
            Some(n)
        }
    }

    fn parts() -> Vec<Vec<u8>> {
        let mut rnd = 0x9E37_79B9_7F4A_7C15u64;
        (0..14)
            .map(|i| {
                (0..[1usize, 5, 127, 128, 129, 4096, 40_000, 70_000, 200_000][i % 9])
                    .map(|_| {
                        rnd ^= rnd << 13;
                        rnd ^= rnd >> 7;
                        rnd ^= rnd << 17;
                        (rnd % if i % 2 == 0 { 4 } else { 256 }) as u8
                    })
                    .collect()
            })
            .collect()
    }

    const KEY: [u8; 32] = [7; 32];
    const IV: [u8; 12] = [9; 12];

    fn run(kind1: i32, level1: i32, kind2: i32) -> Run {
        Run { kind1, level1, kind2, level2: 0 }
    }

    fn wire(r: Run, key: bool, buf: usize) -> (Vec<u8>, Vec<u8>) {
        let parts = parts();
        let all = parts.concat();
        let mut src = transmitter::Stage::new(Src(parts, true), r, key.then_some((&KEY, &IV)));
        let (mut wire, mut b) = (Vec::new(), vec![0u8; buf]);
        let mut idle = 0;
        loop {
            match src.read(&mut b) {
                -1 => break,
                0 => {
                    idle += 1;
                    assert!(idle < 100, "stuck");
                }
                n => {
                    idle = 0;
                    wire.extend_from_slice(&b[..n as usize])
                }
            }
        }
        assert!(src.at_boundary());
        assert_eq!(src.plain_bytes, all.len() as u64);
        (wire, all)
    }

    fn take(r: Run, key: bool, wire: &[u8], all: &[u8], chunk: usize, room: usize, refuse: usize, writable: bool) {
        let mut sink = receiver::Stage::new(Sink(Vec::new(), room, refuse, 0), r, key.then_some((&KEY, &IV)));
        let mut at = 0;
        let mut copy = wire.to_vec();
        let mut stalls = 0;
        while at < wire.len() {
            let end = (at + chunk).min(wire.len());
            let before = sink.inner.0.len();
            let n = if writable { sink.on_slice(&mut copy[at..end]) } else { sink.on_part(&wire[at..end]) }.expect("not broken");
            at += n;
            stalls = if n == 0 && sink.inner.0.len() == before { stalls + 1 } else { 0 };
            assert!(stalls < 10, "stuck at {at} of {}, chunk {chunk} room {room}", wire.len());
        }
        let mut idle = 0;
        while sink.inner.0.len() < all.len() && idle < 10 {
            let before = sink.inner.0.len();
            sink.retry(); // the sink refused: what is held goes on
            idle = if sink.inner.0.len() == before { idle + 1 } else { 0 };
        }
        assert!(
            sink.inner.0 == all,
            "chunk {chunk} room {room} refuse {refuse} writable {writable}: {} of {}",
            sink.inner.0.len(),
            all.len()
        );
    }

    fn round_trip(r: Run, key: bool) {
        for buf in [128usize, 4096, 1 << 16] {
            let (wire, all) = wire(r, key, buf);
            for (chunk, room, refuse) in [(1usize, 1usize << 30, 0usize), (7, 1000, 3), (4096, 3, 0), (1 << 16, 1 << 20, 2), (1 << 20, 1 << 30, 0)] {
                for writable in [true, false] {
                    take(r, key, &wire, &all, chunk, room, refuse, writable);
                }
            }
        }
    }

    #[test]
    fn stages_connection_zstd() {
        round_trip(run(ZSTD, 3, 0), false);
    }

    #[test]
    fn stages_connection_zstd_chacha20() {
        round_trip(run(ZSTD, 3, CHACHA20), true);
    }

    /// The source runs dry after each part: each burst ends a frame of Zstd, the keystream of ChaCha20 goes on across them.
    #[test]
    fn stages_connection_zstd_chacha20_bursts() {
        struct Dry(Vec<Vec<u8>>, bool);
        impl BytesSrc for Dry {
            fn read(&mut self, dst: &mut [u8]) -> isize {
                if std::mem::take(&mut self.1) {
                    return 0;
                }
                let Some(p) = self.0.first_mut() else { return -1 };
                let n = p.len().min(dst.len());
                dst[..n].copy_from_slice(&p[..n]);
                p.drain(..n);
                if p.is_empty() {
                    self.0.remove(0);
                    self.1 = true;
                }
                n as isize
            }
        }
        let r = run(ZSTD, 3, CHACHA20);
        let parts = parts();
        let all = parts.concat();
        let mut src = transmitter::Stage::new(Dry(parts, false), r, Some((&KEY, &IV)));
        let (mut wire, mut b) = (Vec::new(), vec![0u8; 4096]);
        loop {
            match src.read(&mut b) {
                -1 => break,
                n => wire.extend_from_slice(&b[..n.max(0) as usize]),
            }
        }
        take(r, true, &wire, &all, 4096, 1 << 30, 0, true);
    }

    #[test]
    fn stages_connection_chacha20() {
        let r = run(CHACHA20, 0, 0);
        let (wire, all) = wire(r, true, 4096);
        assert!(wire.len() == all.len() && wire != all, "length-preserving, ciphered");
        round_trip(r, true);
    }
}
