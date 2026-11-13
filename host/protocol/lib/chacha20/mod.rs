// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! ChaCha20 streams and ChaCha20-Poly1305 authenticated encryption (RFC 8439).
//!
//! [`Core`] retains a stream position across buffer fills. [`Aead`] authenticates individual
//! messages with associated data. The stages of a chain are [`crate::lib::stages`]. The cipher lies
//! in the native library ([`crate::lib::crypto`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::lib::crypto::{api, Mem};

// Core — the raw keystream engine

/// The sequences a thread takes from the process at once ([`Core::next_seq`]).
const SEQ_BLOCK: u64 = 1 << 16;

/// The ChaCha20 keystream engine (RFC 8439): 256-bit key, 96-bit nonce, 32-bit block counter, 20
/// rounds. Encryption and decryption are the same XOR operation. The keystream position is retained
/// across [`process`](Self::process) calls, so a single keyed stream spans any number of chunks and
/// socket fills. Its fields are the `Stream` of `adhoc_crypto.c`: no call for a new stream.
#[repr(C)]
pub struct Core {
    key: [u8; 32],
    nonce: [u8; 12],
    counter: u32,
    /// Into `ks`; 64 - used up.
    pos: u32,
    ks: [u8; 64],
}

/// `base` with the sequence over its last eight bytes, little-endian.
#[inline]
fn nonce_of_seq(base: &[u8; 12], seq: u64) -> [u8; 12] {
    let mut n = *base;
    let s = seq.to_le_bytes();
    for i in 0..8 {
        n[4 + i] ^= s[i];
    }
    n
}

impl Core {
    pub const KEY: usize = 32;
    pub const NONCE: usize = 12;

    /// (Re)key for a fresh stream. Both ends must pass identical values (and the same `counter`,
    /// normally 0).
    pub fn new(key: &[u8; 32], nonce: &[u8; 12], counter: u32) -> Self {
        Self {
            key: *key,
            nonce: *nonce,
            counter,
            pos: 64,
            ks: [0; 64],
        }
    }

    /// A stream of its own nonce: `nonce XOR (0⁴ ‖ seq)`.
    pub fn with_seq(key: &[u8; 32], nonce: &[u8; 12], seq: u64) -> Self {
        Self::new(key, &nonce_of_seq(nonce, seq), 0)
    }

    /// The block counter (the RFC 8439 test vectors start at 1).
    pub fn set_counter(&mut self, counter: u32) {
        self.counter = counter;
        self.pos = 64;
    }

    /// Right after [`new`](Self::new): the stream's nonce becomes the nonce XORed with the
    /// little-endian sequence in its final eight bytes, from block zero. Both peers must derive the
    /// same nonce.
    pub fn nonce_seq(&mut self, seq: u64) {
        self.nonce = nonce_of_seq(&self.nonce, seq);
        self.set_counter(0);
    }

    /// XOR `src` with the keystream into `dst`. Advances the keystream by the data length.
    pub fn process(&mut self, src: &[u8], dst: &mut [u8]) {
        let dst = &mut dst[..src.len()];
        dst.copy_from_slice(src);
        self.process_in_place(dst);
    }

    /// XOR `data` with the keystream, in place. Advances the keystream by the data length.
    #[inline]
    pub fn process_in_place(&mut self, data: &mut [u8]) {
        // SAFETY: the state and the data are this call's
        unsafe { (api().stream_xor)(self as *mut Self as i64, data.as_mut_ptr() as i64, data.len() as i32) };
    }

    /// Allocates a per-record sequence number.
    ///
    /// The process starts at a randomized 64-bit value. Threads reserve [`SEQ_BLOCK`] numbers at
    /// once, reducing shared atomic updates. Allocation order across threads is unspecified;
    /// records carry their sequence explicitly.
    #[inline]
    pub fn next_seq() -> u64 {
        thread_local! {
            /// The next sequence of this thread and how many of its block are left.
            static BLOCK: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
        }
        BLOCK.with(|b| {
            let (next, left) = b.get();
            if left != 0 {
                b.set((next.wrapping_add(1), left - 1));
                return next;
            }
            let start = Self::take_block();
            b.set((start.wrapping_add(1), SEQ_BLOCK - 1));
            start
        })
    }

    /// The next block of sequences of the process: it starts at a random point of the 64-bit space.
    #[cold]
    fn take_block() -> u64 {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let seed = *SEED.get_or_init(|| {
            let mut b = [0u8; 8];
            if getrandom::fill(&mut b).is_err() {
                b = 0x9E37_79B9_7F4A_7C15u64.to_le_bytes();
            }
            u64::from_le_bytes(b)
        });
        if SEQ.load(Ordering::Relaxed) == 0 {
            let _ = SEQ.compare_exchange(0, seed, Ordering::SeqCst, Ordering::SeqCst);
        }
        SEQ.fetch_add(SEQ_BLOCK, Ordering::SeqCst)
    }
}

// Aead — ChaCha20-Poly1305

/// Unencrypted packet header: four-byte little-endian key identifier followed by the eight-byte
/// little-endian sequence used to derive the nonce.
pub const KEY_HDR: usize = 12;

/// The tag length (`ChaCha20.Aead.TAG`).
pub const TAG: usize = 16;

/// ChaCha20-Poly1305 AEAD (RFC 8439) under one key, a message at a time: [`seal`](Self::seal) /
/// [`open`](Self::open) work in place and take the 64-bit message number the nonce is made of — 32
/// zero bits, then the number little-endian, as the Noise protocol forms it. A number must never be
/// used twice under one key. The context lies in native memory of its own, or in that of the
/// handshake it came from.
pub struct Aead {
    ctx: i64,
    _mem: Arc<Mem>,
}

impl Aead {
    pub fn new(key: &[u8; 32]) -> Self {
        let mem = Arc::new(Mem::new(unsafe { (api().aead_size)() } as usize));
        let mut a = Self { ctx: mem.at(), _mem: mem };
        a.rekey(key);
        a
    }

    /// The transport key a handshake made in its memory: `ctx` lies in `mem`.
    pub(crate) fn inside(mem: Arc<Mem>, ctx: i64) -> Self {
        Self { ctx, _mem: mem }
    }

    /// Replaces the key used for subsequent authenticated messages.
    pub fn rekey(&mut self, key: &[u8; 32]) {
        // SAFETY: the context is of the size the library said
        let ok = unsafe { (api().aead_init)(0, self.ctx as *mut u8, key.as_ptr(), 32) };
        assert!(ok != 0, "the key of an AEAD");
    }

    /// Overwrites the key schedule with volatile zero writes and a compiler fence. Rekey before
    /// further use.
    pub fn wipe(&mut self) {
        // SAFETY: the context is of the size the library said
        unsafe { super::noise::wipe(std::slice::from_raw_parts_mut(self.ctx as *mut u8, (api().aead_size)() as usize)) };
    }

    /// Where the context lies, for a call of the library that opens or seals many.
    #[inline(always)]
    pub(crate) fn ctx(&self) -> i64 {
        self.ctx
    }

    /// Seals the records `[length 2][text][room of the tag]` of the `bytes` at `address`, numbered
    /// from `n`: the next number; negative - it failed.
    #[inline]
    pub(crate) fn seal_records(&self, n: u64, address: *mut u8, bytes: usize) -> i64 {
        // SAFETY: the caller's bytes
        unsafe { (api().seal_records)(self.ctx, n as i64, address as i64, bytes as i32) }
    }

    /// Opens all the whole records among the `have` bytes at `address`: `their bytes << 32 | their
    /// count`; negative - one is forged.
    #[inline]
    pub(crate) fn open_records(&self, n: u64, address: *mut u8, have: usize, max: usize) -> i64 {
        // SAFETY: the caller's bytes
        unsafe { (api().open_records)(self.ctx, n as i64, address as i64, have as i32, max as i32) }
    }

    /// Opens the record text in place, `[ad][text][tag]`: false - forged.
    #[inline]
    pub(crate) fn open_at(&self, n: u64, ad: *const u8, ad_len: usize, text: *mut u8, len: usize) -> bool {
        // SAFETY: the caller's bytes
        unsafe { (api().open)(self.ctx, n as i64, ad as i64, ad_len as i32, text as i64, len as i32) != 0 }
    }

    /// Encrypts `data` in place and writes the tag of it and `ad`.
    #[inline]
    pub fn seal(&mut self, n: u64, ad: &[u8], data: &mut [u8], tag: &mut [u8; TAG]) {
        self.seal_with_nonce(&Self::nonce_of(n), ad, data, tag);
    }

    fn nonce_of(n: u64) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&n.to_le_bytes());
        nonce
    }

    /// As [`seal`](Self::seal), with the whole 12-byte nonce given. The library writes the tag right
    /// behind the data: a tag elsewhere costs a copy.
    pub fn seal_with_nonce(&mut self, nonce: &[u8; 12], ad: &[u8], data: &mut [u8], tag: &mut [u8; TAG]) {
        let f = api().seal_nonce;
        // SAFETY: the data, then the tag's room, are the caller's
        unsafe {
            let ok = if data.is_empty() || data.as_mut_ptr().add(data.len()) == tag.as_mut_ptr() {
                let text = if data.is_empty() { tag.as_mut_ptr() } else { data.as_mut_ptr() };
                f(self.ctx, nonce.as_ptr(), ad.as_ptr(), ad.len() as i32, text, data.len() as i32)
            } else {
                let mut joined = vec![0u8; data.len() + TAG];
                joined[..data.len()].copy_from_slice(data);
                let ok = f(self.ctx, nonce.as_ptr(), ad.as_ptr(), ad.len() as i32, joined.as_mut_ptr(), data.len() as i32);
                data.copy_from_slice(&joined[..data.len()]);
                tag.copy_from_slice(&joined[data.len()..]);
                ok
            };
            assert!(ok != 0, "seal");
        }
    }

    /// Decrypts `data` in place when `tag` is right for it and `ad`; false - it is left unusable.
    #[inline]
    pub fn open(&mut self, n: u64, ad: &[u8], data: &mut [u8], tag: &[u8; TAG]) -> bool {
        self.open_with_nonce(&Self::nonce_of(n), ad, data, tag)
    }

    /// As [`open`](Self::open), with the whole 12-byte nonce given.
    pub fn open_with_nonce(&mut self, nonce: &[u8; 12], ad: &[u8], data: &mut [u8], tag: &[u8; TAG]) -> bool {
        let f = api().open_nonce;
        // SAFETY: the data, then the tag, are the caller's; with no data nothing is written
        unsafe {
            if data.is_empty() || data.as_ptr().add(data.len()) == tag.as_ptr() {
                let text = if data.is_empty() { tag.as_ptr() as *mut u8 } else { data.as_mut_ptr() };
                return f(self.ctx, nonce.as_ptr(), ad.as_ptr(), ad.len() as i32, text, data.len() as i32) != 0;
            }
            let mut joined = vec![0u8; data.len() + TAG];
            joined[..data.len()].copy_from_slice(data);
            joined[data.len()..].copy_from_slice(tag);
            let ok = f(self.ctx, nonce.as_ptr(), ad.as_ptr(), ad.len() as i32, joined.as_mut_ptr(), data.len() as i32) != 0;
            if ok {
                data.copy_from_slice(&joined[..data.len()]);
            }
            ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap()).collect()
    }

    /// RFC 8439 2.8.2: the AEAD; 2.4.2: the stream from counter 1, in pieces of any length.
    #[test]
    fn rfc8439() {
        let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f").try_into().unwrap();
        let nonce: [u8; 12] = hex("070000004041424344454647").try_into().unwrap();
        let ad = hex("50515253c0c1c2c3c4c5c6c7");
        let text = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let mut buf = text.to_vec();
        let mut tag = [0u8; TAG];
        let mut a = Aead::new(&key);
        a.seal_with_nonce(&nonce, &ad, &mut buf, &mut tag);
        assert_eq!(&tag[..], &hex("1ae10b594f09e26a7e902ecbd0600691")[..]);
        assert!(a.open_with_nonce(&nonce, &ad, &mut buf, &tag));
        assert_eq!(&buf[..], &text[..]);
        tag[0] ^= 1;
        assert!(!a.open_with_nonce(&nonce, &ad, &mut buf, &tag));

        let key: [u8; 32] = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f").try_into().unwrap();
        let nonce: [u8; 12] = hex("000000000000004a00000000").try_into().unwrap();
        let mut whole = text.to_vec();
        Core::new(&key, &nonce, 1).process_in_place(&mut whole);
        assert_eq!(&whole[..16], &hex("6e2e359a2568f98041ba0728dd0d6981")[..]);
        let mut pieces = text.to_vec();
        let mut c = Core::new(&key, &nonce, 1);
        for piece in pieces.chunks_mut(7) {
            c.process_in_place(piece);
        }
        assert!(pieces == whole);
    }
}
