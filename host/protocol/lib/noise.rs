// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Noise_*_25519_ChaChaPoly_SHA256 handshakes and an encrypted record stream, in the native library.
//!
//! [`NoiseNK`] runs any of the patterns NK, IK, XX, KK: one call a message makes every hash,
//! agreement and key of it, and when the last message is through the transport keys and the key of
//! the packs lie made in its memory. [`NoiseStream`] carries the application records.
//!
//! ```text
//! <- s              server static public key known to the client (NK)
//! ...
//! -> e, es          client handshake message
//! <- e, ee          server handshake message
//! ```

use std::io;
use std::sync::Arc;

use super::chacha20::{Aead, TAG};
use super::crypto::{api, Mem};
use crate::lib::adhoc::connection::keyring::Key;
use crate::lib::network::buf::Buf;
use crate::lib::network::engine::Loop;

/// What a [`ByteSource`] gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    /// This many bytes were written into the offered buffer; the source is still open.
    Bytes(usize),
    /// Nothing now, still open.
    Pending,
    /// The end of the stream: no bytes.
    Eof,
}

/// The plaintext a [`NoiseStream`] seals into records.
pub trait ByteSource {
    /// Fills `buf` with up to `buf.len()` bytes.
    fn pull(&mut self, buf: &mut [u8]) -> io::Result<Pull>;

    /// The bytes of the last pull are frames of tunnels with a Noise of their own: they go in a
    /// record in the clear ([`CLEAR`]). Default: no.
    #[inline(always)]
    fn clear(&self) -> bool {
        false
    }
}

impl<T: ByteSource + ?Sized> ByteSource for &mut T {
    #[inline]
    fn pull(&mut self, buf: &mut [u8]) -> io::Result<Pull> {
        (**self).pull(buf)
    }

    #[inline]
    fn clear(&self) -> bool {
        (**self).clear()
    }
}

/// Where a [`NoiseStream`] hands the plaintext of the records it opens.
pub trait ByteSink {
    /// Takes up to `bytes.len()` bytes; returns how many. `Ok(0)` - none now: they are offered
    /// again later.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;

    /// As [`write`](Self::write), the bytes of a record in the clear ([`CLEAR`]): only frames of
    /// tunnels with a Noise of their own may be there. Default: none may - the stream is broken.
    fn write_clear(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(NoiseError.into())
    }

    /// The protocol sink under it refused the stream for good
    /// ([`crate::lib::adhoc::BytesDst::broken`]). Default: no.
    fn broken(&self) -> bool {
        false
    }

    /// The socket is gone and the session parks: the protocol sink under it keeps what survives a
    /// reconnect ([`crate::lib::adhoc::BytesDst::suspend`]). Default: nothing.
    fn suspend(&mut self) {}
}

impl<T: ByteSink + ?Sized> ByteSink for &mut T {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (**self).write(bytes)
    }

    #[inline]
    fn write_clear(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (**self).write_clear(bytes)
    }
}

/// What the handshake of a stream is bound to.
pub const STREAM_PROLOGUE: &[u8] = b"AdHoc stream 1";

/// Handshake / key length (`NoiseNK.KEY`).
pub const KEY: usize = 32;

/// The overhead of a handshake message of NK: the ephemeral key and the tag of the payload
/// (`NoiseNK.OVERHEAD`).
pub const OVERHEAD: usize = KEY + TAG;

/// The patterns of the Noise Protocol Framework. NK: the client knows the server. IK: and proves
/// itself in its first message. XX: neither knows the other beforehand. KK: both know.
pub const PATTERN_NK: i32 = 0;
pub const PATTERN_IK: i32 = 1;
pub const PATTERN_XX: i32 = 2;
pub const PATTERN_KK: i32 = 3;

/// Decides who may come in, by the key the other side has proved.
pub type OnPeer = Arc<dyn Fn(&[u8; KEY]) -> bool + Send + Sync>;

/// Out of turn, a short message, a low-order peer key, or a forged one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoiseError;

impl std::fmt::Display for NoiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("noise handshake error")
    }
}

impl std::error::Error for NoiseError {}

impl From<NoiseError> for io::Error {
    fn from(e: NoiseError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

/// Zeroes a secret so that the write is not optimized away as dead.
#[inline(never)]
pub(crate) fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY: a valid, aligned byte of the slice
        unsafe { std::ptr::write_volatile(x, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

// The memory of a handshake: adhoc_crypto.c, `Hs`.
const H: usize = 0;
const PACK_KEY: usize = 32;
const PACK_IV: usize = 64;
const DONE: usize = 76;
const PEER: usize = 80;

/// The handshake of an encrypted connection, in native memory. The transport keys it makes lie in
/// that memory too: an [`Aead`] of [`split`](Self::split) holds it.
pub struct NoiseNK {
    mem: Arc<Mem>,
}

impl NoiseNK {
    /// Any of the patterns. `static_secret`: this side's, `None` - the pattern gives it none.
    /// `static_public`: of that secret, computed once - `None`: the library computes it. `peer`: the
    /// other side's static key where the pattern has it known beforehand. `ephemeral` is for tests
    /// only.
    pub fn of(pattern: i32, initiator: bool, prologue: &[u8], static_secret: Option<&[u8; KEY]>, static_public: Option<&[u8; KEY]>, peer: Option<&[u8; KEY]>, ephemeral: Option<&[u8; KEY]>) -> Self {
        let a = api();
        let mem = Arc::new(Mem::new(unsafe { (a.noise_size)() } as usize));
        let or_null = |k: Option<&[u8; KEY]>| k.map_or(std::ptr::null(), |k| k.as_ptr());
        // SAFETY: the memory is of the size the library said, the keys of 32 bytes
        unsafe {
            (a.noise_init)(
                mem.at(),
                pattern,
                initiator as i32,
                prologue.as_ptr(),
                prologue.len() as i32,
                or_null(static_secret),
                or_null(static_public),
                or_null(peer),
                or_null(ephemeral),
            )
        };
        Self { mem }
    }

    /// The client's side: it knows the server's static public key. `ephemeral` is for tests only:
    /// the ephemeral secret to use instead of a random one.
    pub fn initiator(server_public: &[u8; KEY], prologue: &[u8], ephemeral: Option<&[u8; KEY]>) -> Self {
        Self::of(PATTERN_NK, true, prologue, None, None, Some(server_public), ephemeral)
    }

    /// The server's side: its static key pair. `static_public` is of `static_secret`: the server
    /// computes it once, not a curve multiplication a handshake.
    pub fn responder(static_secret: &[u8; KEY], static_public: &[u8; KEY], prologue: &[u8], ephemeral: Option<&[u8; KEY]>) -> Self {
        Self::of(PATTERN_NK, false, prologue, Some(static_secret), Some(static_public), None, ephemeral)
    }

    /// Writes this side's next handshake message into `dst`, with `payload`, encrypted.
    pub fn write_message(&mut self, dst: &mut [u8], payload: &[u8]) -> Result<usize, NoiseError> {
        // SAFETY: the bounds go with the pointers
        let n = unsafe { (api().noise_write)(self.mem.at(), dst.as_mut_ptr(), dst.len() as i32, payload.as_ptr(), payload.len() as i32) };
        if n < 0 {
            Err(NoiseError)
        } else {
            Ok(n as usize)
        }
    }

    /// Reads the peer's handshake message; its payload is decrypted into `payload`. A message that
    /// fails leaves the state as it was: a forged one ahead of the real one does not spoil the
    /// handshake.
    pub fn read_message(&mut self, msg: &[u8], payload: &mut [u8]) -> Result<usize, NoiseError> {
        // SAFETY: the bounds go with the pointers
        let n = unsafe { (api().noise_read)(self.mem.at(), msg.as_ptr(), msg.len() as i32, payload.as_mut_ptr(), payload.len() as i32) };
        if n < 0 {
            Err(NoiseError)
        } else {
            Ok(n as usize)
        }
    }

    /// All the messages are exchanged (`Done`).
    #[inline]
    pub fn done(&self) -> bool {
        // SAFETY: inside the memory of the handshake
        unsafe { (self.mem.ptr().add(DONE) as *const i32).read() != 0 }
    }

    /// The handshake hash: the same on both sides, unique to this handshake — a channel binding
    /// (`HandshakeHash`).
    pub fn handshake_hash(&self) -> [u8; 32] {
        self.bytes(H)
    }

    /// The static key of the other side, proved by the handshake, once it is done.
    pub fn peer_key(&self) -> [u8; KEY] {
        self.bytes(PEER)
    }

    #[inline]
    fn bytes<const N: usize>(&self, at: usize) -> [u8; N] {
        // SAFETY: inside the memory of the handshake
        unsafe { (self.mem.ptr().add(at) as *const [u8; N]).read_unaligned() }
    }

    /// The transport keys once the handshake is done: one ChaCha20-Poly1305 key per direction,
    /// `(send, receive)`, each message numbered by its sender from 0. They lie in this handshake's
    /// memory.
    pub fn split(&mut self) -> (Aead, Aead) {
        assert!(self.done(), "the handshake is not done");
        let a = api();
        // SAFETY: offsets the library gave
        let (tx, rx) = unsafe { ((a.noise_tx)() as i64, (a.noise_rx)() as i64) };
        (Aead::inside(self.mem.clone(), self.mem.at() + tx), Aead::inside(self.mem.clone(), self.mem.at() + rx))
    }

    /// The key this handshake gives the pack stages of the session: 32 bytes of key and 12 of base
    /// nonce, the id from the handshake hash — the same on both sides, never 0. Before
    /// [`wipe`](Self::wipe).
    pub fn pack_key(&self) -> Key {
        assert!(self.done(), "the handshake is not done");
        let id = u32::from_le_bytes(self.bytes::<4>(H));
        Key::new(if id == 0 { 1 } else { id }, self.bytes(PACK_KEY), self.bytes(PACK_IV))
    }

    /// What is secret and not needed any more goes. When the handshake is done that is the key of
    /// the packs; its transport keys stay until their [`Aead::wipe`].
    pub fn wipe(&mut self) {
        let (from, to) = if self.done() { (PACK_KEY, DONE) } else { (0, self.mem.len()) };
        // SAFETY: inside the memory of the handshake
        wipe(unsafe { std::slice::from_raw_parts_mut(self.mem.ptr().add(from), to - from) });
    }
}

/// The secrets of a handshake do not outlive it.
impl Drop for NoiseNK {
    fn drop(&mut self) {
        self.wipe();
    }
}

// NoiseStream — the record layer over a byte stream

/// The plaintext cap of one record (`NoiseStream.RECORD`).
pub const RECORD: usize = 16 * 1024;

/// The record header length (`NoiseStream.HEADER`).
pub const HEADER: usize = 2;

/// The high bit of the length of a record: a record in the clear - its bytes as they are, no tag,
/// no nonce counted. Frames of tunnels with a Noise of their own go so: their records are sealed
/// end to end, once (`NoiseStream.CLEAR`).
pub const CLEAR: usize = 0x8000;

/// The sealed runs between the records in the clear that one pull of a chain queues: each is an
/// operation of the chain, and the chain holds a few.
const CHAIN_RUNS: usize = 3;

/// The longest message of the patterns, with no payload: 96 bytes, and its length.
const HS_BUFFER: usize = 128;

/// The handshake messages, going out and coming in: gone when the handshake is over.
struct HsBuffers {
    out: [u8; HS_BUFFER],
    came: [u8; HS_BUFFER],
}

/// Encrypted record stream shared by the transport directions.
///
/// Handshake messages carry a one-byte length. Application records use a two-byte little-endian
/// length, ciphertext, and a 16-byte authentication tag, with at most [`RECORD`] plaintext bytes.
/// The length is associated data and each direction uses its record counter as the nonce.
/// Authentication or ordering failure terminates the stream.
///
/// A length with [`CLEAR`] set is of a record in the clear: the frames of tunnels with a Noise of
/// their own the source gives ([`ByteSource::clear`]), as they are, with no tag and no nonce. The
/// sink takes them apart ([`ByteSink::write_clear`]): anything else there breaks the stream.
///
/// [`pull_from`](Self::pull_from) and [`push_to`](Self::push_to) receive the active source and sink
/// on each call. The optional wake callback signals handshake output ready to send.
///
/// A record that lies whole in the transport's buffer is opened there. Only a record cut by the end
/// of that buffer, and the plaintext the destination refuses, go through a whole-record buffer of
/// the loop, taken while one comes that way.
pub struct NoiseStream {
    hs: Option<NoiseNK>,
    tx: Option<Aead>,
    rx: Option<Aead>,
    tx_n: u64,
    rx_n: u64,

    hs_buffers: Option<Box<HsBuffers>>,
    hs_out_len: usize,
    hs_out_pos: usize,

    /// The record coming in, of its buffer: what of it arrived, how long it is, its opened plaintext
    /// still to hand on.
    rec: Option<Buf>,
    got: usize,
    need: usize,
    plain_pos: usize,
    plain_end: usize,
    /// Bytes ahead in the transport's buffer that are whole records opened already; of the current
    /// one, the plaintext not yet taken and the tag not yet passed.
    opened: usize,
    body: usize,
    tag_left: usize,
    /// A byte taken already, kept in the transport's hands: see [`keep_tail`](Self::keep_tail).
    skip: bool,
    /// The bytes of the record in the clear coming in still to hand on: straight from the offer.
    clear_left: usize,
    /// The chain has been through this offer: no call for what it could not open.
    ahead: bool,

    failed: bool,
    /// Records that did not open: the stream ended at the first.
    pub forged: i64,

    wake: Option<Box<dyn FnMut() + Send>>,
    on_peer: Option<OnPeer>,
    peer_key: Option<[u8; KEY]>,
    /// The same key for a reader that must not wait for this stream: the receive of the connection holds its lock while the
    /// protocol under it takes a pack, and the login handler there asks who it is.
    pub(crate) peer: Arc<std::sync::OnceLock<[u8; KEY]>>,

    /// The stream rides a tunnel, a burst at a time: while the keys are not there yet the burst
    /// ends ([`Pull::Eof`]) instead of holding its carrier ([`Pull::Pending`]), and the wake of the
    /// handshake queues the next one.
    pub(crate) burst: bool,

    /// The shared memory of the chain this stream is a layer of: it seals with the others, in their
    /// one call. Null - by itself.
    chain: *mut u8,
    /// The loop of that chain: where the whole-record buffers come from.
    lp: *const Loop,
}

// SAFETY: `chain` and `lp` are set and used on the loop of the connection, the rest is plain data
unsafe impl Send for NoiseStream {}

impl NoiseStream {
    /// Any pattern. `first`: this side writes the first message.
    pub fn of(mut hs: NoiseNK, first: bool, wake: Option<Box<dyn FnMut() + Send>>, on_peer: Option<OnPeer>) -> Self {
        let mut buffers = Box::new(HsBuffers {
            out: [0; HS_BUFFER],
            came: [0; HS_BUFFER],
        });
        let mut hs_out_len = 0;
        if first {
            hs_out_len = 1 + hs.write_message(&mut buffers.out[1..], &[]).expect("a fresh initiator can always write message 1");
            buffers.out[0] = (hs_out_len - 1) as u8;
        }
        Self {
            hs: Some(hs),
            tx: None,
            rx: None,
            tx_n: 0,
            rx_n: 0,
            hs_buffers: Some(buffers),
            hs_out_len,
            hs_out_pos: 0,
            rec: None,
            got: 0,
            need: 1,
            plain_pos: 0,
            plain_end: 0,
            opened: 0,
            body: 0,
            tag_left: 0,
            skip: false,
            clear_left: 0,
            ahead: false,
            failed: false,
            forged: 0,
            wake,
            on_peer,
            peer_key: None,
            peer: Arc::new(std::sync::OnceLock::new()),
            burst: false,
            chain: std::ptr::null_mut(),
            lp: std::ptr::null(),
        }
    }

    /// The client's end: it pinned the server's static public key, and sends the first message at
    /// once.
    pub fn client(server_public: &[u8; KEY], wake: Option<Box<dyn FnMut() + Send>>) -> Self {
        Self::of(NoiseNK::initiator(server_public, STREAM_PROLOGUE, None), true, wake, None)
    }

    /// The server's end, with its static secret key: it waits for the client's first message.
    pub fn server(static_secret: &[u8; KEY], static_public: &[u8; KEY], wake: Option<Box<dyn FnMut() + Send>>) -> Self {
        Self::of(NoiseNK::responder(static_secret, static_public, STREAM_PROLOGUE, None), false, wake, None)
    }

    /// Both directions have their keys (`Ready`).
    pub fn ready(&self) -> bool {
        self.tx.is_some()
    }

    /// The key of the other side, proved by the handshake: for a pattern that brings it.
    pub fn peer_key(&self) -> Option<&[u8; KEY]> {
        self.peer_key.as_ref()
    }

    /// The stream broke (a forged record, a failed handshake).
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// A message of the handshake waits to go (`Pending`).
    pub(crate) fn pending(&self) -> bool {
        self.hs_out_pos < self.hs_out_len
    }

    /// This stream is a layer of the chain of `chain` on the loop `lp`.
    pub(crate) fn link(&mut self, chain: *mut u8, lp: *const Loop) {
        self.chain = chain;
        self.lp = lp;
    }

    /// The chain may open the records of the next offer: nothing of a record is held here.
    #[inline]
    pub(crate) fn aligned(&self) -> bool {
        self.rx.is_some() && self.opened == 0 && self.got == 0 && self.need == HEADER && self.body == 0 && self.tag_left == 0 && self.plain_pos == self.plain_end && self.clear_left == 0
    }

    #[inline]
    pub(crate) fn rx_context(&self) -> i64 {
        self.rx.as_ref().map_or(0, |r| r.ctx())
    }

    #[inline]
    pub(crate) fn rx_number(&self) -> u64 {
        self.rx_n
    }

    /// The chain has opened this many bytes of the next offer, in so many records.
    #[inline]
    pub(crate) fn opened_ahead(&mut self, bytes: usize, records: u64) {
        self.opened = bytes;
        self.rx_n += records;
        self.ahead = true;
    }

    /// What is held of a record moves to the start of `dst`, when it leaves room to read: how much.
    pub(crate) fn give_back(&mut self, dst: &mut [u8]) -> usize {
        if self.got == 0 || self.rx.is_none() || self.rec.is_none() || self.opened != 0 || self.body != 0 || self.tag_left != 0 || self.plain_pos != self.plain_end || dst.len() <= self.got << 1 {
            return 0;
        }
        let len = self.got;
        dst[..len].copy_from_slice(&self.rec.as_ref().unwrap()[..len]);
        self.got = 0;
        self.need = HEADER;
        self.release();
        len
    }

    /// The whole-record buffer, back where it came from: the record is taken up, or the connection
    /// is gone.
    pub(crate) fn release(&mut self) {
        if let Some(b) = self.rec.take() {
            if !self.lp.is_null() {
                // SAFETY: on the loop of the connection
                unsafe { (*self.lp).give_record(b) };
            }
        }
    }

    /// The bytes to send: the handshake message, then records of what `inner` gives - all of them
    /// sealed in one call, the chain's when it has one.
    pub fn pull_from<S: ByteSource + ?Sized>(&mut self, inner: &mut S, buf: &mut [u8]) -> io::Result<Pull> {
        if self.failed {
            return Err(NoiseError.into());
        }
        if self.hs_out_pos < self.hs_out_len {
            let n = (self.hs_out_len - self.hs_out_pos).min(buf.len());
            if n == 0 {
                return Ok(Pull::Pending);
            }
            buf[..n].copy_from_slice(&self.hs_buffers.as_ref().unwrap().out[self.hs_out_pos..self.hs_out_pos + n]);
            self.hs_out_pos += n;
            return Ok(Pull::Bytes(n));
        }
        if self.tx.is_none() {
            return Ok(if self.burst { Pull::Eof } else { Pull::Pending }); // the keys come with the server's answer
        }
        self.hs_buffers = None;
        // `run` - where the sealed records not yet sealed begin: a record in the clear ends their run
        let (mut pos, mut run, mut records, mut runs) = (0, 0, 0u64, 0);
        while HEADER + 1 + TAG <= buf.len() - pos {
            let room = RECORD.min(buf.len() - pos - HEADER - TAG);
            match inner.pull(&mut buf[pos + HEADER..pos + HEADER + room])? {
                Pull::Bytes(n) if inner.clear() => {
                    self.seal(buf, run, pos, records)?;
                    records = 0;
                    buf[pos] = n as u8;
                    buf[pos + 1] = ((n | CLEAR) >> 8) as u8;
                    pos += HEADER + n;
                    run = pos;
                    runs += 1;
                    if CHAIN_RUNS <= runs && !self.chain.is_null() {
                        break;
                    }
                }
                Pull::Bytes(n) => {
                    buf[pos] = n as u8;
                    buf[pos + 1] = (n >> 8) as u8;
                    pos += HEADER + n + TAG;
                    records += 1;
                }
                // Nothing now, or the source is dry.
                Pull::Pending if pos == 0 => return Ok(Pull::Pending),
                Pull::Eof if pos == 0 => return Ok(Pull::Eof),
                _ => break,
            }
        }
        if pos == 0 {
            // The buffer cannot hold even the smallest record (HEADER + 1 payload + TAG): report
            // "nothing now" rather than a 0-byte read, which is not a valid `Pull`.
            return Ok(Pull::Pending);
        }
        self.seal(buf, run, pos, records)?;
        Ok(Pull::Bytes(pos))
    }

    /// The sealed records of `buf[from..to]`, `records` of them, sealed in one call - the chain's
    /// when it has one.
    #[inline]
    fn seal(&mut self, buf: &mut [u8], from: usize, to: usize, records: u64) -> io::Result<()> {
        if from == to {
            return Ok(());
        }
        let tx = self.tx.as_ref().unwrap();
        if self.chain.is_null() {
            let n = tx.seal_records(self.tx_n, buf[from..].as_mut_ptr(), to - from);
            if n < 0 {
                return Err(io::Error::other("seal"));
            }
            self.tx_n = n as u64;
        } else {
            // SAFETY: the memory of the chain, on its loop
            unsafe { crate::lib::chain::op(self.chain, crate::lib::chain::OP_NOISE, buf[from..].as_mut_ptr() as i64, to - from, tx.ctx(), self.tx_n as i64) };
            self.tx_n += records;
        }
        Ok(())
    }

    /// The bytes received: the handshake message, then records, opened and handed on to `inner`.
    /// Returns how many were taken; a broken stream is an `InvalidData` error. Every byte goes
    /// through the record buffer: [`push_slice`](Self::push_slice) opens whole records in place.
    pub fn push_to<D: ByteSink + ?Sized>(&mut self, inner: &mut D, bytes: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(NoiseError.into());
        }
        let mut taken = usize::from(self.skip && !bytes.is_empty());
        if taken != 0 {
            self.skip = false;
        }
        loop {
            if !self.flush(inner)? {
                return Ok(self.keep_tail(bytes.len(), taken)); // the destination refuses now: offer again later
            }
            if taken == bytes.len() {
                return Ok(taken);
            }
            if 0 < self.clear_left {
                let n = self.hand_clear(inner, &bytes[taken..])?;
                taken += n;
                if 0 < self.clear_left {
                    return Ok(taken); // refused, or all there is: the rest stays with the transport
                }
                continue;
            }
            taken += self.gather(&bytes[taken..])?;
        }
    }

    /// What of the record in the clear coming in `bytes` hold, straight to the sink - it takes
    /// apart what may be there: how many it took.
    #[inline]
    fn hand_clear<D: ByteSink + ?Sized>(&mut self, inner: &mut D, bytes: &[u8]) -> io::Result<usize> {
        let k = self.clear_left.min(bytes.len());
        let n = match inner.write_clear(&bytes[..k]) {
            Ok(n) => n,
            Err(_) => return self.fail(),
        };
        self.clear_left -= n;
        Ok(n)
    }

    /// A record in the clear of the length `len`, checked, starts: its bytes come next.
    #[inline]
    fn clear_header(&mut self, len: usize) -> io::Result<()> {
        let len = len & !CLEAR;
        if len == 0 || RECORD < len {
            return self.fail();
        }
        self.clear_left = len;
        Ok(())
    }

    /// As [`push_to`](Self::push_to), the bytes writable: the whole records in them are opened
    /// where they lie, in one call - or lie opened already, by the chain - and their plaintext
    /// handed on from there: what the destination refuses stays there, for the transport to offer
    /// again. Only a record cut by the end of `bytes` goes through the record buffer.
    pub fn push_slice<D: ByteSink + ?Sized>(&mut self, inner: &mut D, bytes: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(NoiseError.into());
        }
        let mut taken = usize::from(self.skip && !bytes.is_empty());
        if taken != 0 {
            self.skip = false;
        }
        loop {
            if !self.flush(inner)? {
                return Ok(self.keep_tail(bytes.len(), taken));
            }
            if 0 < self.clear_left {
                taken += self.hand_clear(inner, &bytes[taken..])?;
                if 0 < self.clear_left {
                    return Ok(taken); // refused, or all there is: the rest stays with the transport
                }
            }
            if !self.ahead && self.opened == 0 && self.got == 0 && self.need == HEADER && self.rx.is_some() && HEADER < bytes.len() - taken {
                let b = &mut bytes[taken..];
                let len = b[0] as usize | (b[1] as usize) << 8;
                if len & CLEAR != 0 {
                    self.clear_header(len)?;
                    taken += HEADER;
                    continue;
                }
                // the sealed records up to the first in the clear: the library opens them in one call
                let (mut have, mut at) = (b.len(), HEADER + len + TAG);
                while at + HEADER <= have {
                    let len = b[at] as usize | (b[at + 1] as usize) << 8;
                    if len & CLEAR != 0 {
                        have = at;
                        break;
                    }
                    at += HEADER + len + TAG;
                }
                let r = self.rx.as_ref().unwrap().open_records(self.rx_n, b.as_mut_ptr(), have, RECORD);
                if r < 0 {
                    return self.fail();
                }
                self.rx_n += r as u32 as u64;
                self.opened = (r >> 32) as usize;
            }
            while 0 < self.opened {
                // Every step only as far as this offer goes: an offer may stop short of what was
                // opened from a longer one.
                let rest = bytes.len() - taken;
                if 0 < self.tag_left {
                    let k = self.tag_left.min(rest);
                    if k == 0 {
                        return Ok(taken);
                    }
                    taken += k;
                    self.tag_left -= k;
                    self.opened -= k;
                    continue;
                }
                if self.body == 0 {
                    if rest < HEADER {
                        return Ok(taken);
                    }
                    self.body = bytes[taken] as usize | (bytes[taken + 1] as usize) << 8;
                    taken += HEADER;
                    self.opened -= HEADER;
                }
                let offered = self.body.min(bytes.len() - taken);
                if offered == 0 {
                    return Ok(taken);
                }
                let n = inner.write(&bytes[taken..taken + offered])?;
                taken += n;
                self.body -= n;
                self.opened -= n;
                if n < offered {
                    return Ok(taken);
                }
                if self.body == 0 {
                    self.tag_left = TAG;
                }
            }
            if taken == bytes.len() {
                return Ok(taken);
            }
            if self.ahead {
                self.ahead = false; // what the chain could not open: a record in the clear, a part of a record, or one that fails there
                continue;
            }
            taken += self.gather(&bytes[taken..])?;
        }
    }

    /// The plaintext of a record put together here is refused, and the transport's buffer has no
    /// more: the transport wakes a parked receive only for a tail it holds, so the last byte stays
    /// with it, counted as taken when it comes again.
    #[inline]
    fn keep_tail(&mut self, len: usize, taken: usize) -> usize {
        if taken == len && 0 < taken && !self.skip {
            self.skip = true;
            return taken - 1;
        }
        taken
    }

    /// The plaintext of an opened record the destination has not taken all of, offered again.
    /// `false` - it refuses now.
    #[inline]
    fn flush<D: ByteSink + ?Sized>(&mut self, inner: &mut D) -> io::Result<bool> {
        while self.plain_pos < self.plain_end {
            let n = inner.write(&self.rec.as_ref().unwrap()[self.plain_pos..self.plain_end])?;
            if n == 0 {
                return Ok(false);
            }
            self.plain_pos += n;
            if self.plain_pos == self.plain_end {
                self.release();
            }
        }
        Ok(true)
    }

    /// Copies what the current message or record still needs out of `bytes` and acts once it is
    /// whole: the handshake, the length of a record, the record opened. Returns how many bytes it
    /// took.
    fn gather(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let k = (self.need - self.got).min(bytes.len());
        if self.rx.is_none() {
            self.hs_buffers.as_mut().unwrap().came[self.got..self.got + k].copy_from_slice(&bytes[..k]);
        } else {
            if self.rec.is_none() {
                // SAFETY: on the loop of the connection
                self.rec = Some(if self.lp.is_null() {
                    vec![0u8; HEADER + RECORD + TAG].into_boxed_slice()
                } else {
                    unsafe { (*self.lp).take_record() }
                });
            }
            self.rec.as_mut().unwrap()[self.got..self.got + k].copy_from_slice(&bytes[..k]);
        }
        self.got += k;
        if self.got < self.need {
            return Ok(k);
        }
        if self.rx.is_none() {
            // The handshake: [length][message].
            if self.need == 1 {
                self.need = 1 + self.hs_buffers.as_ref().unwrap().came[0] as usize;
                if self.need - 1 < KEY || HS_BUFFER < self.need {
                    return self.fail();
                }
                return Ok(k);
            }
            self.handshake()?;
            self.got = 0;
            self.need = if self.rx.is_some() { HEADER } else { 1 }; // a pattern of three messages goes on
            return Ok(k);
        }
        let rec = self.rec.as_mut().unwrap();
        if self.need == HEADER {
            let len = rec[0] as usize | (rec[1] as usize) << 8;
            if len & CLEAR != 0 {
                // a record in the clear: its bytes go on from the offer as they come
                self.got = 0;
                self.release();
                self.clear_header(len)?;
                return Ok(k);
            }
            if len == 0 || RECORD < len {
                return self.fail();
            }
            self.need = HEADER + len + TAG;
            return Ok(k);
        }
        let plain = self.need - HEADER - TAG;
        let at = rec.as_mut_ptr();
        // SAFETY: the record lies whole in its buffer
        if !self.rx.as_ref().unwrap().open_at(self.rx_n, at, HEADER, unsafe { at.add(HEADER) }, plain) {
            return self.fail();
        }
        self.rx_n += 1;
        self.plain_pos = HEADER;
        self.plain_end = HEADER + plain;
        self.got = 0;
        self.need = HEADER;
        Ok(k)
    }

    // A handshake message in came[1..need): the server reads the first and answers, the client reads
    // the answer.
    fn handshake(&mut self) -> io::Result<()> {
        let need = self.need;
        let buffers = self.hs_buffers.as_mut().ok_or(NoiseError)?;
        let h = self.hs.as_mut().ok_or(NoiseError)?;
        if h.read_message(&buffers.came[1..need], &mut []).is_err() {
            return self.fail();
        }
        if !h.done() {
            self.hs_out_len = 1 + h.write_message(&mut buffers.out[1..], &[]).map_err(io::Error::from)?;
            buffers.out[0] = (self.hs_out_len - 1) as u8;
            self.hs_out_pos = 0;
            if !h.done() {
                if let Some(wake) = self.wake.as_mut() {
                    wake(); // the answer goes; the last message is still to come
                }
                return Ok(());
            }
        }
        let key = h.peer_key();
        if key != [0u8; KEY] {
            // a side that does not learn the key of the other ( the server of NK ) has nothing to ask about
            self.peer_key = Some(key);
            if self.on_peer.as_ref().is_some_and(|on_peer| !on_peer(&key)) {
                return self.fail();
            }
            let _ = self.peer.set(key); // before any record of the peer reaches the protocol
        }
        let (send, receive) = h.split();
        h.wipe();
        self.hs = None;
        self.rx = Some(receive);
        self.tx = Some(send); // last: a transmit that sees the keys sees the answer too
        if let Some(wake) = self.wake.as_mut() {
            wake();
        }
        Ok(())
    }

    fn fail<T>(&mut self) -> io::Result<T> {
        self.forged += 1;
        self.failed = true;
        Err(NoiseError.into())
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    struct Data(Vec<u8>, usize);

    impl ByteSource for Data {
        fn pull(&mut self, buf: &mut [u8]) -> io::Result<Pull> {
            let n = (self.0.len() - self.1).min(buf.len());
            if n == 0 {
                return Ok(Pull::Pending);
            }
            buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
            self.1 += n;
            Ok(Pull::Bytes(n))
        }
    }

    /// Takes at most `room` bytes a call, refuses every third call.
    struct Slow(Vec<u8>, usize, usize);

    impl ByteSink for Slow {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.2 += 1;
            if self.2 % 3 == 0 {
                return Ok(0);
            }
            let n = bytes.len().min(self.1);
            self.0.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
    }

    /// The wire of `data` from a client to a server: the handshake both ways, then the records.
    fn wire(data: &[u8], room: usize) -> (NoiseStream, Vec<u8>) {
        let (secret, public) = super::super::x25519::keypair();
        let mut client = NoiseStream::client(&public, None);
        let mut server = NoiseStream::server(&secret, &public, None);
        let mut buf = vec![0u8; room];
        let mut nothing = Data(Vec::new(), 0);
        let Pull::Bytes(n) = client.pull_from(&mut nothing, &mut buf).unwrap() else {
            panic!("no message 1")
        };
        assert_eq!(server.push_to(&mut Slow(Vec::new(), usize::MAX, 1), &buf[..n]).unwrap(), n);
        let Pull::Bytes(n) = server.pull_from(&mut nothing, &mut buf).unwrap() else {
            panic!("no message 2")
        };
        assert_eq!(client.push_to(&mut Slow(Vec::new(), usize::MAX, 1), &buf[..n]).unwrap(), n);
        assert!(client.ready() && server.ready());
        let mut src = Data(data.to_vec(), 0);
        let mut out = Vec::new();
        while let Pull::Bytes(n) = client.pull_from(&mut src, &mut buf).unwrap() {
            out.extend_from_slice(&buf[..n]);
        }
        (server, out)
    }

    #[test]
    fn push_slice_opens_in_place_as_push_to() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2654435761u32) >> 13) as u8).collect();
        for room in [128usize, 1000, 1 << 16] {
            for chunk in [1usize, 17, 128, 4096, 40_000] {
                for sink_room in [7usize, 1 << 20] {
                    for in_place in [false, true] {
                        let (mut server, mut w) = wire(&data, room);
                        let mut sink = Slow(Vec::new(), sink_room, 0);
                        let mut at = 0;
                        let mut stalls = 0;
                        while at < w.len() {
                            let end = (at + chunk).min(w.len());
                            let before = sink.0.len();
                            let n = if in_place {
                                server.push_slice(&mut sink, &mut w[at..end]).unwrap()
                            } else {
                                server.push_to(&mut sink, &w[at..end]).unwrap()
                            };
                            at += n;
                            stalls = if n == 0 && sink.0.len() == before { stalls + 1 } else { 0 };
                            assert!(stalls < 10, "stuck at {at}");
                        }
                        while server.plain_pos < server.plain_end || server.skip {
                            let mut last = [0u8; 1];
                            server.push_slice(&mut sink, if server.skip { &mut last } else { &mut [] }).unwrap();
                        }
                        assert!(sink.0 == data, "room {room} chunk {chunk} sink {sink_room} in place {in_place}");
                    }
                }
            }
        }
    }

    #[test]
    fn push_slice_breaks_on_a_forged_record() {
        let (mut server, mut w) = wire(&[5u8; 3000], 1 << 16);
        let last = w.len() - 1;
        w[last] ^= 1;
        assert!(server.push_slice(&mut Slow(Vec::new(), usize::MAX, 1), &mut w).is_err());
        assert!(server.failed());
    }

    /// Pieces of plaintext, each sealed or in the clear: a pull gives one kind, as a transmitter
    /// does.
    struct Mixed(Vec<(Vec<u8>, bool)>, usize, usize, bool);

    impl ByteSource for Mixed {
        fn pull(&mut self, buf: &mut [u8]) -> io::Result<Pull> {
            let Some((p, clear)) = self.0.get(self.1) else { return Ok(Pull::Pending) };
            let n = (p.len() - self.2).min(buf.len());
            buf[..n].copy_from_slice(&p[self.2..self.2 + n]);
            self.3 = *clear;
            self.2 += n;
            if self.2 == p.len() {
                self.1 += 1;
                self.2 = 0;
            }
            Ok(Pull::Bytes(n))
        }

        fn clear(&self) -> bool {
            self.3
        }
    }

    /// What came sealed and what came in the clear, apart; refuses every third call of each.
    #[derive(Default)]
    struct Apart {
        sealed: Vec<u8>,
        clear: Vec<u8>,
        calls: usize,
        clear_ok: bool,
    }

    impl ByteSink for Apart {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls % 3 == 0 {
                return Ok(0);
            }
            self.sealed.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn write_clear(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.clear_ok {
                return Err(NoiseError.into());
            }
            self.calls += 1;
            if self.calls % 3 == 0 {
                return Ok(0);
            }
            let n = bytes.len().min(7000);
            self.clear.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
    }

    /// Both ends after the handshake.
    fn pair() -> (NoiseStream, NoiseStream) {
        let (secret, public) = super::super::x25519::keypair();
        let mut client = NoiseStream::client(&public, None);
        let mut server = NoiseStream::server(&secret, &public, None);
        let mut buf = vec![0u8; 256];
        let mut nothing = Data(Vec::new(), 0);
        let Pull::Bytes(n) = client.pull_from(&mut nothing, &mut buf).unwrap() else {
            panic!("no message 1")
        };
        server.push_to(&mut Slow(Vec::new(), usize::MAX, 1), &buf[..n]).unwrap();
        let Pull::Bytes(n) = server.pull_from(&mut nothing, &mut buf).unwrap() else {
            panic!("no message 2")
        };
        client.push_to(&mut Slow(Vec::new(), usize::MAX, 1), &buf[..n]).unwrap();
        (client, server)
    }

    fn pieces() -> Vec<(Vec<u8>, bool)> {
        let mut v = Vec::new();
        for i in 0..40u32 {
            let len = [1usize, 5, 300, 17_000, 40_000][(i % 5) as usize];
            let fill = if i % 3 == 0 { b'C' } else { b's' }; // the text in the clear is seen on the wire, the sealed one is not
            v.push(((0..len).map(|j| if j % 64 == 0 { b'#' } else { fill }).collect(), i % 3 == 0));
        }
        v
    }

    /// Records in the clear go between the sealed ones as they are, with no tag: the sealed ones
    /// count the nonce by themselves - every one opens, in any cut, on both ways in.
    #[test]
    fn records_in_the_clear_between_sealed_ones() {
        let all = pieces();
        let (sealed, clear): (Vec<u8>, Vec<u8>) = (
            all.iter().filter(|p| !p.1).flat_map(|p| p.0.clone()).collect(),
            all.iter().filter(|p| p.1).flat_map(|p| p.0.clone()).collect(),
        );
        for room in [100usize, 4096, 1 << 16] {
            let (mut client, _) = pair();
            let mut src = Mixed(all.clone(), 0, 0, false);
            let mut w = Vec::new();
            let mut buf = vec![0u8; room];
            while let Pull::Bytes(n) = client.pull_from(&mut src, &mut buf).unwrap() {
                w.extend_from_slice(&buf[..n]);
            }
            // the wire: the sealed records count the nonce, the records in the clear do not
            let (mut at, mut tagged, mut plain) = (0, 0u64, 0usize);
            while at < w.len() {
                let len = w[at] as usize | (w[at + 1] as usize) << 8;
                if len & CLEAR != 0 {
                    plain += len & !CLEAR;
                    at += HEADER + (len & !CLEAR);
                } else {
                    tagged += 1;
                    at += HEADER + len + TAG;
                }
            }
            assert_eq!(client.tx_n, tagged, "the nonce counts the sealed records only");
            assert_eq!(plain, clear.len(), "the bytes in the clear go as they are");
            assert!(!w.windows(16).any(|x| x.iter().all(|&c| c == b's')), "nothing of the sealed text on the wire");
            assert!(w.windows(16).any(|x| x.iter().all(|&c| c == b'C')), "the text in the clear is on the wire");
            for chunk in [1usize, 2, 3, 19, 4096, 70_000] {
                for in_place in [false, true] {
                    let (mut client, mut server) = pair();
                    let mut src = Mixed(all.clone(), 0, 0, false);
                    let mut w = Vec::new();
                    while let Pull::Bytes(n) = client.pull_from(&mut src, &mut buf).unwrap() {
                        w.extend_from_slice(&buf[..n]);
                    }
                    let mut sink = Apart { clear_ok: true, ..Apart::default() };
                    let (mut at, mut stalls) = (0, 0);
                    while at < w.len() || server.plain_pos < server.plain_end || server.skip {
                        let end = (at + chunk).min(w.len());
                        let before = sink.sealed.len() + sink.clear.len();
                        let mut last = [0u8; 1];
                        let n = if at == w.len() {
                            server.push_slice(&mut sink, if server.skip { &mut last } else { &mut [] }).unwrap()
                        } else if in_place {
                            server.push_slice(&mut sink, &mut w[at..end]).unwrap()
                        } else {
                            server.push_to(&mut sink, &w[at..end]).unwrap()
                        };
                        at += n;
                        stalls = if n == 0 && sink.sealed.len() + sink.clear.len() == before { stalls + 1 } else { 0 };
                        assert!(stalls < 10, "stuck at {at}");
                    }
                    assert!(sink.sealed == sealed && sink.clear == clear, "room {room} chunk {chunk} in place {in_place}");
                    assert_eq!(server.rx_n, client.tx_n);
                }
            }
        }
    }

    /// A record in the clear where the sink takes none - a pack of the connection put in the clear
    /// on the line - breaks the stream as a forged record does.
    #[test]
    fn a_record_in_the_clear_the_sink_refuses_breaks_the_stream() {
        let (mut client, mut server) = pair();
        let mut src = Mixed(vec![(b"login".to_vec(), false)], 0, 0, false);
        let mut buf = vec![0u8; 4096];
        let Pull::Bytes(n) = client.pull_from(&mut src, &mut buf).unwrap() else { panic!("nothing") };
        let mut w = vec![5u8, (CLEAR >> 8) as u8, b'l', b'o', b'g', b'i', b'n'];
        w.extend_from_slice(&buf[..n]);
        let mut sink = Apart::default();
        assert!(server.push_slice(&mut sink, &mut w).is_err());
        assert!(server.failed() && server.forged == 1 && sink.sealed.is_empty());
        // read-only, cut: the same
        let (_, mut server) = pair();
        assert!(server.push_to(&mut Apart::default(), &w[..1]).is_ok());
        assert!(server.push_to(&mut Apart::default(), &w[1..]).is_err());
        // a record in the clear of no length, or longer than a record may be
        let (_, mut server) = pair();
        assert!(server.push_slice(&mut Apart { clear_ok: true, ..Apart::default() }, &mut [0, (CLEAR >> 8) as u8, 1, 2]).is_err());
    }

    /// A sealed record with the bit of the clear flipped on the line does not open as one in the
    /// clear: its bytes are not a frame, and the next sealed record fails its nonce.
    #[test]
    fn a_sealed_record_flipped_into_the_clear_does_not_pass() {
        let (mut client, mut server) = pair();
        let mut src = Mixed(vec![(vec![b's'; 100], false), (vec![b't'; 50], false)], 0, 0, false);
        let mut buf = vec![0u8; 100 + 2 + TAG];
        let mut w = Vec::new();
        while let Pull::Bytes(n) = client.pull_from(&mut src, &mut buf).unwrap() {
            w.extend_from_slice(&buf[..n]);
        }
        w[1] |= (CLEAR >> 8) as u8; // the first record "in the clear": 100 bytes of ciphertext, then its tag read as a header
        let mut sink = Apart { clear_ok: true, ..Apart::default() };
        let r = server.push_slice(&mut sink, &mut w);
        assert!(r.is_err() || sink.sealed.is_empty(), "nothing sealed is taken past the forged record");
        assert!(!sink.clear.windows(16).any(|x| x.iter().all(|&c| c == b's')), "the ciphertext stays ciphertext");
    }
}
