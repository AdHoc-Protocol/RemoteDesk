// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Wire-format primitives shared by receivers, transmitters, and framing.
//!
//! Includes varints, zig-zag integers, CRC-32C, packed fields, timestamp conversion, and state
//! machine mode constants. Tests cover checksum vectors, numeric boundaries, and encoded
//! representations.

// Mode constants

/// Resume modes stored in receiver and transmitter state.
///
/// Select the partially completed operation to continue after an input or output boundary. These
/// constants describe internal state and are not serialized.
pub mod mode {
    /// Idle / done: nothing mid-assembly; advance to the next field. Also the success return of a
    /// write.
    pub const OK: u32 = i32::MAX as u32;
    /// Mid-string: assembling a string (varint char count, then the characters).
    pub const STR: u32 = OK - 100;
    /// Mid-string over UTF-8: assembling a string (varint char count, then the characters as raw
    /// UTF-8 bytes).
    pub const STR_UTF8: u32 = STR - 1;
    /// Generic "need more bytes": resume the half-finished step at the slot's saved state.
    pub const RETRY: u32 = STR + 1;
    /// Mid fixed 4-byte value: raw 32-bit payload gathered into u4.
    pub const VAL4: u32 = RETRY + 1;
    /// Mid fixed 8-byte value: raw 64-bit payload gathered into u8.
    pub const VAL8: u32 = VAL4 + 1;
    /// Mid 1-byte integer: one wire byte to be sign-extended into u8.
    pub const INT1: u32 = VAL8 + 1;
    /// Mid 2-byte integer: two wire bytes to be sign-extended into u8.
    pub const INT2: u32 = INT1 + 1;
    /// Mid 4-byte integer: four wire bytes to be sign-extended into u8.
    pub const INT4: u32 = INT2 + 1;
    /// Mid collection length stored in 0 bytes (length fixed/implicit).
    pub const LEN0: u32 = INT4 + 1;
    /// Mid collection length stored in 1 byte.
    pub const LEN1: u32 = LEN0 + 1;
    /// Mid collection length stored in 2 bytes.
    pub const LEN2: u32 = LEN1 + 1;
    /// Mid bit-field: gathering packed bits into the `bits` accumulator.
    pub const BITS: u32 = LEN2 + 1;
    /// Mid bit-field followed by byte-aligned data (bits first, then bytes).
    pub const BITS_BYTES: u32 = BITS + 1;
    /// Mid base-128 varint: gathering a variable-length integer one continuation byte at a time.
    pub const VARINT: u32 = BITS_BYTES + 1;
    /// Parked at a streamed-datatype boundary: the next drive routes into `unpark()` to re-frame
    /// the chained pack.
    pub const STREAM_PACK: u32 = VARINT + 1;
    /// Mid PREFIX-LESS string: varint characters with no declared count — the string ends WITH the
    /// enclosing stream, finalized by the framer's `[0]`-terminator branch.
    pub const STR_RAW: u32 = STREAM_PACK + 1;
}

// Varint encode / decode

/// Encodes `src` as a base-128 varint into `buf` at `offset`, returning the first offset after the
/// value. Continuation bytes have their high bit set.
///
/// # Panics
///
/// Panics if the destination has insufficient room. Use
/// [`Tx::put_varint`](super::connection::transmitter::Tx::put_varint) for incremental output.
pub fn varint_encode(mut src: u64, buf: &mut [u8], mut offset: usize) -> usize {
    loop {
        if src < 0x80 {
            buf[offset] = src as u8;
            return offset + 1;
        }
        buf[offset] = 0x80 | (src as u8);
        offset += 1;
        src >>= 7;
    }
}

/// Decodes a base-128 varint from `buf[offset..]`.
///
/// Returns the value and first offset after it, or `None` for incomplete input. Payload bits beyond
/// 64 bits are ignored; this helper does not reject overlong encodings.
pub fn varint_decode(buf: &[u8], mut offset: usize) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    while offset < buf.len() {
        let b = buf[offset] as u64;
        offset += 1;
        if b < 0x80 {
            if shift < 64 {
                value |= b << shift;
            }
            return Some((value, offset));
        }
        if shift < 64 {
            value |= (b & 0x7F) << shift; // past 64 bits a malformed varint adds nothing, and does not panic
        }
        shift += 7;
    }
    None
}

/// Maximum number of bytes a 64-bit varint can occupy on the wire (ceil(64/7) = 10).
pub const MAX_VARINT_BYTES: usize = 10;

// Zig-zag encoding

/// Maps signed integers to unsigned values so small magnitudes produce short varints. `right` is
/// the source width minus one: 63 for `i64`, 31 for `i32`.
#[inline]
pub fn zig_zag_encode(src: i64, right: u32) -> u64 {
    ((src << 1) ^ (src >> right)) as u64
}

/// Decodes a signed integer encoded by [`zig_zag_encode`].
#[inline]
pub fn zig_zag_decode(src: u64) -> i64 {
    -((src & 1) as i64) ^ ((src >> 1) as i64)
}

// CRC-32C (Castagnoli): the check of a framed pack

/// Length of the CRC checksum in bytes on the wire (`CRC_LEN_BYTES`).
pub const CRC_LEN_BYTES: usize = 4;

/// CRC-32C tables for a processor without the instruction, built at compile time: `CRC32C_T[0]` is
/// the byte table, `CRC32C_T[k]` the CRC of a byte followed by `k` zero bytes - eight independent
/// lookups take eight bytes at once (slicing-by-8).
static CRC32C_T: [[u32; 256]; 8] = CRC32C_TABLES;

const CRC32C_TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut b = 0;
        while b < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            b += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let c = t[k - 1][i];
            t[k][i] = (c >> 8) ^ t[0][(c & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

/// The bytes of a block of each of the three runs of [`crc32c`].
const CRC_BLOCK: usize = 256;

/// Byte `k` of a crc32 state, value `v`, carried over [`CRC_BLOCK`] zero bytes:
/// `CRC32C_ZEROS[k][v]` - the state is linear in its bytes. Built at compile time, eight zero bytes
/// a step of the slicing tables.
static CRC32C_ZEROS: [[u32; 256]; 4] = {
    let t = &CRC32C_TABLES;
    let mut z = [[0u32; 256]; 4];
    let mut k = 0;
    while k < 4 {
        let mut v = 0;
        while v < 256 {
            let mut c = (v as u32) << (8 * k);
            let mut j = 0;
            while j < CRC_BLOCK {
                c = t[7][(c & 0xFF) as usize] ^ t[6][((c >> 8) & 0xFF) as usize] ^ t[5][((c >> 16) & 0xFF) as usize] ^ t[4][(c >> 24) as usize];
                j += 8;
            }
            z[k][v] = c;
            v += 1;
        }
        k += 1;
    }
    z
};

/// The crc32 state `c` carried over [`CRC_BLOCK`] zero bytes.
#[inline(always)]
fn crc32c_zeros(c: u32) -> u32 {
    let z = &CRC32C_ZEROS;
    z[0][(c & 0xFF) as usize] ^ z[1][((c >> 8) & 0xFF) as usize] ^ z[2][((c >> 16) & 0xFF) as usize] ^ z[3][(c >> 24) as usize]
}

/// The CRC-32C of `crc` followed by `data`: 0 to start, the result goes on with the next bytes. The
/// crc32 instruction where the processor has it: one waits for the one before it, so three runs go
/// side by side over neighbouring blocks of [`CRC_BLOCK`] bytes, the first one's CRC then carried
/// over the other two.
#[inline]
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse4.2") {
            // SAFETY: the processor has SSE4.2
            return !unsafe { crc32c_sse42(!crc, data) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("crc") {
            // SAFETY: the processor has the CRC instructions
            return !unsafe { crc32c_arm(!crc, data) };
        }
    }
    !crc32c_table(!crc, data)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(crc: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};
    let (p, n) = (data.as_ptr(), data.len());
    let word = |at: usize| (p.add(at) as *const u64).read_unaligned();
    let mut c = crc as u64;
    let mut i = 0;
    while i + 3 * CRC_BLOCK <= n {
        let (mut c1, mut c2) = (0u64, 0u64);
        let mut j = i;
        while j < i + CRC_BLOCK {
            c = _mm_crc32_u64(c, word(j));
            c1 = _mm_crc32_u64(c1, word(j + CRC_BLOCK));
            c2 = _mm_crc32_u64(c2, word(j + 2 * CRC_BLOCK));
            j += 8;
        }
        c = (crc32c_zeros(crc32c_zeros(c as u32) ^ c1 as u32) ^ c2 as u32) as u64;
        i += 3 * CRC_BLOCK;
    }
    while i + 8 <= n {
        c = _mm_crc32_u64(c, word(i));
        i += 8;
    }
    let mut crc = c as u32;
    while i < n {
        crc = _mm_crc32_u8(crc, *p.add(i));
        i += 1;
    }
    crc
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc32c_arm(mut crc: u32, data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let (p, n) = (data.as_ptr(), data.len());
    let word = |at: usize| (p.add(at) as *const u64).read_unaligned();
    let mut i = 0;
    while i + 3 * CRC_BLOCK <= n {
        let (mut c1, mut c2) = (0u32, 0u32);
        let mut j = i;
        while j < i + CRC_BLOCK {
            crc = __crc32cd(crc, word(j));
            c1 = __crc32cd(c1, word(j + CRC_BLOCK));
            c2 = __crc32cd(c2, word(j + 2 * CRC_BLOCK));
            j += 8;
        }
        crc = crc32c_zeros(crc32c_zeros(crc) ^ c1) ^ c2;
        i += 3 * CRC_BLOCK;
    }
    while i + 8 <= n {
        crc = __crc32cd(crc, word(i));
        i += 8;
    }
    while i < n {
        crc = __crc32cb(crc, *p.add(i));
        i += 1;
    }
    crc
}

/// The CRC by the tables: eight bytes a step, the tail a byte at a time.
fn crc32c_table(mut crc: u32, data: &[u8]) -> u32 {
    let t = &CRC32C_T;
    let mut w = data.chunks_exact(8);
    for x in &mut w {
        let lo = crc ^ u32::from_le_bytes([x[0], x[1], x[2], x[3]]);
        crc = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][x[4] as usize]
            ^ t[2][x[5] as usize]
            ^ t[1][x[6] as usize]
            ^ t[0][x[7] as usize];
    }
    for &b in w.remainder() {
        crc = (crc >> 8) ^ t[0][((crc ^ b as u32) & 0xFF) as usize];
    }
    crc
}

// Fixed-width little-endian integer helpers.

/// Reads up to eight bytes at `offset` as a little-endian unsigned integer. Bits above the selected
/// byte width are zero.
#[inline]
pub fn read_uint_le(buf: &[u8], offset: usize, bytes: usize) -> u64 {
    debug_assert!(bytes <= 8 && offset + bytes <= buf.len());
    let mut out = 0u64;
    for i in 0..bytes {
        out |= (buf[offset + i] as u64) << (i * 8);
    }
    out
}

/// Writes the lowest `bytes` bytes of `src` at `offset` in little-endian order.
#[inline]
pub fn write_uint_le(value: u64, buf: &mut [u8], offset: usize, bytes: usize) {
    debug_assert!(bytes <= 8 && offset + bytes <= buf.len());
    for i in 0..bytes {
        buf[offset + i] = ((value >> (i * 8)) & 0xFF) as u8;
    }
}

// Bit-level pack/unpack

/// Writes the low `src_bits` bits of `src` into `dst` starting at `dst_bit`. Preserves bits outside
/// the field. A 64-bit field at an unaligned bit offset spans up to nine bytes; a `u128`
/// intermediate preserves those boundary bits.
#[inline]
pub fn pack_bits(src: u64, dst: &mut [u8], dst_bit: i32, src_bits: i32) {
    let byte_idx = (dst_bit / 8) as usize;
    let bit_idx = (dst_bit % 8) as u32;
    let mask = if src_bits >= 64 { u64::MAX } else { (1u64 << src_bits) - 1 };
    let v = ((src & mask) as u128) << bit_idx;
    let total_bits = bit_idx as i32 + src_bits;
    let total_bytes = ((total_bits + 7) / 8) as usize;
    for i in 0..total_bytes {
        let at = byte_idx + i;
        if dst.len() <= at {
            break;
        }
        let mut keep = 0u8;
        if i == 0 {
            keep |= (1u8 << bit_idx) - 1;
        }
        let used = total_bits - i as i32 * 8;
        if used < 8 {
            keep |= !((1u8 << used) - 1);
        }
        dst[at] = (dst[at] & keep) | ((v >> (i * 8)) as u8 & !keep);
    }
}

/// Reads `src_bits` bits from `src` at `src_bit` into the low bits of a `u64`.
#[inline]
pub fn unpack_bits(src: &[u8], src_bit: i32, src_bits: i32) -> u64 {
    let byte_idx = (src_bit / 8) as usize;
    let bit_idx = (src_bit % 8) as u32;
    let total_bytes = ((bit_idx as i32 + src_bits + 7) / 8) as usize;
    let mut raw = 0u128;
    for i in 0..total_bytes {
        if byte_idx + i < src.len() {
            raw |= (src[byte_idx + i] as u128) << (i as u32 * 8);
        }
    }
    let mask = if src_bits >= 64 { u64::MAX } else { (1u64 << src_bits) - 1 };
    (raw >> bit_idx) as u64 & mask
}

// Little-endian block copy

/// Copies `src` to `dst`, swapping each `width`-sized element on a big-endian host. On a
/// little-endian host — the common case — it is a plain copy.
pub fn copy_le(src: &[u8], dst: &mut [u8], width: usize) {
    #[cfg(target_endian = "little")]
    let _ = width;
    #[cfg(target_endian = "little")]
    dst[..src.len()].copy_from_slice(src);
    #[cfg(target_endian = "big")]
    {
        if width == 1 {
            dst[..src.len()].copy_from_slice(src);
            return;
        }
        for i in (0..src.len()).step_by(width) {
            for j in 0..width {
                dst[i + j] = src[i + width - 1 - j];
            }
        }
    }
}

// Collection helpers for generated code

/// Resizes a `Vec` to `new_length`, filling any expansion with `fill_value`.
pub fn resize_fill<T: Clone>(src: &mut Vec<T>, new_length: usize, fill_value: T) {
    src.resize(new_length, fill_value);
}

/// Converts a slice to a `Vec`, taking at most `max` elements.
pub fn to_list<T: Clone>(src: &[T], max: usize) -> Vec<T> {
    src[..max.min(src.len())].to_vec()
}

/// A new `Vec` of `size` elements, filled with `fill`.
pub fn size_array<T: Clone>(fill: T, size: usize) -> Vec<T> {
    vec![fill; size]
}

/// Computes a list size using the same layout as [`size_array`].
pub fn size_list<T: Clone>(fill: T, size: usize) -> Vec<T> {
    size_array(fill, size)
}

// bits_as

/// The bit pattern of one packed item — already masked — as the item's type. What `get_bits<T>`
/// gives for the item.
pub trait BitsAs: Copy {
    fn from_bits(code: u32) -> Self;
}

impl BitsAs for i32 {
    #[inline]
    fn from_bits(code: u32) -> Self {
        code as i32
    }
}
impl BitsAs for u32 {
    #[inline]
    fn from_bits(code: u32) -> Self {
        code
    }
}
impl BitsAs for f32 {
    #[inline]
    fn from_bits(code: u32) -> Self {
        f32::from_bits(code)
    }
}
impl BitsAs for i16 {
    #[inline]
    fn from_bits(code: u32) -> Self {
        code as u16 as i16
    }
}
impl BitsAs for u16 {
    #[inline]
    fn from_bits(code: u32) -> Self {
        code as u16
    }
}
impl BitsAs for i8 {
    #[inline]
    fn from_bits(code: u32) -> Self {
        code as u8 as i8
    }
}
impl BitsAs for u8 {
    #[inline]
    fn from_bits(code: u32) -> Self {
        code as u8
    }
}

/// `bits_as<T>(code)`.
#[inline]
pub fn bits_as<T: BitsAs>(code: u64) -> T {
    T::from_bits(code as u32)
}

// Hex dump

const HEX_CHARS: &[u8; 16] = b"0123456789ABCDEF";

/// The hex dump of `src`.
pub fn hex_dump(src: &[u8]) -> String {
    let mut dst = String::with_capacity(src.len() * 5);
    hex_dump_into(src, 0, src.len(), &mut dst);
    dst
}

/// `ToString(byte[], int, int, StringBuilder)`: 16 bytes per line — the hex offset (leading zeroes
/// as spaces), the bytes, the decimal end offset padded to 8.
pub fn hex_dump_into(src: &[u8], start: usize, end: usize, dst: &mut String) {
    for i in (start..end).step_by(16) {
        let mut leading = true;
        for shift in (0..=28).rev().step_by(4) {
            let nibble = (i >> shift) & 0xF;
            if leading && nibble == 0 && shift > 0 {
                dst.push(' ');
            } else {
                leading = false;
                dst.push(HEX_CHARS[nibble] as char);
            }
        }
        dst.push_str(": ");
        for j in 0..16 {
            if i + j < end {
                let value = src[i + j] as usize;
                dst.push(HEX_CHARS[(value >> 4) & 0xF] as char);
                dst.push(HEX_CHARS[value & 0xF] as char);
                dst.push(' ');
            } else {
                dst.push_str("   ");
            }
        }
        dst.push_str(": ");
        let s = (i + 16).min(end).to_string();
        for _ in s.len()..8 {
            dst.push(' ');
        }
        dst.push_str(&s);
        dst.push('\n');
    }
}

// DateTime

/// Parses `date_time` using an explicit format into [`std::time::SystemTime`].
///
/// Supported tokens include `yyyy`/`yy` for years, `MMMM`/`MMM`/`MM`/`M` for months, `dddd`/`ddd`
/// for weekday validation, `dd`/`d` for days, `HH`/`H` and `hh`/`h` for hours, `tt`/`t` for AM/PM,
/// `mm`/`m` for minutes, and `ss`/`s` for seconds. Up to seven `f` or `F` tokens represent
/// fractional seconds. `z`/`zz`/`zzz` and `K` parse numeric offsets or `Z`. Quotes and backslash
/// escapes introduce literal text; other characters match literally.
///
/// Month and weekday names use English names without case sensitivity. Invalid calendar dates are
/// rejected. Explicit offsets are applied; input without an offset is interpreted as UTC. Dates
/// before the Unix epoch are supported.
pub fn date_time(date_time: &str, format: &str) -> Result<std::time::SystemTime, String> {
    let src = date_time.as_bytes();
    let fmt = format.as_bytes();
    let (mut year, mut month, mut day, mut hour, mut minute, mut second, mut nanos) = (1i64, 1u32, 1u32, 0u32, 0u32, 0u32, 0u32);
    let (mut pm, mut h12, mut offset, mut weekday) = (None::<bool>, false, 0i64, None::<i64>);
    let mut di = 0usize;
    let mut fi = 0usize;
    let bad = || format!("'{date_time}' does not match format '{format}'");
    // `n` digits exactly, or 1 to `n` when `flexible`
    let digits = |di: &mut usize, min: usize, max: usize| -> Result<u64, String> {
        let start = *di;
        while *di < src.len() && *di - start < max && src[*di].is_ascii_digit() {
            *di += 1;
        }
        if *di - start < min {
            return Err(format!("'{date_time}' does not match format '{format}'"));
        }
        Ok(std::str::from_utf8(&src[start..*di]).unwrap().parse::<u64>().unwrap())
    };
    while fi < fmt.len() {
        let c = fmt[fi];
        let mut run = 1;
        while fi + run < fmt.len() && fmt[fi + run] == c {
            run += 1;
        }
        match c {
            b'y' => {
                let v = digits(&mut di, run.max(1).min(4), if run <= 2 { 2 } else { run.max(4) })? as i64;
                year = if run <= 2 {
                    if v < 50 {
                        2000 + v
                    } else {
                        1900 + v
                    }
                } else {
                    v
                }; // InvariantCulture TwoDigitYearMax 2049
            }
            b'M' if run <= 2 => month = digits(&mut di, run, 2)? as u32,
            b'M' => month = name(src, &mut di, if run == 3 { &MONTHS_ABBR } else { &MONTHS }).ok_or_else(bad)? as u32 + 1,
            b'd' if run <= 2 => day = digits(&mut di, run, 2)? as u32,
            b'd' => weekday = Some(name(src, &mut di, if run == 3 { &DAYS_ABBR } else { &DAYS }).ok_or_else(bad)? as i64),
            b'H' => hour = digits(&mut di, run.min(2), 2)? as u32,
            b'h' => {
                hour = digits(&mut di, run.min(2), 2)? as u32;
                h12 = true;
            }
            b'm' => minute = digits(&mut di, run.min(2), 2)? as u32,
            b's' => second = digits(&mut di, run.min(2), 2)? as u32,
            b'f' | b'F' if run <= 7 => {
                let start = di;
                let v = digits(&mut di, if c == b'f' { run } else { 0 }, run)?;
                nanos = (v * 10u64.pow(9 - (di - start) as u32)) as u32;
            }
            b't' => {
                let n = run.min(2);
                if src.len() < di + n {
                    return Err(bad());
                }
                let t = &src[di..di + n];
                pm = Some(match t[0].to_ascii_uppercase() {
                    b'A' => false,
                    b'P' => true,
                    _ => return Err(bad()),
                });
                if n == 2 && !t[1].eq_ignore_ascii_case(&b'M') {
                    return Err(bad());
                }
                di += n;
            }
            b'z' | b'K' => {
                if c == b'K' && (di == src.len() || !matches!(src[di], b'+' | b'-' | b'Z' | b'z')) {
                    // K: no zone in the input is allowed
                } else if di < src.len() && (src[di] == b'Z' || src[di] == b'z') {
                    di += 1;
                } else {
                    if di == src.len() || !matches!(src[di], b'+' | b'-') {
                        return Err(bad());
                    }
                    let sign = if src[di] == b'-' { -1 } else { 1 };
                    di += 1;
                    let h = digits(&mut di, 1, 2)? as i64;
                    let mut m = 0i64;
                    if di < src.len() && src[di] == b':' {
                        di += 1;
                        m = digits(&mut di, 2, 2)? as i64;
                    } else if di + 1 < src.len() && src[di].is_ascii_digit() && src[di + 1].is_ascii_digit() {
                        m = digits(&mut di, 2, 2)? as i64;
                    }
                    offset = sign * (h * 3600 + m * 60);
                }
            }
            b'\'' | b'"' => {
                // text in quotes: as it is
                let mut q = fi + 1;
                while q < fmt.len() && fmt[q] != c {
                    if di >= src.len() || src[di] != fmt[q] {
                        return Err(bad());
                    }
                    di += 1;
                    q += 1;
                }
                fi = q + 1;
                continue;
            }
            b'\\' => {
                if fi + 1 >= fmt.len() || di >= src.len() || src[di] != fmt[fi + 1] {
                    return Err(bad());
                }
                di += 1;
                fi += 2;
                continue;
            }
            _ => {
                for _ in 0..run {
                    if di >= src.len() || src[di] != c {
                        return Err(bad());
                    }
                    di += 1;
                }
            }
        }
        fi += run;
    }
    if di != src.len() {
        return Err(format!("trailing characters in '{date_time}'"));
    }
    if h12 || pm.is_some() {
        if !(1..=12).contains(&hour) && h12 {
            return Err(format!("hour {hour} is not of a 12-hour clock in '{date_time}'"));
        }
        if pm == Some(true) && hour < 12 {
            hour += 12;
        } else if pm == Some(false) && hour == 12 {
            hour = 0;
        }
    }
    if 23 < hour || 59 < minute || 59 < second {
        return Err(format!("invalid time in '{date_time}'"));
    }
    let days = days_from_civil(year as i32, month, day)? as i64;
    if weekday.is_some_and(|w| w != (days + 4).rem_euclid(7)) {
        // 1970-01-01 was a Thursday (4, Sunday 0)
        return Err(format!("'{date_time}': the day of the week is not that of the date"));
    }
    let secs = days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64 - offset;
    Ok(if 0 <= secs {
        std::time::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos)
    } else {
        // before 1970: the whole seconds back, then the fraction forward
        std::time::UNIX_EPOCH - std::time::Duration::new((-secs) as u64, 0) + std::time::Duration::new(0, nanos)
    })
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const MONTHS_ABBR: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const DAYS_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Which of `names` the input has at `di`, in any case: its index, `di` past it.
fn name(src: &[u8], di: &mut usize, names: &[&str]) -> Option<usize> {
    let i = names.iter().position(|n| src.len() - *di >= n.len() && src[*di..*di + n.len()].eq_ignore_ascii_case(n.as_bytes()))?;
    *di += names[i].len();
    Some(i)
}

/// Days since 1970-01-01 from a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i32, m: u32, d: u32) -> Result<i32, String> {
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let last = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=12).contains(&m) || d == 0 || last < d {
        return Err(format!("invalid date {y}-{m}-{d}"));
    }
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok((era * 146097 + doe - 719468) as i32)
}

/// A `DateTime` field with no Def narrowing it travels as the milliseconds from the Unix epoch,
/// signed: a moment before 1970 is negative.
#[inline]
pub fn unix_millis(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => {
            // Round down across the epoch: half a millisecond before it is -1 ms.
            let d = e.duration();
            -(d.as_millis() as i64) - i64::from(d.subsec_nanos() % 1_000_000 != 0)
        }
    }
}

/// The moment `ms` milliseconds from the Unix epoch.
#[inline]
pub fn at_unix_millis(ms: i64) -> std::time::SystemTime {
    if ms < 0 {
        std::time::UNIX_EPOCH - std::time::Duration::from_millis(ms.unsigned_abs())
    } else {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64)
    }
}

/// A UUID ( `Guid` of the description ) is a `u128` whose most significant byte is byte 0 of RFC 9562,
/// as `uuid::Uuid::from_u128` / `as_u128`: the order of `u128` is the order of RFC, the same in all
/// languages. On the wire it is a `UInt128`.
pub mod uuid {
    const VERSION: u128 = 0xF << 76;
    const VARIANT: u128 = 0b11 << 62;
    const RFC: u128 = 0b10 << 62;

    /// The 16 bytes of RFC 9562.
    #[inline(always)]
    pub fn to_bytes(v: u128) -> [u8; 16] {
        v.to_be_bytes()
    }

    /// The UUID of the 16 bytes of RFC 9562.
    #[inline(always)]
    pub fn from_bytes(src: &[u8; 16]) -> u128 {
        u128::from_be_bytes(*src)
    }

    /// The version: the high 4 bits of byte 6 of RFC.
    #[inline(always)]
    pub fn version(v: u128) -> u8 {
        (v >> 76) as u8 & 0xF
    }

    /// `random` as a version 4 UUID: the version and the RFC variant over its bits.
    #[inline(always)]
    pub fn v4(random: u128) -> u128 {
        random & !(VERSION | VARIANT) | 4 << 76 | RFC
    }

    /// A version 7 UUID: the low 48 bits of `unix_ms` first, then the bits of `random`.
    #[inline(always)]
    pub fn v7(unix_ms: u64, random: u128) -> u128 {
        (unix_ms as u128 & 0xFFFF_FFFF_FFFF) << 80 | random & ((1 << 80) - 1) & !(VERSION | VARIANT) | 7 << 76 | RFC
    }

    /// A random version 4 UUID.
    pub fn new_v4() -> u128 {
        let mut b = [0u8; 16];
        let _ = getrandom::fill(&mut b);
        v4(u128::from_le_bytes(b))
    }

    /// A version 7 UUID of now.
    pub fn new_v7() -> u128 {
        let mut b = [0u8; 16];
        let _ = getrandom::fill(&mut b);
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        v7(ms, u128::from_le_bytes(b))
    }

    /// The canonical text of RFC 9562 into `dst`: 36 chars `8-4-4-4-12`, lowercase hex.
    pub fn to_chars(v: u128, dst: &mut [u8; 36]) -> &str {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut shift = 128;
        for (i, c) in dst.iter_mut().enumerate() {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                *c = b'-';
                continue;
            }
            shift -= 4;
            *c = HEX[(v >> shift) as usize & 0xF];
        }
        // only ASCII is there
        unsafe { std::str::from_utf8_unchecked(dst) }
    }

    /// The UUID of the canonical text of RFC 9562, either case; `None` — `src` is not one.
    pub fn from_chars(src: &[u8]) -> Option<u128> {
        if src.len() != 36 {
            return None;
        }
        let mut v = 0u128;
        for (i, &c) in src.iter().enumerate() {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                if c != b'-' {
                    return None;
                }
                continue;
            }
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return None,
            };
            v = v << 4 | d as u128;
        }
        Some(v)
    }

    /// The canonical text of RFC 9562 for `{}`, without a `String`.
    pub struct Text(pub u128);

    impl std::fmt::Display for Text {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let mut b = [0u8; 36];
            f.write_str(to_chars(self.0, &mut b))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rfc_text_bytes_versions() {
            // RFC 9562, appendix A.6: the v7 example
            let t = "017f22e2-79b0-7cc3-98c4-dc0c0c07398f";
            let v = from_chars(t.as_bytes()).unwrap();
            assert_eq!(v, 0x017f22e2_79b0_7cc3_98c4_dc0c0c07398f);
            assert_eq!(to_chars(v, &mut [0u8; 36]), t);
            assert_eq!(Text(v).to_string(), t);
            assert_eq!(from_chars(t.to_uppercase().as_bytes()), Some(v));
            assert_eq!(to_bytes(v)[0], 0x01);
            assert_eq!(from_bytes(&to_bytes(v)), v);
            assert_eq!(version(v), 7);
            assert_eq!(v7(0x017F22E279B0, v), v);
            assert_eq!(version(v4(!0)), 4);
            assert_eq!(v4(!0) >> 62 & 0b11, 0b10);
            assert_eq!(version(new_v7()), 7);
            assert_eq!(from_chars(b"017f22e2-79b0-7cc3-98c4-dc0c0c07398"), None);
            assert_eq!(from_chars(b"017f22e2+79b0-7cc3-98c4-dc0c0c07398f"), None);
        }
    }
}

/// A decimal of the description ( `[Decimal(p, s)] decimal` ) is a signed integer of units 10^-s within
/// ±(10^p - 1), p 1..=38, 0 <= s <= p. The generated type of each ( `Money`, `Decimal_5_2` ) wraps the units
/// and calls these with its constants. Exact: no floating point, nothing allocated.
pub mod decimal {
    /// The longest text: the sign, 38 digits, the point and the `0` before it ( `-0.0…` when s == p ).
    pub const MAX_CHARS: usize = 41;

    /// 10^n, n 0..=38.
    pub const POW10: [u128; 39] = {
        let mut t = [1u128; 39];
        let mut i = 1;
        while i < 39 {
            t[i] = t[i - 1] * 10;
            i += 1;
        }
        t
    };

    /// 10^19: the units wider than 64 bits are written as two `u64` parts of 19 digits.
    const P19: u128 = 10_000_000_000_000_000_000;

    /// The text of the magnitude `hi`·10^19 + `lo` with exactly `scale` digits after the point, `-` before it
    /// when `neg`, at the end of `dst`; returns where the text starts.
    #[inline]
    fn digits(neg: bool, hi: u64, lo: u64, scale: u32, dst: &mut [u8; MAX_CHARS]) -> usize {
        let mut i = MAX_CHARS;
        let (mut cur, mut next, mut left) = (lo, hi, if hi == 0 { u32::MAX } else { 19 });
        let mut n = 0u32;
        loop {
            if n == scale && scale != 0 {
                i -= 1;
                dst[i] = b'.';
            }
            i -= 1;
            dst[i] = b'0' + (cur % 10) as u8;
            cur /= 10;
            n += 1;
            left -= 1;
            if left == 0 {
                cur = next;
                next = 0;
                left = u32::MAX;
            }
            if scale < n && cur == 0 && next == 0 {
                break;
            }
        }
        if neg {
            i -= 1;
            dst[i] = b'-';
        }
        i
    }

    /// The text of `units` with exactly `scale` digits after the point ( `-0.05`, `12.30`; no point when
    /// `scale` is 0 ), written into `dst`.
    #[inline]
    pub fn to_chars(units: i128, scale: u32, dst: &mut [u8; MAX_CHARS]) -> &str {
        let m = units.unsigned_abs();
        let i = if m >> 64 == 0 {
            digits(units < 0, 0, m as u64, scale, dst)
        } else {
            digits(units < 0, (m / P19) as u64, (m % P19) as u64, scale, dst)
        };
        // only ASCII is there
        unsafe { std::str::from_utf8_unchecked(&dst[i..]) }
    }

    /// [`to_chars`] of units that fit 64 bits: no 128-bit division.
    #[inline]
    pub fn to_chars64(units: i64, scale: u32, dst: &mut [u8; MAX_CHARS]) -> &str {
        let i = digits(units < 0, 0, units.unsigned_abs(), scale, dst);
        // only ASCII is there
        unsafe { std::str::from_utf8_unchecked(&dst[i..]) }
    }

    /// The units of `src` at `scale`: an optional sign, digits, an optional point and digits. `None` — not such a
    /// text, a nonzero digit past `scale`, or a value outside ±(10^`precision` - 1).
    pub fn from_chars(src: &[u8], precision: u32, scale: u32) -> Option<i128> {
        let (neg, src) = match src.first() {
            Some(b'-') => (true, &src[1..]),
            Some(b'+') => (false, &src[1..]),
            _ => (false, src),
        };
        let mut acc = 0u128; // at most `precision` significant digits: below 10^38
        let mut sig = 0u32; // the digits from the first nonzero one
        let mut frac = u32::MAX; // the digits taken after the point; MAX before it
        let mut any = false;
        for &c in src {
            if c == b'.' && frac == u32::MAX {
                frac = 0;
                continue;
            }
            let d = c.wrapping_sub(b'0');
            if 9 < d {
                return None;
            }
            any = true;
            if frac != u32::MAX {
                if frac == scale {
                    if d != 0 {
                        return None;
                    }
                    continue;
                }
                frac += 1;
            }
            if acc != 0 || d != 0 {
                sig += 1;
                if precision < sig {
                    return None;
                }
            }
            acc = acc * 10 + d as u128;
        }
        if !any {
            return None;
        }
        let pad = scale - if frac == u32::MAX { 0 } else { frac };
        if acc != 0 && precision < sig + pad {
            return None;
        }
        let m = (acc * POW10[pad as usize]) as i128;
        Some(if neg { -m } else { m })
    }

    /// [`from_chars`] of units that fit 64 bits ( `precision` <= 18 ).
    #[inline]
    pub fn from_chars64(src: &[u8], precision: u32, scale: u32) -> Option<i64> {
        match from_chars(src, precision, scale) {
            Some(v) => Some(v as i64),
            None => None,
        }
    }

    /// The error of `FromStr` of a decimal type: the text is not a value of its precision and scale.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ParseError;

    impl std::fmt::Display for ParseError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("not a decimal of the precision and scale of the type")
        }
    }

    impl std::error::Error for ParseError {}

    /// The rounding of a result that does not fit its scale: the modes and the order of `java.math.RoundingMode`, the
    /// same in every language of AdHoc. `Unnecessary` — a part to drop is an error.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum RoundingMode {
        Up,
        Down,
        Ceiling,
        Floor,
        HalfUp,
        HalfDown,
        HalfEven,
        Unnecessary,
    }

    const M64: u128 = u64::MAX as u128;

    /// The magnitude `q` rounded by `mode`: `exact` — nothing dropped, `half` — the dropped part against a half of the
    /// step, `neg` — the sign of the result. `None` — `Unnecessary` and a part to drop.
    #[inline]
    fn round(q: u128, exact: bool, half: std::cmp::Ordering, neg: bool, mode: RoundingMode) -> Option<u128> {
        use std::cmp::Ordering::{Equal, Greater, Less};
        if exact {
            return Some(q);
        }
        let up = match mode {
            RoundingMode::Up => true,
            RoundingMode::Down => false,
            RoundingMode::Ceiling => !neg,
            RoundingMode::Floor => neg,
            RoundingMode::HalfUp => half != Less,
            RoundingMode::HalfDown => half == Greater,
            RoundingMode::HalfEven => half == Greater || half == Equal && q & 1 == 1,
            RoundingMode::Unnecessary => return None,
        };
        if up {
            q.checked_add(1)
        } else {
            Some(q)
        }
    }

    /// The signed value of the magnitude `m`: `None` — beyond `i128`.
    #[inline]
    fn signed(m: u128, neg: bool) -> Option<i128> {
        if m <= i128::MAX as u128 {
            Some(if neg { -(m as i128) } else { m as i128 })
        } else if neg && m == 1 << 127 {
            Some(i128::MIN)
        } else {
            None
        }
    }

    /// The 256-bit product of `a` and `b`: ( high, low ).
    #[inline]
    fn mul_wide(a: u128, b: u128) -> (u128, u128) {
        if (a | b) >> 64 == 0 {
            return (0, a * b);
        }
        let (a1, a0, b1, b0) = (a >> 64, a & M64, b >> 64, b & M64);
        let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
        let mid = (p00 >> 64) + (p01 & M64) + (p10 & M64);
        (p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64), mid << 64 | p00 & M64)
    }

    /// ( `r`·2^64 + `l` ) / `d` with `d` normalized ( the top bit set ) and `r` < `d`: the 64-bit quotient and the
    /// remainder ( Knuth, algorithm D, two 64-bit digits of the divisor ).
    #[inline]
    fn div_3by2(r: u128, l: u64, d: u128) -> (u64, u128) {
        let (d1, d0) = (d >> 64, d & M64);
        let mut q = if r >> 64 == d1 { M64 } else { r / d1 };
        loop {
            let lo = q * d0;
            let hi = q * d1 + (lo >> 64); // the high 128 bits of q·d, the low 64 — `lo as u64`
            let pl = lo as u64;
            if r < hi || r == hi && l < pl {
                q -= 1; // at most twice
                continue;
            }
            return (q as u64, (r - hi - (l < pl) as u128) << 64 | l.wrapping_sub(pl) as u128);
        }
    }

    /// ( `hi`·2^128 + `lo` ) / `d` with `hi` < `d`: the quotient and the remainder.
    #[inline]
    fn div_wide(hi: u128, lo: u128, d: u128) -> (u128, u128) {
        if hi == 0 {
            if (lo | d) >> 64 == 0 {
                let (l, d) = (lo as u64, d as u64);
                return ((l / d) as u128, (l % d) as u128);
            }
            return (lo / d, lo % d);
        }
        let s = d.leading_zeros();
        let (d, hi, lo) = if s == 0 { (d, hi, lo) } else { (d << s, hi << s | lo >> (128 - s), lo << s) };
        let (q1, r) = div_3by2(hi, (lo >> 64) as u64, d);
        let (q0, r) = div_3by2(r, lo as u64, d);
        ((q1 as u128) << 64 | q0 as u128, r >> s)
    }

    /// `a`·`b` / `d` rounded by `mode`, exact on the way ( 256 bits; one multiplication and one division of `u128` when
    /// the operands are 64-bit ). `None` — `d` is 0, the result is beyond `i128`, or `Unnecessary` and a part to drop.
    pub fn mul_div(a: i128, b: i128, d: i128, mode: RoundingMode) -> Option<i128> {
        if d == 0 {
            return None;
        }
        let neg = (a < 0) ^ (b < 0) ^ (d < 0);
        let d = d.unsigned_abs();
        let (hi, lo) = mul_wide(a.unsigned_abs(), b.unsigned_abs());
        if d <= hi {
            return None;
        }
        let (q, r) = div_wide(hi, lo, d);
        signed(round(q, r == 0, r.cmp(&(d - r)), neg, mode)?, neg)
    }

    /// `a` ( at any scale ) times `b` at `b_scale`: the units at the scale of `a`.
    #[inline]
    pub fn mul(a: i128, b: i128, b_scale: u32, mode: RoundingMode) -> Option<i128> {
        mul_div(a, b, *POW10.get(b_scale as usize)? as i128, mode)
    }

    /// `a` ( at any scale ) divided by `b` at `b_scale`: the units at the scale of `a`.
    #[inline]
    pub fn div(a: i128, b: i128, b_scale: u32, mode: RoundingMode) -> Option<i128> {
        mul_div(a, *POW10.get(b_scale as usize)? as i128, b, mode)
    }

    /// `u` divided by 10^`n`, rounded by `mode`.
    #[inline]
    pub fn div_pow10(u: i128, n: u32, mode: RoundingMode) -> Option<i128> {
        match POW10.get(n as usize) {
            Some(&p) => mul_div(u, 1, p as i128, mode),
            None => signed(round(0, u == 0, std::cmp::Ordering::Less, u < 0, mode)?, u < 0), // 10^39 and more: |u| is below a half of it
        }
    }

    /// The units `u` at the scale `from` — at the scale `to`.
    #[inline]
    pub fn rescale(u: i128, from: u32, to: u32, mode: RoundingMode) -> Option<i128> {
        if from <= to {
            u.checked_mul(*POW10.get((to - from) as usize)? as i128)
        } else {
            div_pow10(u, from - to, mode)
        }
    }

    /// The value of the units `u` at `scale` as `f64`: for showing only, the nearest `f64` may differ from it.
    #[inline]
    pub fn to_f64(u: i128, scale: u32) -> f64 {
        u as f64 / POW10[scale as usize] as f64
    }

    /// The exact value of `v` rounded by `mode` to `scale`: `None` — not finite, or beyond `i128`.
    pub fn from_f64(v: f64, scale: u32, mode: RoundingMode) -> Option<i128> {
        use std::cmp::Ordering::{Equal, Greater, Less};
        if !v.is_finite() || 38 < scale {
            return None;
        }
        let bits = v.to_bits();
        let neg = bits >> 63 != 0;
        let exp = (bits >> 52 & 0x7FF) as i32;
        let frac = bits & ((1 << 52) - 1);
        let (m, e) = if exp == 0 { (frac, -1074) } else { (frac | 1 << 52, exp - 1075) }; // v = m·2^e
        if m == 0 {
            return Some(0);
        }
        let (hi, lo) = mul_wide(m as u128, POW10[scale as usize]); // the units = m·10^scale·2^e, below 2^180·2^e
        if 0 <= e {
            if hi != 0 || lo.leading_zeros() <= e as u32 {
                return None;
            }
            return signed(lo << e, neg);
        }
        let k = (-e) as u32; // the units = ( hi, lo ) >> k
        let q = if 256 <= k {
            0
        } else if 128 <= k {
            hi >> (k - 128)
        } else if hi >> k != 0 {
            return None;
        } else {
            hi << (128 - k) | lo >> k
        };
        let h = k - 1; // the bit of the half of the step
        let half_bit = if h < 128 {
            lo >> h & 1 != 0
        } else if h < 256 {
            hi >> (h - 128) & 1 != 0
        } else {
            false
        };
        let below = if h <= 128 {
            h != 0 && lo << (128 - h) != 0
        } else if h < 256 {
            lo != 0 || hi << (256 - h) != 0
        } else {
            true
        };
        let half = if !half_bit {
            Less
        } else if below {
            Greater
        } else {
            Equal
        };
        signed(round(q, !half_bit && !below, half, neg, mode)?, neg)
    }

    /// A decimal type: units of 10^-`SCALE` within ±(10^`PRECISION` - 1). A generated type gives the two conversions of
    /// its units; the arithmetic is here, at the scale of the type and checked: `None` — beyond the range of the type
    /// ( an overflow ), a division by zero, or `Unnecessary` and a part to drop. The products are exact on the way: in 128
    /// bits for 64-bit units, in 256 for 128-bit ones.
    pub trait Decimal: Copy {
        const PRECISION: u32;
        const SCALE: u32;

        /// The units.
        fn units128(self) -> i128;

        /// The value of units within ±(10^`PRECISION` - 1): not checked.
        fn from_units128(u: i128) -> Self;

        /// The value of `u` units: `None` — beyond ±(10^`PRECISION` - 1).
        #[inline]
        fn of_units(u: i128) -> Option<Self> {
            if u.unsigned_abs() < POW10[Self::PRECISION as usize] {
                Some(Self::from_units128(u))
            } else {
                None
            }
        }

        /// -1, 0 or 1.
        #[inline]
        fn signum(self) -> i32 {
            self.units128().signum() as i32
        }

        /// The range is symmetric: never beyond it.
        #[inline]
        fn abs(self) -> Self {
            Self::from_units128(self.units128().abs())
        }

        #[inline]
        fn checked_neg(self) -> Option<Self> {
            Some(Self::from_units128(-self.units128()))
        }

        #[inline]
        fn checked_add(self, o: Self) -> Option<Self> {
            Self::of_units(self.units128().checked_add(o.units128())?)
        }

        #[inline]
        fn checked_sub(self, o: Self) -> Option<Self> {
            Self::of_units(self.units128().checked_sub(o.units128())?)
        }

        /// `self` times `o` ( at its own scale ), rounded by `mode` to the scale of `Self`.
        #[inline]
        fn checked_mul<O: Decimal>(self, o: O, mode: RoundingMode) -> Option<Self> {
            Self::of_units(mul(self.units128(), o.units128(), O::SCALE, mode)?)
        }

        /// `self` divided by `o` ( at its own scale ), rounded by `mode` to the scale of `Self`.
        #[inline]
        fn checked_div<O: Decimal>(self, o: O, mode: RoundingMode) -> Option<Self> {
            Self::of_units(div(self.units128(), o.units128(), O::SCALE, mode)?)
        }

        #[inline]
        fn checked_mul_int(self, k: i128) -> Option<Self> {
            Self::of_units(self.units128().checked_mul(k)?)
        }

        #[inline]
        fn checked_div_int(self, k: i128, mode: RoundingMode) -> Option<Self> {
            Self::of_units(mul_div(self.units128(), 1, k, mode)?)
        }

        /// `self` times 10^`n`.
        #[inline]
        fn checked_mul_pow10(self, n: u32) -> Option<Self> {
            Self::of_units(self.units128().checked_mul(*POW10.get(n as usize)? as i128)?)
        }

        /// `self` divided by 10^`n`, rounded by `mode`.
        #[inline]
        fn checked_div_pow10(self, n: u32, mode: RoundingMode) -> Option<Self> {
            Self::of_units(div_pow10(self.units128(), n, mode)?)
        }

        /// The units of the value at `scale`, rounded by `mode`.
        #[inline]
        fn rescale(self, scale: u32, mode: RoundingMode) -> Option<i128> {
            rescale(self.units128(), Self::SCALE, scale, mode)
        }

        /// The value as another decimal type, rounded by `mode`: `None` — beyond the range of `O`.
        #[inline]
        fn convert<O: Decimal>(self, mode: RoundingMode) -> Option<O> {
            O::of_units(self.rescale(O::SCALE, mode)?)
        }

        #[inline]
        fn to_i128(self, mode: RoundingMode) -> Option<i128> {
            self.rescale(0, mode)
        }

        #[inline]
        fn to_i64(self, mode: RoundingMode) -> Option<i64> {
            i64::try_from(self.to_i128(mode)?).ok()
        }

        /// For showing only: the nearest `f64` may differ from the value.
        #[inline]
        fn to_f64(self) -> f64 {
            to_f64(self.units128(), Self::SCALE)
        }

        /// The exact value of `v` rounded by `mode` to the scale of `Self`.
        #[inline]
        fn from_f64(v: f64, mode: RoundingMode) -> Option<Self> {
            Self::of_units(from_f64(v, Self::SCALE, mode)?)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn arithmetic_known() {
            use RoundingMode::*;
            let r: Vec<_> = [Up, Down, Ceiling, Floor, HalfUp, HalfDown, HalfEven, Unnecessary].iter().map(|&m| mul_div(-25, 1, 10, m)).collect();
            assert_eq!(r, [Some(-3), Some(-2), Some(-2), Some(-3), Some(-3), Some(-2), Some(-2), None]);
            assert_eq!(mul_div(35, 1, 10, HalfEven), Some(4));
            assert_eq!(mul_div(26, 1, 10, HalfDown), Some(3));
            assert_eq!(mul_div(1, 1, 0, Down), None);
            let e37 = POW10[37] as i128;
            assert_eq!(mul_div(e37, e37, POW10[36] as i128, Unnecessary), Some(POW10[38] as i128));
            assert_eq!(mul_div(e37, e37, 10, Down), None);
            assert_eq!(mul(250, 150, 2, HalfEven), Some(375)); // 2.50 * 1.50 = 3.75
            assert_eq!(mul(125, 50, 2, HalfEven), Some(62)); // 1.25 * 0.50 = 0.625
            assert_eq!(div(100, 300, 2, HalfUp), Some(33)); // 1.00 / 3.00 = 0.33
            assert_eq!(div(200, 300, 2, Up), Some(67));
            assert_eq!(div(100, 300, 2, Unnecessary), None);
            assert_eq!(rescale(12345, 2, 0, HalfEven), Some(123));
            assert_eq!(rescale(12350, 2, 1, HalfEven), Some(1235));
            assert_eq!(rescale(-12355, 3, 1, HalfEven), Some(-124));
            assert_eq!(rescale(1, 0, 39, Down), None);
            assert_eq!(div_pow10(-1, 60, Floor), Some(-1));
            assert_eq!(from_f64(0.1, 2, HalfEven), Some(10));
            assert_eq!(from_f64(0.125, 2, HalfEven), Some(12));
            assert_eq!(from_f64(-0.125, 2, HalfUp), Some(-13));
            assert_eq!(from_f64(0.1, 20, Unnecessary), None);
            assert_eq!(from_f64(1e300, 0, Down), None);
            assert_eq!(from_f64(f64::NAN, 0, Down), None);
            assert_eq!(to_f64(-1234, 2), -12.34);
        }

        fn text(units: i128, scale: u32) -> String {
            to_chars(units, scale, &mut [0u8; MAX_CHARS]).to_string()
        }

        #[test]
        fn text_exact_scale() {
            assert_eq!(text(-5, 2), "-0.05");
            assert_eq!(text(1230, 2), "12.30");
            assert_eq!(text(0, 2), "0.00");
            assert_eq!(text(0, 0), "0");
            assert_eq!(text(-123, 0), "-123");
            assert_eq!(text(99999, 5), "0.99999");
            assert_eq!(text(-99999, 5), "-0.99999");
            let max = (POW10[38] - 1) as i128;
            assert_eq!(text(max, 10), "9999999999999999999999999999.9999999999");
            assert_eq!(text(-max, 38), "-0.99999999999999999999999999999999999999");
            assert_eq!(text(-max, 0), "-99999999999999999999999999999999999999");
            assert_eq!(text(100_000_000_000_000_000_000, 4), "10000000000000000.0000");
            assert_eq!(text(18_446_744_073_709_551_616, 3), "18446744073709551.616");
            assert_eq!(to_chars64(i64::MIN + 1, 6, &mut [0u8; MAX_CHARS]), "-9223372036854.775807");
            assert_eq!(to_chars64(-999_999_999_999_999_999, 18, &mut [0u8; MAX_CHARS]), "-0.999999999999999999");
        }

        #[test]
        fn parse_exact() {
            assert_eq!(from_chars(b"999.99", 5, 2), Some(99999));
            assert_eq!(from_chars(b"-999.99", 5, 2), Some(-99999));
            assert_eq!(from_chars(b"1000.00", 5, 2), None);
            assert_eq!(from_chars(b"1000", 5, 2), None);
            assert_eq!(from_chars(b"0.001", 5, 2), None);
            assert_eq!(from_chars(b"1.230", 5, 2), Some(123));
            assert_eq!(from_chars(b"+.5", 5, 2), Some(50));
            assert_eq!(from_chars(b"7.", 5, 2), Some(700));
            assert_eq!(from_chars(b"000012.30", 5, 2), Some(1230));
            assert_eq!(from_chars(b"-0", 5, 2), Some(0));
            assert_eq!(from_chars(b"", 5, 2), None);
            assert_eq!(from_chars(b"-", 5, 2), None);
            assert_eq!(from_chars(b".", 5, 2), None);
            assert_eq!(from_chars(b"1.2.3", 5, 2), None);
            assert_eq!(from_chars(b"1e5", 5, 2), None);
            assert_eq!(from_chars(b" 1", 5, 2), None);
            let max = (POW10[38] - 1) as i128;
            assert_eq!(from_chars(b"9999999999999999999999999999.9999999999", 38, 10), Some(max));
            assert_eq!(from_chars(b"-0.99999999999999999999999999999999999999", 38, 38), Some(-max));
            assert_eq!(from_chars(b"10000000000000000000000000000", 38, 10), None);
            assert_eq!(from_chars(b"99999999999999999999999999999999999999999", 38, 0), None);
            assert_eq!(from_chars64(b"-999999999999.999999", 18, 6), Some(-999_999_999_999_999_999));
            assert_eq!(from_chars64(b"1000000000000", 18, 6), None);
            assert_eq!(ParseError.to_string(), "not a decimal of the precision and scale of the type");
        }

        #[test]
        fn text_parse_round_trip() {
            let mut x = 0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C834u128;
            for p in 1..=38u32 {
                let max = POW10[p as usize] - 1;
                for s in 0..=p {
                    for k in 0..64 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let m = match k {
                            0 => max,
                            1 => 0,
                            2 => 1,
                            _ => x % (max + 1),
                        };
                        let u = if x >> 127 == 0 { m as i128 } else { -(m as i128) };
                        let t = text(u, s);
                        assert_eq!(from_chars(t.as_bytes(), p, s), Some(u), "{t} p {p} s {s}");
                        if p <= 18 {
                            assert_eq!(to_chars64(u as i64, s, &mut [0u8; MAX_CHARS]), t);
                            assert_eq!(from_chars64(t.as_bytes(), p, s), Some(u as i64));
                        }
                    }
                    assert_eq!(from_chars(text(max as i128 + 1, s).as_bytes(), p, s), None);
                }
            }
        }
    }
}

// Tests

#[cfg(test)]
mod floor_and_overlong {
    use super::*;

    /// Before the epoch the milliseconds are the floor: half a millisecond before is -1.
    #[test]
    fn unix_millis_before_the_epoch_is_the_floor() {
        let t = std::time::UNIX_EPOCH - std::time::Duration::from_micros(500);
        assert_eq!(unix_millis(t), -1);
        assert_eq!(unix_millis(std::time::UNIX_EPOCH - std::time::Duration::from_millis(3)), -3);
        assert_eq!(unix_millis(std::time::UNIX_EPOCH + std::time::Duration::from_micros(1500)), 1);
    }

    /// A varint of more continuation bytes than 64 bits hold decodes without a panic.
    #[test]
    fn an_overlong_varint_does_not_panic() {
        let buf = [0xFFu8; 12].iter().copied().chain([0x01]).collect::<Vec<u8>>();
        assert_eq!(varint_decode(&buf, 0).map(|(_, at)| at), Some(13));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_64_bits_inside_a_byte() {
        for at in 0..16 {
            let mut buf = [0xA5u8; 12];
            let v = 0x8123_4567_89AB_CDEFu64;
            pack_bits(v, &mut buf, at, 64);
            assert_eq!(unpack_bits(&buf, at, 64), v, "at {at}");
            // the bits around the field are kept
            assert_eq!(unpack_bits(&buf, 0, at), 0xA5A5u64 & ((1u64 << at) - 1), "below, at {at}");
            assert_eq!(unpack_bits(&buf, at + 64, 8), (0xA5A5u64 >> ((at + 64) % 8)) & 0xFF, "above, at {at}");
        }
    }

    /// CRC-32C check vector: `123456789` must produce `0xE3069283`.
    #[test]
    fn crc32c_canonical_check() {
        assert_eq!(crc32c(0, b"123456789"), 0xE306_9283);
        assert_eq!(!crc32c_table(!0, b"123456789"), 0xE306_9283);
    }

    /// The instruction (three runs from `3 * CRC_BLOCK` bytes), the tables and the bit-by-bit CRC
    /// agree: every length, any split of the bytes.
    #[test]
    fn crc32c_paths_agree() {
        let data: Vec<u8> = (0..2400u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for len in 0..data.len() {
            let mut bit = !0u32;
            for &b in &data[..len] {
                bit ^= b as u32;
                for _ in 0..8 {
                    bit = if bit & 1 != 0 { (bit >> 1) ^ 0x82F6_3B78 } else { bit >> 1 };
                }
            }
            let bit = !bit;
            assert_eq!(crc32c(0, &data[..len]), bit, "len {len}");
            assert_eq!(!crc32c_table(!0, &data[..len]), bit, "tables, len {len}");
            let half = len / 3;
            assert_eq!(crc32c(crc32c(0, &data[..half]), &data[half..len]), bit, "split, len {len}");
        }
    }

    #[test]
    fn varint_roundtrip_small() {
        let mut buf = [0u8; 10];
        let n = varint_encode(127, &mut buf, 0);
        assert_eq!(n, 1);
        assert_eq!(buf[0], 0x7F);
        let (v, n2) = varint_decode(&buf, 0).unwrap();
        assert_eq!(v, 127);
        assert_eq!(n2, 1);
    }

    #[test]
    fn varint_roundtrip_boundary_128() {
        let mut buf = [0u8; 10];
        let n = varint_encode(128, &mut buf, 0);
        assert_eq!(n, 2);
        assert_eq!(buf[0], 0x80);
        assert_eq!(buf[1], 0x01);
        let (v, _) = varint_decode(&buf, 0).unwrap();
        assert_eq!(v, 128);
    }

    #[test]
    fn varint_roundtrip_u64_max() {
        let mut buf = [0u8; 10];
        let n = varint_encode(u64::MAX, &mut buf, 0);
        assert_eq!(n, 10);
        let (v, _) = varint_decode(&buf, 0).unwrap();
        assert_eq!(v, u64::MAX);
    }

    #[test]
    fn zig_zag_roundtrip() {
        for src in [0i64, 1, -1, 2, -2, i64::MAX, i64::MIN, 42, -42] {
            let enc = zig_zag_encode(src, 63);
            let dec = zig_zag_decode(enc);
            assert_eq!(dec, src, "roundtrip failed for {src}");
        }
    }

    #[test]
    fn uint_le_roundtrip() {
        let mut buf = [0u8; 8];
        write_uint_le(0xDEADBEEF_CAFEBABE, &mut buf, 0, 8);
        assert_eq!(read_uint_le(&buf, 0, 8), 0xDEADBEEF_CAFEBABE);
    }

    #[test]
    fn collection_helpers() {
        let mut v = vec![1, 2, 3];
        resize_fill(&mut v, 5, 9);
        assert_eq!(v, vec![1, 2, 3, 9, 9]);
        resize_fill(&mut v, 2, 0);
        assert_eq!(v, vec![1, 2]);
        assert_eq!(to_list(&[1, 2, 3, 4], 3), vec![1, 2, 3]);
        assert_eq!(to_list(&[1, 2], 10), vec![1, 2]);
        assert_eq!(size_array(7, 3), vec![7, 7, 7]);
        assert_eq!(size_list(0, 2), vec![0, 0]);
    }

    #[test]
    fn bits_as_reinterpret() {
        assert_eq!(bits_as::<i32>(0xFFFF_FFFF), -1);
        assert_eq!(bits_as::<u32>(42), 42);
        assert_eq!(bits_as::<f32>(0x3F80_0000), 1.0);
        assert_eq!(bits_as::<i16>(0xFFFF), -1);
    }

    #[test]
    fn hex_dump_format() {
        let src: Vec<u8> = (0..20u8).collect();
        let dump = hex_dump(&src);
        let mut lines = dump.lines();
        assert_eq!(lines.next().unwrap(), "       0: 00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F :       16");
        assert_eq!(lines.next().unwrap(), "      10: 10 11 12 13                                     :       20");
    }

    #[test]
    fn date_time_parses_exact() {
        let t = date_time("2024-03-05 12:30:45", "yyyy-MM-dd HH:mm:ss").unwrap();
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        // 2024-03-05 12:30:45 UTC
        assert_eq!(secs, 1709641845);
        let with_millis = date_time("1970-01-01T00:00:00.123", "yyyy-MM-ddTHH:mm:ss.fff").unwrap();
        assert_eq!(with_millis.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(), 123);
        assert!(date_time("2024/03/05", "yyyy-MM-dd").is_err());
        // the names of the months and of the days (InvariantCulture, any case); the day of the week
        // must be the date's
        assert_eq!(date_time("Tuesday, 05 mar 2024 12:30:45", "dddd, dd MMM yyyy HH:mm:ss").unwrap(), t);
        assert_eq!(date_time("Tue 5 March 2024 12:30:45", "ddd d MMMM yyyy HH:mm:ss").unwrap(), t);
        assert!(date_time("Wed 5 March 2024 12:30:45", "ddd d MMMM yyyy HH:mm:ss").is_err(), "not a Wednesday");
        // a day past the end of its month
        assert!(date_time("2023-02-29", "yyyy-MM-dd").is_err());
        assert!(date_time("2024-02-29", "yyyy-MM-dd").is_ok());
        assert!(date_time("2024-04-31", "yyyy-MM-dd").is_err());
        assert!(date_time("2024-13-05", "yyyy-MM-dd").is_err());
        let ms = |t: std::time::SystemTime| crate::lib::adhoc::helpers::unix_millis(t);
        // a 12-hour clock
        assert_eq!(ms(date_time("05.03.2024 1:30:45 PM", "dd.MM.yyyy h:mm:ss tt").unwrap()), 1709645445000);
        assert_eq!(ms(date_time("05.03.2024 12:30:45 AM", "dd.MM.yyyy hh:mm:ss tt").unwrap()), 1709598645000);
        // an offset: applied, the moment is the same
        assert_eq!(ms(date_time("2024-03-05T15:30:45+03:00", "yyyy-MM-dd'T'HH:mm:sszzz").unwrap()), 1709641845000);
        assert_eq!(ms(date_time("2024-03-05T12:30:45Z", "yyyy-MM-ddTHH:mm:ssK").unwrap()), 1709641845000);
        // seven digits of a second
        let t = date_time("1970-01-01 00:00:00.1234567", "yyyy-MM-dd HH:mm:ss.fffffff").unwrap();
        assert_eq!(t.duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(), 123_456_700);
        // before 1970, a year of two digits
        assert_eq!(ms(date_time("1969-12-31 23:59:59", "yyyy-MM-dd HH:mm:ss").unwrap()), -1000);
        assert_eq!(ms(date_time("24-3-5", "yy-M-d").unwrap()), 1709596800000);
        assert!(date_time("13:00 PM", "hh:mm tt").is_err(), "no hour 13 on a 12-hour clock");
    }
}
