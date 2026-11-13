// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! ASCII and short-character fast paths for the protocol text codecs.
//!
//! Text is encoded as a varint per UTF-16 code unit. ASCII bytes already have the required encoding
//! and can be copied in runs. Dispatch uses AVX2 or SSE2 on x86_64, NEON on aarch64, and word-sized
//! operations on other targets.

/// The length of the run of ASCII bytes (below 0x80) `b` starts with: `b.len()` when all of them
/// are.
#[inline]
pub fn prefix(b: &[u8]) -> usize {
    if b.first().is_some_and(|&x| 0x80 <= x) {
        return 0; // text that is not ASCII: no vector for nothing
    }
    if b.len() < 16 {
        return short(b);
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: the processor has AVX2
            return unsafe { avx2(b) };
        }
        // SAFETY: SSE2 is part of x86_64
        return unsafe { sse2(b) };
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of aarch64
        return unsafe { neon(b) };
    }
    #[allow(unreachable_code)]
    swar(b)
}

/// The length of `s` in UTF-16 units, what a string's length is on the wire: the ASCII front found
/// with [`prefix`], the rest counted as the bytes that start a character plus one for every 4-byte
/// character (two units).
#[inline]
pub fn utf16_len(s: &str) -> usize {
    utf16_len_ascii(s).0
}

/// The UTF-16 units of `s` and the length of its ASCII head: the head is found once, the count and
/// the copy both use it.
pub fn utf16_len_ascii(s: &str) -> (usize, usize) {
    let b = s.as_bytes();
    let a = prefix(b);
    let rest = &b[a..];
    if rest.is_empty() {
        return (a, a);
    }
    (a + units(rest), a)
}

/// The UTF-16 units of the UTF-8 `b`: a byte that starts a character is one, one that starts four
/// bytes is two. Counted in 32 lanes of bytes, a vector a step - a lane takes at most 2 a step, so
/// it is emptied every 127 steps.
#[inline]
fn units(b: &[u8]) -> usize {
    #[inline(always)]
    fn of(x: u8) -> u8 {
        ((x as i8) >= -64) as u8 + (0xF0 <= x) as u8 // 0x80..=0xBF go on a character
    }
    let mut total = 0usize;
    let mut blocks = b.chunks_exact(32 * 127);
    for block in &mut blocks {
        let mut lanes = [0u8; 32];
        for c in block.chunks_exact(32) {
            for k in 0..32 {
                lanes[k] += of(c[k]);
            }
        }
        total += lanes.iter().map(|&x| x as usize).sum::<usize>();
    }
    let rest = blocks.remainder();
    let mut lanes = [0u8; 32];
    let mut steps = rest.chunks_exact(32);
    for c in &mut steps {
        for k in 0..32 {
            lanes[k] += of(c[k]);
        }
    }
    total + lanes.iter().map(|&x| x as usize).sum::<usize>() + steps.remainder().iter().map(|&x| of(x) as usize).sum::<usize>()
}

/// The leading run of characters U+0800..=U+FFFF (Indic, Thai, kana, CJK, Hangul …) of the UTF-8
/// `src` as the varints of their units into `dst`: three bytes each in UTF-8; in varints two below
/// U+4000, three from it. Only what both sides have room for. Returns the bytes taken and the bytes
/// written.
#[inline]
pub fn three_to_varints(src: &[u8], dst: &mut [u8]) -> (usize, usize) {
    let steps = src.len().min(dst.len()) / 3;
    let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
    let (mut i, mut o) = (0, 0);
    for _ in 0..steps {
        // SAFETY: `steps` steps at most, each reads the three bytes of `src` at `i` and writes
        // three of `dst` at `o <= i`: `i + 3 <= 3 * steps`, within both
        unsafe {
            let b0 = *s.add(i);
            if b0 & 0xF0 != 0xE0 {
                break; // ASCII, below U+0800, or four bytes: another path
            }
            let cp = (b0 as u32 & 0x0F) << 12 | (*s.add(i + 1) as u32 & 0x3F) << 6 | (*s.add(i + 2) as u32 & 0x3F);
            *d.add(o) = 0x80 | cp as u8;
            i += 3;
            if cp < 0x4000 {
                *d.add(o + 1) = (cp >> 7) as u8;
                o += 2;
            } else {
                *d.add(o + 1) = 0x80 | (cp >> 7) as u8;
                *d.add(o + 2) = (cp >> 14) as u8;
                o += 3;
            }
        }
    }
    (i, o)
}

/// The leading run of varints of units U+0800..=U+FFFF of `src`, at most `max_units`, into UTF-8 in
/// `out`: a varint of two bytes below U+4000, of three from it, each unit three bytes of UTF-8. A
/// unit below U+0800 or a surrogate is left to the other paths. Returns the bytes taken and the
/// units.
#[inline]
pub fn varints_to_three(src: &[u8], max_units: usize, out: &mut Vec<u8>) -> (usize, usize) {
    let most = max_units.min(src.len() / 2);
    out.reserve(most * 3);
    let base = out.len();
    let (s, d) = (src.as_ptr(), out.spare_capacity_mut().as_mut_ptr() as *mut u8);
    let (mut i, mut u) = (0, 0);
    while u < most && i + 3 <= src.len() {
        // SAFETY: the three bytes at `i` are within `src`; a unit takes two bytes at least, so
        // `u < most <= src.len() / 2` and its three bytes of UTF-8 are within the room reserved
        unsafe {
            let (b0, b1) = (*s.add(i), *s.add(i + 1));
            if b0 < 0x80 {
                break;
            }
            let cp = if b1 < 0x80 {
                if b1 < 0x10 {
                    break; // below U+0800: the short path
                }
                i += 2;
                (b0 as u32 & 0x7F) | (b1 as u32) << 7
            } else {
                let b2 = *s.add(i + 2);
                let cp = (b0 as u32 & 0x7F) | (b1 as u32 & 0x7F) << 7 | (b2 as u32) << 14;
                if 2 < b2.wrapping_sub(1) || cp & 0xF800 == 0xD800 {
                    break; // not a unit, or a surrogate
                }
                i += 3;
                cp
            };
            let o = d.add(u * 3);
            *o = 0xE0 | (cp >> 12) as u8;
            *o.add(1) = 0x80 | (cp >> 6) as u8 & 0x3F;
            *o.add(2) = 0x80 | cp as u8 & 0x3F;
            u += 1;
        }
    }
    // SAFETY: the first `u * 3` bytes of the spare room were written above
    unsafe { out.set_len(base + u * 3) };
    (i, u)
}

/// One UTF-16 unit into the UTF-8 of a string being received: a high surrogate waits in `hi` for
/// its low one, the pair is one character; an unpaired surrogate is U+FFFD - as
/// `String::from_utf16_lossy` makes it.
#[inline]
pub fn push_unit(out: &mut Vec<u8>, hi: &mut u16, unit: u16) {
    const FFFD: &[u8] = "\u{FFFD}".as_bytes();
    let mut b = [0u8; 4];
    match unit {
        0xD800..=0xDBFF => {
            if *hi != 0 {
                out.extend_from_slice(FFFD);
            }
            *hi = unit;
        }
        0xDC00..=0xDFFF => {
            if *hi == 0 {
                out.extend_from_slice(FFFD);
                return;
            }
            let cp = 0x10000 + (((*hi - 0xD800) as u32) << 10) + (unit - 0xDC00) as u32;
            *hi = 0;
            // SAFETY: a surrogate pair gives a scalar value in 0x10000..=0x10FFFF
            out.extend_from_slice(unsafe { char::from_u32_unchecked(cp) }.encode_utf8(&mut b).as_bytes());
        }
        _ => {
            if *hi != 0 {
                *hi = 0;
                out.extend_from_slice(FFFD);
            }
            // SAFETY: not a surrogate
            out.extend_from_slice(unsafe { char::from_u32_unchecked(unit as u32) }.encode_utf8(&mut b).as_bytes());
        }
    }
}

/// The end of a string: a high surrogate left waiting is U+FFFD.
#[inline]
pub fn end_units(out: &mut Vec<u8>, hi: &mut u16) {
    if *hi != 0 {
        *hi = 0;
        out.extend_from_slice("\u{FFFD}".as_bytes());
    }
}

/// The leading run of characters below U+0800 (ASCII, Latin, Cyrillic, Greek, Hebrew, Arabic …) of
/// `src` as the varints of their UTF-16 units into `dst`. Below U+0800 a character is one or two
/// bytes in UTF-8 and as many in varints, at the same places: a vector at a time (AVX2 32 bytes,
/// SSE2 or NEON 16, 8 a word elsewhere and in the tail), whatever their mix, each byte turned in
/// place. A character the end of a vector cuts goes into the next one. Only what both sides have
/// room for: up to a vector of `dst` past the result is scratch. Returns the bytes done - the run
/// ends at a wider character.
#[inline]
pub fn short_to_varints(src: &[u8], dst: &mut [u8]) -> usize {
    let lim = src.len().min(dst.len());
    #[allow(unused_mut)]
    let mut i = 0;
    #[cfg(target_arch = "x86_64")]
    if 16 <= lim {
        // SAFETY: SSE2 is part of x86_64, AVX2 is checked; the kernels stay within `lim`
        let (n, stop) = unsafe { if 32 <= lim && std::is_x86_feature_detected!("avx2") { s2v_avx2(src, dst, lim) } else { s2v_sse2(src, dst, lim) } };
        if stop {
            return n;
        }
        i = n;
    }
    #[cfg(target_arch = "aarch64")]
    if 16 <= lim {
        // SAFETY: NEON is part of aarch64; the kernel stays within `lim`
        let (n, stop) = unsafe { s2v_neon(src, dst, lim) };
        if stop {
            return n;
        }
        i = n;
    }
    i + s2v_swar(&src[i..lim], &mut dst[i..lim])
}

/// The leading run of the varints of UTF-16 units below 0x800 in `src`, at most `units` of them,
/// appended to `out` as their UTF-8: one or two bytes in both, at the same places - a vector at a
/// time, as [`short_to_varints`]. Returns the bytes taken and the units they are.
#[inline]
pub fn varints_to_short(src: &[u8], units: usize, out: &mut Vec<u8>) -> (usize, usize) {
    #[allow(unused_mut)]
    let (mut i, mut done) = (0, 0);
    #[cfg(target_arch = "x86_64")]
    if 16 <= src.len() && 16 <= units {
        // SAFETY: SSE2 is part of x86_64, AVX2 is checked; the kernels stay within `src` and write
        // within the room they reserve
        let (n, u, stop) = unsafe { if 32 <= src.len() && 32 <= units && std::is_x86_feature_detected!("avx2") { v2s_avx2(src, units, out) } else { v2s_sse2(src, units, out) } };
        if stop {
            return (n, u);
        }
        (i, done) = (n, u);
    }
    #[cfg(target_arch = "aarch64")]
    if 16 <= src.len() && 16 <= units {
        // SAFETY: NEON is part of aarch64; the kernel stays within `src` and writes within the room
        // it reserves
        let (n, u, stop) = unsafe { v2s_neon(src, units, out) };
        if stop {
            return (n, u);
        }
        (i, done) = (n, u);
    }
    let (n, u) = v2s_swar(&src[i..], units - done, out);
    (i + n, done + u)
}

const HI8: u64 = 0x8080_8080_8080_8080;
const TOP8: u64 = 0x8000_0000_0000_0000;

/// [`short_to_varints`] 8 bytes a word.
#[inline]
fn s2v_swar(src: &[u8], dst: &mut [u8]) -> usize {
    let lim = src.len().min(dst.len());
    let mut i = 0;
    while i + 8 <= lim {
        let w = u64::from_le_bytes(src[i..i + 8].try_into().unwrap());
        let (b7, b6, b5) = (w & HI8, w << 1 & HI8, w << 2 & HI8);
        let leads = b7 & b6 & !b5;
        let lead = leads & (b7 & !b6) >> 8; // 110xxxxx followed by 10yyyyyy
        let cut = leads & TOP8; // its second byte in the next word
        let bad = HI8 & !(!w & HI8 | lead | lead << 8 | cut);
        let l = (lead >> 7) * 0xFF;
        let c = l << 8;
        // 110xxxxx 10yyyyyy -> 1x yyyyyy (the low 7 bits of the unit, more follow), 0000 xxxx (the
        // high 4); ASCII as it is
        let o = w & !(l | c) | l & (HI8 | (w & 0x0101_0101_0101_0101) << 6 | w >> 8 & 0x3F3F_3F3F_3F3F_3F3F) | c & w << 7 & 0x0F0F_0F0F_0F0F_0F0F;
        dst[i..i + 8].copy_from_slice(&o.to_le_bytes());
        if bad != 0 {
            return i + (bad.trailing_zeros() as usize >> 3);
        }
        i += if cut == 0 { 8 } else { 7 };
    }
    i
}

/// [`varints_to_short`] 8 bytes a word.
#[inline]
fn v2s_swar(src: &[u8], units: usize, out: &mut Vec<u8>) -> (usize, usize) {
    const LOW: u64 = 0x0F0F_0F0F_0F0F_0F0F;
    let (mut i, mut done) = (0, 0);
    while i + 8 <= src.len() && 8 <= units - done {
        let w = u64::from_le_bytes(src[i..i + 8].try_into().unwrap());
        // the byte after a varint's first is 1..=0x0F: a unit 0x80..=0x7FF (below 0x80 a unit is
        // one byte: an overlong pair is no pair)
        let small = !((w & 0x7070_7070_7070_7070) + 0x7070_7070_7070_7070 | w) & ((w & LOW) + LOW) << 3 & HI8;
        let first = w & HI8 & small >> 8;
        let cut = w & TOP8; // a varint the next word goes on with
        let bad = w & HI8 & !first & !cut;
        let h = (first >> 7) * 0xFF;
        let s = h << 8;
        // 1yyyyyyy 0000xxxx -> 110xxxxy 10yyyyyy; ASCII as it is
        let o = w & !(h | s) | h & (0xC0C0_C0C0_C0C0_C0C0 | w >> 7 & 0x1E1E_1E1E_1E1E_1E1E | w >> 6 & 0x0101_0101_0101_0101) | s & (HI8 | w << 8 & 0x3F3F_3F3F_3F3F_3F3F);
        let n = if bad != 0 { bad.trailing_zeros() as usize >> 3 } else if cut != 0 { 7 } else { 8 };
        out.extend_from_slice(&o.to_le_bytes()[..n]);
        i += n;
        if n != 0 {
            done += n - (first & u64::MAX >> (64 - (n << 3))).count_ones() as usize;
        }
        if bad != 0 {
            break;
        }
    }
    (i, done)
}

/// [`short_to_varints`] 16 bytes a step, until a wider character (true) or the last whole vector
/// (false).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn s2v_sse2(src: &[u8], dst: &mut [u8], lim: usize) -> (usize, bool) {
    use std::arch::x86_64::*;
    let (sp, dp) = (src.as_ptr(), dst.as_mut_ptr());
    let (e0, c0, x80, x01, x3f, x1e) = (_mm_set1_epi8(0xE0u8 as i8), _mm_set1_epi8(0xC0u8 as i8), _mm_set1_epi8(0x80u8 as i8), _mm_set1_epi8(1), _mm_set1_epi8(0x3F), _mm_set1_epi8(0x1E));
    let mut i = 0;
    while i + 16 <= lim {
        let v = _mm_loadu_si128(sp.add(i) as *const __m128i);
        let (nx, pv) = (_mm_srli_si128::<1>(v), _mm_slli_si128::<1>(v));
        let leads = _mm_cmpeq_epi8(_mm_and_si128(v, e0), c0);
        let lead = _mm_and_si128(leads, _mm_cmpeq_epi8(_mm_and_si128(nx, c0), x80)); // 110xxxxx followed by 10yyyyyy
        let cont = _mm_slli_si128::<1>(lead);
        let ascii = _mm_cmpgt_epi8(v, _mm_set1_epi8(-1));
        // 110xxxxx 10yyyyyy -> 1x yyyyyy, 0000 xxxx; ASCII as it is
        let ol = _mm_or_si128(x80, _mm_or_si128(_mm_slli_epi16::<6>(_mm_and_si128(v, x01)), _mm_and_si128(nx, x3f)));
        let oc = _mm_srli_epi16::<1>(_mm_and_si128(pv, x1e));
        let o = _mm_or_si128(_mm_and_si128(v, ascii), _mm_or_si128(_mm_and_si128(ol, lead), _mm_and_si128(oc, cont)));
        _mm_storeu_si128(dp.add(i) as *mut __m128i, o);
        let cut = _mm_movemask_epi8(leads) as u32 & 0x8000; // its second byte in the next vector
        let bad = !(_mm_movemask_epi8(_mm_or_si128(ascii, _mm_or_si128(lead, cont))) as u32 | cut) & 0xFFFF;
        if bad != 0 {
            return (i + bad.trailing_zeros() as usize, true);
        }
        i += if cut == 0 { 16 } else { 15 };
    }
    (i, false)
}

/// [`short_to_varints`] 32 bytes a step, then [`s2v_sse2`].
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn s2v_avx2(src: &[u8], dst: &mut [u8], lim: usize) -> (usize, bool) {
    use std::arch::x86_64::*;
    let (sp, dp) = (src.as_ptr(), dst.as_mut_ptr());
    let (e0, c0, x80, x01, x3f, x1e) = (_mm256_set1_epi8(0xE0u8 as i8), _mm256_set1_epi8(0xC0u8 as i8), _mm256_set1_epi8(0x80u8 as i8), _mm256_set1_epi8(1), _mm256_set1_epi8(0x3F), _mm256_set1_epi8(0x1E));
    let mut i = 0;
    while i + 32 <= lim {
        let v = _mm256_loadu_si256(sp.add(i) as *const __m256i);
        let (nx, pv) = (next256(v), prev256(v));
        let leads = _mm256_cmpeq_epi8(_mm256_and_si256(v, e0), c0);
        let lead = _mm256_and_si256(leads, _mm256_cmpeq_epi8(_mm256_and_si256(nx, c0), x80));
        let cont = prev256(lead);
        let ascii = _mm256_cmpgt_epi8(v, _mm256_set1_epi8(-1));
        let ol = _mm256_or_si256(x80, _mm256_or_si256(_mm256_slli_epi16::<6>(_mm256_and_si256(v, x01)), _mm256_and_si256(nx, x3f)));
        let oc = _mm256_srli_epi16::<1>(_mm256_and_si256(pv, x1e));
        let o = _mm256_or_si256(_mm256_and_si256(v, ascii), _mm256_or_si256(_mm256_and_si256(ol, lead), _mm256_and_si256(oc, cont)));
        _mm256_storeu_si256(dp.add(i) as *mut __m256i, o);
        let cut = _mm256_movemask_epi8(leads) as u32 & 0x8000_0000;
        let bad = !(_mm256_movemask_epi8(_mm256_or_si256(ascii, _mm256_or_si256(lead, cont))) as u32 | cut);
        if bad != 0 {
            return (i + bad.trailing_zeros() as usize, true);
        }
        i += if cut == 0 { 32 } else { 31 };
    }
    if i + 16 <= lim {
        let (n, stop) = s2v_sse2(&src[i..], &mut dst[i..], lim - i);
        return (i + n, stop);
    }
    (i, false)
}

/// The vector one byte on: byte `k` is byte `k + 1` of `v`, the last 0.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn next256(v: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m256i {
    use std::arch::x86_64::*;
    _mm256_alignr_epi8::<1>(_mm256_permute2x128_si256::<0x81>(v, v), v)
}

/// The vector one byte back: byte `k` is byte `k - 1` of `v`, the first 0.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn prev256(v: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m256i {
    use std::arch::x86_64::*;
    _mm256_alignr_epi8::<15>(v, _mm256_permute2x128_si256::<0x08>(v, v))
}

/// [`varints_to_short`] 16 bytes a step, until a wider unit (true) or the last whole vector
/// (false).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn v2s_sse2(src: &[u8], units: usize, out: &mut Vec<u8>) -> (usize, usize, bool) {
    use std::arch::x86_64::*;
    let sp = src.as_ptr();
    let (zero, f0, c0, x80, x01, x3f) = (_mm_setzero_si128(), _mm_set1_epi8(0xF0u8 as i8), _mm_set1_epi8(0xC0u8 as i8), _mm_set1_epi8(0x80u8 as i8), _mm_set1_epi8(1), _mm_set1_epi8(0x3F));
    let (mut i, mut done) = (0, 0);
    while i + 16 <= src.len() && 16 <= units - done {
        let v = _mm_loadu_si128(sp.add(i) as *const __m128i);
        let (nx, pv) = (_mm_srli_si128::<1>(v), _mm_slli_si128::<1>(v));
        let high = _mm_cmplt_epi8(v, zero);
        // the byte after a varint's first is 1..=0x0F: a unit 0x80..=0x7FF (an overlong pair is no
        // pair)
        let small = _mm_andnot_si128(_mm_cmpeq_epi8(nx, zero), _mm_cmpeq_epi8(_mm_and_si128(nx, f0), zero));
        let first = _mm_and_si128(high, small);
        let second = _mm_slli_si128::<1>(first);
        // 1yyyyyyy 0000xxxx -> 110xxxxy 10yyyyyy; ASCII as it is
        let of = _mm_or_si128(c0, _mm_or_si128(_mm_add_epi8(nx, nx), _mm_and_si128(_mm_srli_epi16::<6>(v), x01)));
        let os = _mm_or_si128(x80, _mm_and_si128(pv, x3f));
        let o = _mm_or_si128(_mm_andnot_si128(_mm_or_si128(first, second), v), _mm_or_si128(_mm_and_si128(of, first), _mm_and_si128(os, second)));
        let (mh, mf) = (_mm_movemask_epi8(high) as u32, _mm_movemask_epi8(first) as u32);
        let cut = mh & 0x8000; // a varint the next vector goes on with
        let bad = mh & !mf & !cut;
        let n = if bad != 0 { bad.trailing_zeros() as usize } else if cut != 0 { 15 } else { 16 };
        out.reserve(16);
        let len = out.len();
        _mm_storeu_si128(out.as_mut_ptr().add(len) as *mut __m128i, o);
        out.set_len(len + n);
        i += n;
        if n != 0 {
            done += n - (mf & u32::MAX >> (32 - n)).count_ones() as usize;
        }
        if bad != 0 {
            return (i, done, true);
        }
    }
    (i, done, false)
}

/// [`varints_to_short`] 32 bytes a step, then [`v2s_sse2`].
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn v2s_avx2(src: &[u8], units: usize, out: &mut Vec<u8>) -> (usize, usize, bool) {
    use std::arch::x86_64::*;
    let sp = src.as_ptr();
    let (zero, f0, c0, x80, x01, x3f) = (_mm256_setzero_si256(), _mm256_set1_epi8(0xF0u8 as i8), _mm256_set1_epi8(0xC0u8 as i8), _mm256_set1_epi8(0x80u8 as i8), _mm256_set1_epi8(1), _mm256_set1_epi8(0x3F));
    let (mut i, mut done) = (0, 0);
    while i + 32 <= src.len() && 32 <= units - done {
        let v = _mm256_loadu_si256(sp.add(i) as *const __m256i);
        let (nx, pv) = (next256(v), prev256(v));
        let high = _mm256_cmpgt_epi8(zero, v);
        let small = _mm256_andnot_si256(_mm256_cmpeq_epi8(nx, zero), _mm256_cmpeq_epi8(_mm256_and_si256(nx, f0), zero));
        let first = _mm256_and_si256(high, small);
        let second = prev256(first);
        let of = _mm256_or_si256(c0, _mm256_or_si256(_mm256_add_epi8(nx, nx), _mm256_and_si256(_mm256_srli_epi16::<6>(v), x01)));
        let os = _mm256_or_si256(x80, _mm256_and_si256(pv, x3f));
        let o = _mm256_or_si256(_mm256_andnot_si256(_mm256_or_si256(first, second), v), _mm256_or_si256(_mm256_and_si256(of, first), _mm256_and_si256(os, second)));
        let (mh, mf) = (_mm256_movemask_epi8(high) as u32, _mm256_movemask_epi8(first) as u32);
        let cut = mh & 0x8000_0000;
        let bad = mh & !mf & !cut;
        let n = if bad != 0 { bad.trailing_zeros() as usize } else if cut != 0 { 31 } else { 32 };
        out.reserve(32);
        let len = out.len();
        _mm256_storeu_si256(out.as_mut_ptr().add(len) as *mut __m256i, o);
        out.set_len(len + n);
        i += n;
        if n != 0 {
            done += n - (mf & u32::MAX >> (32 - n)).count_ones() as usize;
        }
        if bad != 0 {
            return (i, done, true);
        }
    }
    if i + 16 <= src.len() && 16 <= units - done {
        let (n, u, stop) = v2s_sse2(&src[i..], units - done, out);
        return (i + n, done + u, stop);
    }
    (i, done, false)
}

/// A mask vector (bytes 0 or 0xFF) as 4 bits a byte.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn nibbles(m: std::arch::aarch64::uint8x16_t) -> u64 {
    use std::arch::aarch64::*;
    vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(m))))
}

/// [`short_to_varints`] 16 bytes a step, until a wider character (true) or the last whole vector
/// (false).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn s2v_neon(src: &[u8], dst: &mut [u8], lim: usize) -> (usize, bool) {
    use std::arch::aarch64::*;
    let (sp, dp) = (src.as_ptr(), dst.as_mut_ptr());
    let zero = vdupq_n_u8(0);
    let mut i = 0;
    while i + 16 <= lim {
        let v = vld1q_u8(sp.add(i));
        let (nx, pv) = (vextq_u8::<1>(v, zero), vextq_u8::<15>(zero, v));
        let leads = vceqq_u8(vandq_u8(v, vdupq_n_u8(0xE0)), vdupq_n_u8(0xC0));
        let lead = vandq_u8(leads, vceqq_u8(vandq_u8(nx, vdupq_n_u8(0xC0)), vdupq_n_u8(0x80)));
        let cont = vextq_u8::<15>(zero, lead);
        let ascii = vcltq_u8(v, vdupq_n_u8(0x80));
        let ol = vorrq_u8(vdupq_n_u8(0x80), vorrq_u8(vshlq_n_u8::<6>(vandq_u8(v, vdupq_n_u8(1))), vandq_u8(nx, vdupq_n_u8(0x3F))));
        let oc = vshrq_n_u8::<1>(vandq_u8(pv, vdupq_n_u8(0x1E)));
        let o = vorrq_u8(vandq_u8(v, ascii), vorrq_u8(vandq_u8(ol, lead), vandq_u8(oc, cont)));
        vst1q_u8(dp.add(i), o);
        let cut = vgetq_lane_u8::<15>(leads) != 0;
        let mut ok = vorrq_u8(ascii, vorrq_u8(lead, cont));
        if cut {
            ok = vsetq_lane_u8::<15>(0xFF, ok);
        }
        let bad = !nibbles(ok);
        if bad != 0 {
            return (i + (bad.trailing_zeros() as usize >> 2), true);
        }
        i += if cut { 15 } else { 16 };
    }
    (i, false)
}

/// [`varints_to_short`] 16 bytes a step, until a wider unit (true) or the last whole vector
/// (false).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn v2s_neon(src: &[u8], units: usize, out: &mut Vec<u8>) -> (usize, usize, bool) {
    use std::arch::aarch64::*;
    let sp = src.as_ptr();
    let zero = vdupq_n_u8(0);
    let (mut i, mut done) = (0, 0);
    while i + 16 <= src.len() && 16 <= units - done {
        let v = vld1q_u8(sp.add(i));
        let (nx, pv) = (vextq_u8::<1>(v, zero), vextq_u8::<15>(zero, v));
        let high = vcgeq_u8(v, vdupq_n_u8(0x80));
        let small = vandq_u8(vtstq_u8(nx, nx), vceqq_u8(vandq_u8(nx, vdupq_n_u8(0xF0)), zero));
        let first = vandq_u8(high, small);
        let second = vextq_u8::<15>(zero, first);
        let of = vorrq_u8(vdupq_n_u8(0xC0), vorrq_u8(vaddq_u8(nx, nx), vandq_u8(vshrq_n_u8::<6>(v), vdupq_n_u8(1))));
        let os = vorrq_u8(vdupq_n_u8(0x80), vandq_u8(pv, vdupq_n_u8(0x3F)));
        let o = vorrq_u8(vbicq_u8(v, vorrq_u8(first, second)), vorrq_u8(vandq_u8(of, first), vandq_u8(os, second)));
        let (mh, mf) = (nibbles(high), nibbles(first));
        let cut = mh & 0xF000_0000_0000_0000;
        let bad = mh & !mf & !cut;
        let n = if bad != 0 { bad.trailing_zeros() as usize >> 2 } else if cut != 0 { 15 } else { 16 };
        out.reserve(16);
        let len = out.len();
        vst1q_u8(out.as_mut_ptr().add(len), o);
        out.set_len(len + n);
        i += n;
        if n != 0 {
            done += n - ((mf & u64::MAX >> (64 - (n << 2))).count_ones() as usize >> 2);
        }
        if bad != 0 {
            return (i, done, true);
        }
    }
    (i, done, false)
}

/// [`prefix`] of fewer than 16 bytes: two words that overlap, the first from the start, the second
/// to the end - the bytes the second reads again are known ASCII by then, the answer is exact. No
/// loop, no branch a byte.
#[inline(always)]
fn short(b: &[u8]) -> usize {
    let (n, p) = (b.len(), b.as_ptr());
    if 8 <= n {
        // SAFETY: 8 <= n: [0, 8) and [n - 8, n) are in `b`
        let (w0, w1) = unsafe { ((p as *const u64).read_unaligned() & 0x8080_8080_8080_8080, (p.add(n - 8) as *const u64).read_unaligned() & 0x8080_8080_8080_8080) };
        if w0 != 0 {
            return (u64::from_le(w0).trailing_zeros() / 8) as usize;
        }
        return if w1 == 0 { n } else { n - 8 + (u64::from_le(w1).trailing_zeros() / 8) as usize };
    }
    if 4 <= n {
        // SAFETY: 4 <= n: [0, 4) and [n - 4, n) are in `b`
        let (w0, w1) = unsafe { ((p as *const u32).read_unaligned() & 0x8080_8080, (p.add(n - 4) as *const u32).read_unaligned() & 0x8080_8080) };
        if w0 != 0 {
            return (u32::from_le(w0).trailing_zeros() / 8) as usize;
        }
        return if w1 == 0 { n } else { n - 4 + (u32::from_le(w1).trailing_zeros() / 8) as usize };
    }
    scalar(b, 0)
}

#[inline(always)]
fn scalar(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i] < 0x80 {
        i += 1;
    }
    i
}

#[allow(dead_code)]
#[inline]
fn swar(b: &[u8]) -> usize {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut i = 0;
    while i + 8 <= b.len() {
        let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap()) & HIGH;
        if w != 0 {
            return i + (w.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    scalar(b, i)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2(b: &[u8]) -> usize {
    use std::arch::x86_64::*;
    let p = b.as_ptr();
    let n = b.len();
    let mut i = 0;
    // two vectors a step: their bytes ORed, one movemask says whether any of the 64 has its high
    // bit
    while i + 64 <= n {
        let a = _mm256_loadu_si256(p.add(i) as *const __m256i);
        let c = _mm256_loadu_si256(p.add(i + 32) as *const __m256i);
        if _mm256_movemask_epi8(_mm256_or_si256(a, c)) != 0 {
            let m = _mm256_movemask_epi8(a) as u32;
            if m != 0 {
                return i + m.trailing_zeros() as usize;
            }
            return i + 32 + (_mm256_movemask_epi8(c) as u32).trailing_zeros() as usize;
        }
        i += 64;
    }
    while i + 32 <= n {
        let m = _mm256_movemask_epi8(_mm256_loadu_si256(p.add(i) as *const __m256i)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 32;
    }
    while i + 16 <= n {
        let m = _mm_movemask_epi8(_mm_loadu_si128(p.add(i) as *const __m128i)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 16;
    }
    scalar(b, i)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn sse2(b: &[u8]) -> usize {
    use std::arch::x86_64::*;
    let p = b.as_ptr();
    let n = b.len();
    let mut i = 0;
    while i + 32 <= n {
        let a = _mm_loadu_si128(p.add(i) as *const __m128i);
        let c = _mm_loadu_si128(p.add(i + 16) as *const __m128i);
        if _mm_movemask_epi8(_mm_or_si128(a, c)) != 0 {
            let m = _mm_movemask_epi8(a) as u32;
            if m != 0 {
                return i + m.trailing_zeros() as usize;
            }
            return i + 16 + (_mm_movemask_epi8(c) as u32).trailing_zeros() as usize;
        }
        i += 32;
    }
    while i + 16 <= n {
        let m = _mm_movemask_epi8(_mm_loadu_si128(p.add(i) as *const __m128i)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 16;
    }
    scalar(b, i)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon(b: &[u8]) -> usize {
    use std::arch::aarch64::*;
    let p = b.as_ptr();
    let n = b.len();
    let mut i = 0;
    while i + 32 <= n {
        let a = vld1q_u8(p.add(i));
        let c = vld1q_u8(p.add(i + 16));
        if vmaxvq_u8(vorrq_u8(a, c)) & 0x80 != 0 {
            break; // the byte itself: found below, 16 at a time
        }
        i += 32;
    }
    while i + 16 <= n {
        if vmaxvq_u8(vld1q_u8(p.add(i))) & 0x80 != 0 {
            return scalar(b, i);
        }
        i += 16;
    }
    scalar(b, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_len_as_encode_utf16() {
        for s in ["", "a", "abc", "юникод", "😀", "a😀b", "日本語テキスト", "mixed: ascii, кириллица, 😀😀, 漢字", &"x".repeat(1000), &("ж".repeat(77) + "😀")] {
            assert_eq!(utf16_len(s), s.encode_utf16().count(), "{s}");
        }
    }

    #[test]
    fn every_place_of_the_first_high_byte() {
        for len in [0usize, 1, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65, 100, 1000, 4099] {
            let base: Vec<u8> = (0..len).map(|i| (i * 7 % 0x7F) as u8).collect();
            assert_eq!(prefix(&base), len, "len {len}: all ASCII");
            assert_eq!(swar(&base), len);
            for at in (0..len).step_by(1.max(len / 37)) {
                let mut b = base.clone();
                b[at] = 0x80 | (at as u8);
                if at + 1 < len {
                    b[len - 1] = 0xFF; // a later high byte must not be the answer
                }
                assert_eq!(prefix(&b), at, "len {len} at {at}");
                assert_eq!(swar(&b), at, "swar len {len} at {at}");
                // an unaligned start
                if 1 < len && at >= 1 {
                    assert_eq!(prefix(&b[1..]), at - 1, "len {len} at {at} unaligned");
                }
            }
        }
    }

    /// The varints of a string's UTF-16 units, one unit at a time.
    fn varints(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for u in s.encode_utf16() {
            let mut v = u as u32;
            while 0x7F < v {
                out.push(v as u8 | 0x80);
                v >>= 7;
            }
            out.push(v as u8);
        }
        out
    }

    /// The UTF-16 length of a string of every width and mix, across the blocks the count is taken
    /// in, is what encode_utf16 counts.
    #[test]
    fn utf16_len_of_every_mix() {
        let alphabet = ['a', 'ж', '\u{7FF}', '\u{800}', '日', '\u{FFFF}', '😀', '\u{10FFFF}'];
        let mut rnd = 0x2545_F491_4F6C_DD1Du64;
        for len in [0usize, 1, 31, 32, 33, 1000, 4063, 4064, 4065, 9000, 20_000] {
            for weight in [0usize, 2, 7] {
                let s: String = (0..len).map(|_| {
                    rnd ^= rnd << 13;
                    rnd ^= rnd >> 7;
                    rnd ^= rnd << 17;
                    alphabet[rnd as usize % (alphabet.len() - weight.min(alphabet.len() - 1))]
                }).collect();
                assert_eq!(utf16_len(&s), s.encode_utf16().count(), "len {len} weight {weight}");
            }
        }
    }

    /// Every character U+0800..=U+FFFF but the surrogates, in every mix of two-byte and three-byte
    /// varints, goes out and comes back by the three-byte runs as by one unit at a time; a run
    /// stops at what is not of it, and within the room.
    #[test]
    fn three_both_ways_as_one_unit_at_a_time() {
        let all: Vec<char> = (0x800u32..=0xFFFF).filter_map(char::from_u32).collect();
        let mut rnd = 0x9E37_79B9_7F4A_7C15u64;
        let mut cases: Vec<String> = vec![all.iter().collect()];
        for _ in 0..200 {
            cases.push((0..rnd as usize % 40).map(|_| {
                rnd ^= rnd << 13;
                rnd ^= rnd >> 7;
                rnd ^= rnd << 17;
                all[rnd as usize % all.len()]
            }).collect());
        }
        for s in &cases {
            let want = varints(s);
            let mut dst = vec![0u8; s.len() + 3];
            assert_eq!(three_to_varints(s.as_bytes(), &mut dst), (s.len(), want.len()));
            assert_eq!(dst[..want.len()], want[..]);
            let mut out = b"x".to_vec();
            let mut ended = want.clone();
            ended.extend_from_slice(b"abc"); // ASCII after: the run takes every unit, stops there
            assert_eq!(varints_to_three(&ended, usize::MAX, &mut out), (want.len(), s.chars().count()));
            assert_eq!(&out[1..], s.as_bytes());
        }
        // a run stops at what is not of it, on both sides
        let mixed = "日、本a語ж한";
        let mut dst = vec![0u8; mixed.len()];
        assert_eq!(three_to_varints(mixed.as_bytes(), &mut dst), (9, 8), "日、本: three, two, three");
        let v = varints(mixed);
        let mut out = Vec::new();
        assert_eq!(varints_to_three(&v, usize::MAX, &mut out), (8, 3));
        assert_eq!(out, "日、本".as_bytes());
        assert_eq!(varints_to_three(&v, 2, &mut Vec::new()), (5, 2), "no more units than asked");
        assert_eq!(varints_to_three(&varints("жж"), usize::MAX, &mut Vec::new()), (0, 0), "below U+0800: the short path");
        assert_eq!(three_to_varints("ж".as_bytes(), &mut [0u8; 8]), (0, 0));
        assert_eq!(three_to_varints("日".as_bytes(), &mut [0u8; 2]), (0, 0), "no room");
    }

    #[test]
    fn short_both_ways_as_one_unit_at_a_time() {
        let mut rnd = 0x2545_F491_4F6C_DD1Du64;
        let alphabet = ['a', ' ', '.', 'ж', 'Я', 'é', 'ß', 'א', 'ع', '\u{7F}', '\u{80}', '\u{7FF}', '\u{800}', '日', '😀'];
        for len in 0..120 {
            for weight in [0usize, 3, 9] {
                let s: String = (0..len)
                    .map(|_| {
                        rnd ^= rnd << 13;
                        rnd ^= rnd >> 7;
                        rnd ^= rnd << 17;
                        let r = rnd as usize % 16;
                        if r < weight { ['ж', ' '][r & 1] } else { alphabet[r % alphabet.len()] }
                    })
                    .collect();
                let b = s.as_bytes();
                let want = varints(&s);
                for from in (0..b.len()).filter(|&f| s.is_char_boundary(f)) {
                    // the characters below U+0800 that lead from `from`
                    let lead: Vec<char> = s[from..].chars().take_while(|c| (*c as u32) < 0x800).collect();
                    let lead_bytes: usize = lead.iter().map(|c| c.len_utf8()).sum();
                    let rest = b.len() - from;
                    let mut dst = vec![0u8; rest];
                    let n = short_to_varints(&b[from..], &mut dst);
                    assert!(n <= lead_bytes && lead_bytes - n < 8 && s.is_char_boundary(from + n), "{s:?} from {from}: {n} of {lead_bytes}");
                    let at = varints(&s[..from]).len();
                    assert_eq!(dst[..n], want[at..at + n], "{s:?} from {from}");
                    // and back
                    for cap in [usize::MAX, 0, 7, 8, 9, 17] {
                        let mut out = vec![b'#'];
                        let (got, units) = varints_to_short(&want[at..], cap, &mut out);
                        assert!(units <= cap && got <= lead_bytes && (lead_bytes - got < 8 || cap - units < 8), "{s:?} back from {from} cap {cap}: {got} of {lead_bytes}");
                        assert!(s.is_char_boundary(from + got), "{s:?} back from {from} cap {cap}: {got} cuts a character");
                        assert_eq!(out[1..], b[from..from + got], "{s:?} back from {from} cap {cap}");
                        assert_eq!(units, s[from..from + got].chars().count(), "{s:?} back from {from} cap {cap}");
                    }
                }
            }
        }
        // a unit below 0x80 in two bytes (not what a runtime sends) is no pair: it would be an
        // overlong UTF-8
        let mut out = Vec::new();
        assert_eq!(varints_to_short(&[0xC1, 0x00, 0x9F, 0x08, 0x9F, 0x08, 0x9F, 0x08], usize::MAX, &mut out), (0, 0));
        assert_eq!(varints_to_short(&[b'a', 0x9F, 0x08, b' ', 0x9F, 0x08, 0x9F, 0x08, 0x9F, 0x08, 0xC1, 0x00, 0, 0, 0, 0], usize::MAX, &mut out), (10, 6));
        assert_eq!(out, "aП ППП".as_bytes());
    }
}
