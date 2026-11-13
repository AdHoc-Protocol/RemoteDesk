// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Varint encoding of UTF-16 code units.
//!
//! Each `u16` uses one to three bytes with seven payload bits per byte and a continuation bit on
//! every byte except the last. ASCII units use one byte. The codec accepts `&[u16]` and appends
//! decoded units to `Vec<u16>`, preserving unpaired surrogates that cannot be stored directly in a
//! Rust `String`.

/// Suggested decoder batch size. The codec appends directly to the destination and does not
/// allocate a separate scratch buffer.
pub const VARINT_SCRATCH: usize = 256;

/// ASCII fast-path backoff interval reserved for callers. The codecs in this module do not consult
/// this constant.
pub const VARINT_BACKOFF: i32 = 24;

/// The exact number of bytes needed to varint-encode `src`: one byte per code unit, plus a
/// continuation byte for each `>= 0x80`, plus one more for each `>= 0x4000`.
pub fn varint_bytes(src: &[u16]) -> usize {
    // no branch: the compiler counts a vector of units at a time
    src.len() + src.iter().map(|&ch| (0x80 <= ch) as usize + (0x4000 <= ch) as usize).sum::<usize>()
}

/// The number of characters encoded by the `src` bytes: a character ends on the one byte per
/// sequence whose high bit is CLEAR.
pub fn varint_chars(src: &[u8]) -> usize {
    src.iter().map(|&b| (b < 0x80) as usize).sum() // no branch: a vector of bytes at a time
}

/// Encodes `src` into `dst`, stopping at the first character that does not fit whole.
///
/// Returns a 64-bit word: the high 32 bits hold the index in `src` of the next unprocessed code
/// unit (`src.len()` when all fit), the low 32 bits the number of bytes written to `dst`.
pub fn varint_encode(src: &[u16], dst: &mut [u8]) -> u64 {
    let (mut src_from, mut dst_from) = (0usize, 0usize);
    while src_from < src.len() {
        // eight ASCII units at a time: one byte each, narrowed in one pass
        while src_from + 8 <= src.len() && dst_from + 8 <= dst.len() {
            let chunk: &[u16; 8] = src[src_from..src_from + 8].try_into().unwrap();
            if chunk.iter().fold(0u16, |a, &c| a | c) & 0xFF80 != 0 {
                break;
            }
            let out: &mut [u8; 8] = (&mut dst[dst_from..dst_from + 8]).try_into().unwrap();
            for k in 0..8 {
                out[k] = chunk[k] as u8;
            }
            src_from += 8;
            dst_from += 8;
        }
        if src_from == src.len() {
            break;
        }
        let ch = src[src_from] as usize;
        if ch < 0x80 {
            if dst_from == dst.len() {
                break;
            }
            dst[dst_from] = ch as u8;
            dst_from += 1;
        } else if ch < 0x4000 {
            if dst.len() - dst_from < 2 {
                break;
            }
            dst[dst_from] = (0x80 | ch) as u8;
            dst[dst_from + 1] = (ch >> 7) as u8;
            dst_from += 2;
        } else {
            if dst.len() - dst_from < 3 {
                break;
            }
            dst[dst_from] = (0x80 | ch) as u8;
            dst[dst_from + 1] = (0x80 | (ch >> 7)) as u8;
            dst[dst_from + 2] = (ch >> 14) as u8;
            dst_from += 3;
        }
        src_from += 1;
    }
    ((src_from as u64) << 32) | dst_from as u64
}

/// The shift a unit gets to: three bytes at most, the last of them with 2 bits.
pub(crate) const UNIT_SHIFT_MAX: i32 = 14;

/// The shift of a unit that went past three bytes: its bits are dropped, the byte that ends it
/// makes U+FFFD.
pub(crate) const UNIT_BROKEN: i32 = 21;

/// A unit of more than three bytes, or above U+FFFF: told to the handler of the errors of the
/// receive, mended as U+FFFD - the stream goes on, the count of the characters (the bytes that end
/// one) as it was.
#[cold]
#[inline(never)]
pub(crate) fn broken_unit() -> u16 {
    crate::lib::adhoc::connection::receiver::on_error::rx(crate::lib::adhoc::connection::receiver::on_error::RxOnError::BytesDistortion, &"a UTF-16 unit of more than three varint bytes, or above U+FFFF: mended with U+FFFD");
    0xFFFD
}

/// The unit a varint ends with the byte `b` (below 0x80) at `shift`, `acc` the bits before it.
#[inline(always)]
pub(crate) fn end_unit(acc: u32, b: u8, shift: i32) -> u16 {
    if shift < UNIT_SHIFT_MAX || (shift == UNIT_SHIFT_MAX && b <= 3) {
        (acc | (b as u32) << shift) as u16
    } else {
        broken_unit()
    }
}

/// Decodes varint-encoded bytes, appending to `dst` .
///
/// `ret` carries resume state across calls: the low 16 bits are the partial character, the next 8
/// bits the payload bits already accumulated for it. The return value is the state to pass as `ret`
/// to the next call; `0` means a character boundary. A unit of more than three bytes, or above
/// U+FFFF, is U+FFFD ([`broken_unit`]).
pub fn varint_decode(src: &[u8], ret: i32, dst: &mut Vec<u16>) -> i32 {
    let mut ch = ret & 0xFFFF;
    let mut s = (ret >> 16) & 0xFF;
    if src.is_empty() {
        return ret;
    }
    let mut i = 0;
    while i < src.len() {
        if s == 0 {
            // on a character boundary: a run of ASCII bytes is a run of characters, widened in one
            // pass
            let n = crate::lib::adhoc::ascii::prefix(&src[i..]);
            if 0 < n {
                dst.extend(src[i..i + n].iter().map(|&b| b as u16));
                i += n;
                continue;
            }
        }
        let b = src[i] as i32;
        i += 1;
        if b < 0x80 {
            dst.push(end_unit(ch as u32, b as u8, s));
            s = 0;
            ch = 0;
        } else if s < UNIT_SHIFT_MAX {
            ch |= (b & 0x7F) << s;
            s += 7;
        } else {
            // a fourth byte: past what 16 bits take - no shift past 14, the state keeps its fields
            ch = 0;
            s = UNIT_BROKEN;
        }
    }
    (s << 16) | ch
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_all(src: &[u16]) -> Vec<u8> {
        let mut dst = vec![0u8; varint_bytes(src)];
        let r = varint_encode(src, &mut dst);
        assert_eq!(r >> 32, src.len() as u64, "all code units consumed");
        assert_eq!(r as u32 as usize, dst.len(), "buffer fills precisely");
        dst
    }

    #[test]
    fn widths_at_the_thresholds() {
        assert_eq!(encode_all(&[0x00]), vec![0x00]);
        assert_eq!(encode_all(&[0x7F]), vec![0x7F]);
        assert_eq!(encode_all(&[0x80]), vec![0x80, 0x01]);
        assert_eq!(encode_all(&[0x3FFF]), vec![0xFF, 0x7F]);
        assert_eq!(encode_all(&[0x4000]), vec![0x80, 0x80, 0x01]);
        assert_eq!(encode_all(&[0xFFFF]), vec![0xFF, 0xFF, 0x03]);
    }

    #[test]
    fn counts_are_exact() {
        let src: Vec<u16> = (0..100u32).map(|i| (i * 331) as u16).collect();
        assert_eq!(varint_bytes(&src), encode_all(&src).len());
        assert_eq!(varint_chars(&encode_all(&src)), src.len());
        assert_eq!(varint_bytes(&[]), 0);
        assert_eq!(varint_chars(&[]), 0);
    }

    #[test]
    fn round_trip_mixed_text() {
        let src: Vec<u16> = "héllo, こんにちは — ASCII & ünïcödé & 𝒜?"
            .encode_utf16()
            .collect();
        let bytes = encode_all(&src);
        let mut back = Vec::new();
        let ret = varint_decode(&bytes, 0, &mut back);
        assert_eq!(ret, 0, "ends on a character boundary");
        assert_eq!(back, src);
    }

    #[test]
    fn unpaired_surrogates_survive() {
        // Unpaired surrogates must round-trip as `u16` code units even though they cannot appear
        // directly in a Rust `String`.
        let src = vec![0xD800u16, 0x61, 0xDC00];
        let bytes = encode_all(&src);
        let mut back = Vec::new();
        varint_decode(&bytes, 0, &mut back);
        assert_eq!(back, src);
    }

    #[test]
    fn encode_stops_at_full_dst() {
        let src: Vec<u16> = "abcdefgh".encode_utf16().collect();
        let mut dst = [0u8; 5];
        let r = varint_encode(&src, &mut dst);
        assert_eq!(r >> 32, 5, "5 of 8 characters fit");
        assert_eq!(r as u32, 5);
        assert_eq!(&dst, b"abcde");
        // Resume from the high-word index.
        let mut dst2 = [0u8; 3];
        let r2 = varint_encode(&src[(r >> 32) as usize..], &mut dst2);
        assert_eq!(r2 >> 32, 3);
        assert_eq!(&dst2, b"fgh");
    }

    #[test]
    fn decode_resumes_mid_character() {
        let src = vec![0x4E2Du16]; // 中 — 3 bytes
        let bytes = encode_all(&src);
        assert_eq!(bytes, vec![0xAD, 0x9C, 0x01]);
        let mut dst = Vec::new();
        // Feed one byte at a time: the state must carry the partial.
        let mut ret = 0;
        for &b in &bytes[..2] {
            ret = varint_decode(&[b], ret, &mut dst);
        }
        assert!(dst.is_empty(), "no complete character yet");
        ret = varint_decode(&bytes[2..], ret, &mut dst);
        assert_eq!(ret, 0);
        assert_eq!(dst, src);
    }

    #[test]
    fn decode_empty_src_keeps_state() {
        let ret = varint_decode(&[], (7 << 16) | 0x41, &mut Vec::new());
        assert_eq!(ret, (7 << 16) | 0x41);
    }

    /// A unit of more than three bytes, or above U+FFFF: told, U+FFFD, the state on a character
    /// boundary after its end byte - also when the bytes come one at a time.
    #[test]
    fn a_unit_past_three_bytes_is_mended() {
        use crate::lib::adhoc::connection::receiver::on_error::{heard, RxOnError};
        heard::on();
        let wire = [b'a', 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, b'b', 0xFF, 0xFF, 0x04, b'c'];
        let mut dst = Vec::new();
        assert_eq!(varint_decode(&wire, 0, &mut dst), 0);
        assert_eq!(dst, vec![0x61, 0xFFFD, 0x62, 0xFFFD, 0x63]);
        let mut dst = Vec::new();
        let mut ret = 0;
        for b in wire {
            ret = varint_decode(&[b], ret, &mut dst);
        }
        assert_eq!((ret, dst), (0, vec![0x61, 0xFFFD, 0x62, 0xFFFD, 0x63]));
        assert_eq!(heard::take(), vec![RxOnError::BytesDistortion; 4]);
    }

    /// Long runs of ASCII - the paths a vector at a time - between characters of two and three
    /// bytes: what is encoded in pieces of any size and decoded in pieces of any size is the text.
    #[test]
    fn long_mixed_text_round_trips_in_pieces() {
        let mut x = 0x2545_F491u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..50 {
            let mut text: Vec<u16> = Vec::new();
            while text.len() < 600 {
                let run = (next() % 40) as usize;
                text.extend((0..run).map(|_| (next() % 0x80) as u16));
                text.push(match next() % 3 {
                    0 => 0x80 + (next() % 0x3F80) as u16,
                    1 => 0x4000 + (next() % 0xC000) as u16,
                    _ => (next() % 0x80) as u16,
                });
            }
            let whole = encode_all(&text);
            assert_eq!(varint_chars(&whole), text.len());
            // encoded into pieces of a random room
            let mut pieces = Vec::new();
            let mut at = 0usize;
            while at < text.len() {
                let mut room = vec![0u8; 3 + (next() % 30) as usize];
                let r = varint_encode(&text[at..], &mut room);
                at += (r >> 32) as usize;
                pieces.extend_from_slice(&room[..r as u32 as usize]);
            }
            assert_eq!(pieces, whole);
            // decoded from pieces of a random size
            let mut got = Vec::new();
            let (mut ret, mut at) = (0, 0usize);
            while at < whole.len() {
                let n = (1 + next() % 25) as usize;
                let end = (at + n).min(whole.len());
                ret = varint_decode(&whole[at..end], ret, &mut got);
                at = end;
            }
            assert_eq!((ret, got), (0, text));
        }
    }
}
