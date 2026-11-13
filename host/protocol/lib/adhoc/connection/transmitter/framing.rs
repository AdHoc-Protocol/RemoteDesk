// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! COBS-style packet framing with a `0xFF` delimiter and CRC-32C.
//!
//! Each packet starts with the delimiter, followed by encoded payload and checksum blocks. Short
//! blocks replace a nearby payload delimiter with a length code. Long blocks contain up to [`LONG`]
//! bytes without an implied delimiter. See [`crate::lib::adhoc::connection::receiver::framing`] for
//! the wire layout.
//!
//! [`State::read`] uses one buffer: serialization writes into its tail and framing writes blocks
//! into its head, allowing output before the entire packet is available.

use crate::lib::adhoc::helpers::{crc32c, CRC_LEN_BYTES};
use crate::lib::adhoc::connection::transmitter::{TxHost, TxState};

use crate::lib::adhoc::connection::receiver::framing::{ff_free, ff_in, LONG};

/// The room the encoded bytes of a pack may take beyond its raw ones in a buffer, but for its long
/// blocks: its `0xFF`, the head of the long block the end of the buffer ends, the code and the CRC
/// of its last block, the word a short block is copied with.
const RESERVE: usize = 1 + 3 + 1 + CRC_LEN_BYTES + 8;

/// The transmitting framing.
#[derive(Default)]
pub struct State {
    /// The CRC-32C of the raw bytes of the pack so far.
    crc: u32,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes of the packs to send, framed, into `dst`. Returns the bytes written; 0 — a pack
    /// waits for a larger buffer; -1 — nothing to send.
    pub fn read<H: TxHost>(&mut self, dst: &mut [u8], tx: &mut TxState, host: &mut H) -> isize {
        let limit = dst.len();
        let mut enc = 0;
        loop {
            // the raw bytes go behind room for their encoding: RESERVE and an eighth of the room -
            // the heads of long blocks, a 3 in 254 at most, and a copy of the raw bytes down to
            // their place far from them
            let raw = enc + RESERVE + (limit - enc) / 8;
            if limit <= raw {
                break;
            }
            let (len, started, ended) = tx.read_one(&mut dst[raw..], host);
            if started {
                dst[enc] = 0xFF;
                enc += 1;
            }
            let len = if len < 0 {
                if enc == 0 {
                    return -1;
                }
                break;
            } else {
                len as usize
            };
            let end = raw + len;
            self.crc = crc32c(self.crc, &dst[raw..end]); // before they are encoded over
            let (r, e) = encode(dst, raw, end, enc, ended);
            enc = e;
            if ended {
                // the last bytes of the pack, fewer than 250 after its last 0xFF, then its CRC in
                // short blocks: the last one ends the pack
                let code = enc;
                enc += 1;
                let j = end - r;
                dst.copy_within(r..end, enc); // enc <= r
                enc += j;
                let mut code = code;
                for x in self.crc.to_be_bytes() {
                    if x == 0xFF {
                        dst[code] = (enc - code - 1) as u8; // ends its block: the code implies it
                        code = enc;
                    } else {
                        dst[enc] = x;
                    }
                    enc += 1;
                }
                dst[code] = (enc - code - 1) as u8;
                self.crc = 0;
            } else if len == 0 {
                break; // the pack waits for room
            }
        }
        enc as isize
    }
}

/// The raw bytes `[r, end)` of `dst` encoded into `enc` (`enc <= r`): a short block up to a `0xFF`
/// of the data closer than 254 bytes, else a long block; the bytes at the end go at once, as a long
/// block. When `last`, the bytes of the pack after its last `0xFF`, fewer than 250, are left: they
/// go with the CRC. Returns where the raw bytes left start and where the encoding goes on.
#[inline]
fn encode(dst: &mut [u8], mut r: usize, end: usize, mut enc: usize, last: bool) -> (usize, usize) {
    assert!(end <= dst.len() && (end <= r || enc + 8 <= r)); // the gap (RESERVE): what the word overrun takes
    let p = dst.as_mut_ptr();
    while r < end {
        if r + 8 <= end {
            // SAFETY: [r, r + 8) is in dst
            let w = unsafe { (p.add(r) as *const u64).read_unaligned() };
            let m = ff_in(u64::from_le(w));
            if m != 0 {
                // a short block in the word: its code and the word - the gap (RESERVE) takes the
                // overrun -, the 0xFF taken out
                let j = (m.trailing_zeros() / 8) as usize;
                // SAFETY: enc + 9 <= r + 8 <= end: the room is in dst, the bytes past the block
                // were read already or are of the gap (enc + 8 <= r)
                unsafe {
                    *p.add(enc) = j as u8;
                    (p.add(enc + 1) as *mut u64).write_unaligned(w);
                }
                enc += 1 + j;
                r += j + 1;
                continue;
            }
        }
        let (j, found) = {
            let span = (end - r).min(LONG);
            let j = ff_free(&dst[r..r + span]);
            if j == span && last && r + j == end && j < 250 {
                break;
            }
            (j, j < span)
        };
        let h = if found && j < 254 {
            dst[enc] = j as u8; // a short block: up to a 0xFF of the data, the code implies it
            1
        } else {
            // a long block: up to a 0xFF, LONG bytes or the end of the bytes here, no 0xFF implied
            dst[enc] = 254;
            dst[enc + 1] = (j % 255) as u8;
            dst[enc + 2] = (j / 255) as u8;
            3
        };
        if j <= 8 && r + 8 <= dst.len() {
            // a short block as a word: the gap (RESERVE) takes the overrun
            let w = u64::from_le_bytes(dst[r..r + 8].try_into().unwrap());
            dst[enc + h..enc + h + 8].copy_from_slice(&w.to_le_bytes());
        } else {
            dst.copy_within(r..r + j, enc + h); // enc + h <= r: the bytes behind the block are not reached
        }
        enc += h + j;
        r += if found && h == 1 { j + 1 } else { j }; // a 0xFF of the data is taken out: the code implies it
    }
    (r, enc)
}

/// The packs a connection `T` queued, framed for the network: `&T` is the transmitter host of the
/// generated code.
pub struct Framing<T>
where
    for<'a> &'a T: TxHost,
{
    conn: std::sync::Arc<T>,
    tx: TxState,
    framing: State,
}

impl<T> Framing<T>
where
    for<'a> &'a T: TxHost,
{
    pub fn new(conn: std::sync::Arc<T>) -> Self {
        Self { conn, tx: TxState::new(), framing: State::new() }
    }

    /// The keys of the session: a ChaCha20 stage of a pack chain takes the newest there.
    pub fn keys(mut self, keys: std::sync::Arc<crate::lib::adhoc::connection::keyring::Keyring>) -> Self {
        self.tx.keys = keys;
        self
    }

    /// The keys of the Envelope stages of the connection ( [`crate::lib::chain::Envelope`] ).
    pub fn envelope(mut self, envelope: std::sync::Arc<crate::lib::chain::Envelope>) -> Self {
        self.tx.envelope = Some(envelope);
        self
    }

    /// The packs come from `conn` from now on: the encoder and the serialization start over; the
    /// keys of the session and of its envelope stay. Returns the connection before.
    pub fn switch_to(&mut self, conn: std::sync::Arc<T>) -> std::sync::Arc<T> {
        let keys = self.tx.keys.clone();
        let envelope = self.tx.envelope.take();
        self.tx = TxState::new();
        self.tx.keys = keys;
        self.tx.envelope = envelope;
        self.framing = State::new();
        std::mem::replace(&mut self.conn, conn)
    }
}

impl<T: Send + Sync> crate::lib::adhoc::BytesSrc for Framing<T>
where
    for<'a> &'a T: TxHost,
{
    /// A pack is framed whole in the call that ends it - its bytes and its CRC: between two calls a
    /// pack is either half out or not started.
    #[inline]
    fn at_boundary(&self) -> bool {
        !self.tx.is_open()
    }

    #[inline]
    fn read(&mut self, dst: &mut [u8]) -> isize {
        let mut host: &T = &self.conn;
        let n = self.framing.read(dst, &mut self.tx, &mut host);
        self.tx.gave(n, host.speed())
    }

    /// The pack in flight is dropped: the frame it began stays unfinished - the `0xFF` of the next
    /// pack ends it, and the receiver drops it on its CRC.
    #[inline]
    fn abandon(&mut self) {
        self.tx.abandon();
        self.framing.crc = 0;
    }
}
