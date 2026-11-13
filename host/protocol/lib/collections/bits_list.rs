//! Fixed-width integers packed into `u64` storage, least significant bits first.
//!
//! Each item occupies 1 to 8 bits. Setting beyond the end fills gaps with the configured default
//! value. A zero padding word permits reads across a storage-word boundary. Insertion and removal
//! shift words; unused bits stay zero for equality and hashing. Bulk operations process groups of
//! eight items, with BMI2 paths on supported x86_64 processors.

use super::keys::ByteKey;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::iter::FusedIterator;
use core::marker::PhantomData;

/// Items a bulk call reads or writes through the stack at a time.
const BULK: usize = 256;

#[inline(always)]
fn mask(n: u32) -> u64 {
    (1u64 << n) - 1
}

/// The groups of 8 items of `src` (a multiple of 8) packed into `B` bytes each from `p`: the width
/// a constant, the shifts are too - the loop unrolls. 8 bytes are written at once while that stays
/// inside the groups, the group's own bytes at the end.
#[cfg(target_endian = "little")]
#[inline(always)]
unsafe fn pack_groups<const B: usize, V: Copy>(p: *mut u8, src: &[V], f: &impl Fn(V) -> u8) {
    let end = src.len() / 8 * B;
    let m = mask(B as u32);
    for (g, c) in src.chunks_exact(8).enumerate() {
        let mut x = 0u64;
        for (k, &v) in c.iter().enumerate() {
            x |= (f(v) as u64 & m) << (k * B);
        }
        let q = p.add(g * B);
        if g * B + 8 <= end {
            q.cast::<u64>().write_unaligned(x.to_le());
        } else {
            core::ptr::copy_nonoverlapping(x.to_le_bytes().as_ptr(), q, B);
        }
    }
}

/// The low `B` bits of each byte of 8, gathered (`pext`) or spread (`pdep`).
#[cfg(target_arch = "x86_64")]
const fn byte_lanes(b: usize) -> u64 {
    0x0101_0101_0101_0101 * ((1u64 << b) - 1)
}

/// [`pack_groups`] by BMI2: a group's 8 items as 8 bytes, their low `B` bits gathered by one
/// `pext`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
unsafe fn pack_groups_bmi2<const B: usize, V: Copy>(p: *mut u8, src: &[V], f: &impl Fn(V) -> u8) {
    let end = src.len() / 8 * B;
    for (g, c) in src.chunks_exact(8).enumerate() {
        let bytes = [f(c[0]), f(c[1]), f(c[2]), f(c[3]), f(c[4]), f(c[5]), f(c[6]), f(c[7])];
        let x = core::arch::x86_64::_pext_u64(u64::from_le_bytes(bytes), byte_lanes(B));
        let q = p.add(g * B);
        if g * B + 8 <= end {
            q.cast::<u64>().write_unaligned(x.to_le());
        } else {
            core::ptr::copy_nonoverlapping(x.to_le_bytes().as_ptr(), q, B);
        }
    }
}

/// [`unpack_groups`] by BMI2: a group's `8 * B` bits spread to the low bits of 8 bytes by one
/// `pdep`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
unsafe fn unpack_groups_bmi2<const B: usize, V>(p: *const u8, dst: &mut [V], f: &impl Fn(u8) -> V) {
    for (g, c) in dst.chunks_exact_mut(8).enumerate() {
        let x = u64::from_le(p.add(g * B).cast::<u64>().read_unaligned());
        let bytes = core::arch::x86_64::_pdep_u64(x, byte_lanes(B)).to_le_bytes();
        for (d, &b) in c.iter_mut().zip(&bytes) {
            *d = f(b);
        }
    }
}

#[cfg(all(target_endian = "little", not(target_arch = "x86_64")))]
#[inline(always)]
unsafe fn pack_groups_bmi2<const B: usize, V: Copy>(p: *mut u8, src: &[V], f: &impl Fn(V) -> u8) {
    pack_groups::<B, V>(p, src, f)
}

#[cfg(all(target_endian = "little", not(target_arch = "x86_64")))]
#[inline(always)]
unsafe fn unpack_groups_bmi2<const B: usize, V>(p: *const u8, dst: &mut [V], f: &impl Fn(u8) -> V) {
    unpack_groups::<B, V>(p, dst, f)
}

/// Tests turn BMI2 off to run the plain kernels too.
#[cfg(test)]
pub(crate) static NO_BMI2: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn bmi2_off() -> bool {
    #[cfg(test)]
    return NO_BMI2.load(core::sync::atomic::Ordering::Relaxed);
    #[cfg(not(test))]
    false
}

/// `$plain::<B>` or, where the CPU has BMI2, `$bmi2::<B>` for the width `$b` (1..=8); 8 is a plain
/// copy either way.
#[cfg(target_endian = "little")]
macro_rules! by_width {
    ($b:expr, $plain:ident, $bmi2:ident, $($arg:expr),*) => {{
        #[cfg(target_arch = "x86_64")]
        let bmi2 = std::is_x86_feature_detected!("bmi2") && !bmi2_off();
        #[cfg(not(target_arch = "x86_64"))]
        let bmi2 = false;
        match $b {
            1 => if bmi2 { $bmi2::<1, _>($($arg),*) } else { $plain::<1, _>($($arg),*) },
            2 => if bmi2 { $bmi2::<2, _>($($arg),*) } else { $plain::<2, _>($($arg),*) },
            3 => if bmi2 { $bmi2::<3, _>($($arg),*) } else { $plain::<3, _>($($arg),*) },
            4 => if bmi2 { $bmi2::<4, _>($($arg),*) } else { $plain::<4, _>($($arg),*) },
            5 => if bmi2 { $bmi2::<5, _>($($arg),*) } else { $plain::<5, _>($($arg),*) },
            6 => if bmi2 { $bmi2::<6, _>($($arg),*) } else { $plain::<6, _>($($arg),*) },
            7 => if bmi2 { $bmi2::<7, _>($($arg),*) } else { $plain::<7, _>($($arg),*) },
            _ => $plain::<8, _>($($arg),*),
        }
    }};
}

/// The groups of 8 items packed `B` bytes each from `p` into `dst` (a multiple of 8): one 8-byte
/// load and 8 constant shifts each.
#[cfg(target_endian = "little")]
#[inline(always)]
unsafe fn unpack_groups<const B: usize, V>(p: *const u8, dst: &mut [V], f: &impl Fn(u8) -> V) {
    let m = mask(B as u32) as u8;
    for (g, c) in dst.chunks_exact_mut(8).enumerate() {
        let x = u64::from_le(p.add(g * B).cast::<u64>().read_unaligned());
        for (k, d) in c.iter_mut().enumerate() {
            *d = f((x >> (k * B)) as u8 & m);
        }
    }
}

/// From this many items a kernel over an array of a pack is worth its dispatch.
#[cfg(target_endian = "little")]
pub(crate) const KERNEL_ITEMS: i32 = 32;

/// The items of `src` (a multiple of 8), `b` bits each, packed from `p`.
#[cfg(target_endian = "little")]
#[inline(never)]
pub(crate) unsafe fn pack_items<V: Copy>(b: usize, p: *mut u8, src: &[V], f: &impl Fn(V) -> u8) {
    by_width!(b, pack_groups, pack_groups_bmi2, p, src, f)
}

/// The items of `dst` (a multiple of 8), `b` bits each, unpacked from `p`.
#[cfg(target_endian = "little")]
#[inline(never)]
pub(crate) unsafe fn unpack_items<V>(b: usize, p: *const u8, dst: &mut [V], f: &impl Fn(u8) -> V) {
    by_width!(b, unpack_groups, unpack_groups_bmi2, p, dst, f)
}

pub struct BitsList<T = u8> {
    /// `words.len()` is the storage: zeroed, at least one word past the last item when not empty.
    words: Vec<u64>,
    len: usize,
    bits: u32,
    /// Masked to `bits`.
    default_value: u8,
    _item: PhantomData<fn() -> T>,
}

impl<T> BitsList<T> {
    /// An empty list of `bits_per_item`-bit items (1..=8) that fills gaps with 0. Does not
    /// allocate.
    pub fn new(bits_per_item: u32) -> Self {
        assert!((1..=8).contains(&bits_per_item), "bits per item must be 1..=8");
        Self { words: Vec::new(), len: 0, bits: bits_per_item, default_value: 0, _item: PhantomData }
    }

    /// An empty list with room for `items` items.
    pub fn with_capacity(bits_per_item: u32, items: usize) -> Self {
        let mut l = Self::new(bits_per_item);
        l.reserve(items);
        l
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub fn bits_per_item(&self) -> u32 {
        self.bits
    }

    /// Items storable without reallocation.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.words.len().saturating_sub(1) * 64 / self.bits as usize
    }

    /// Storage words for `items` items, the padding word included.
    #[inline(always)]
    fn words_for(&self, items: usize) -> usize {
        (items * self.bits as usize).div_ceil(64) + 1
    }

    /// Makes room for `additional` more items.
    pub fn reserve(&mut self, additional: usize) {
        let need = self.words_for(self.len + additional);
        if need > self.words.len() {
            let grown = self.words.len() + self.words.len() / 2;
            self.words.resize(need.max(grown), 0);
        }
    }

    /// Frees the storage past the last item.
    pub fn shrink_to_fit(&mut self) {
        if self.len == 0 {
            self.words = Vec::new();
        } else {
            self.words.truncate(self.words_for(self.len));
            self.words.shrink_to_fit();
        }
    }

    /// The raw value of item `i`, unchecked against `len`.
    #[inline(always)]
    fn get_raw(&self, i: usize) -> u8 {
        let bit = i * self.bits as usize;
        #[cfg(target_endian = "little")]
        {
            // An item of at most 8 bits from bit `bit & 7` of a byte lies in two bytes: one
            // unaligned `u16` load. SAFETY: `i < len` and the padding word make byte `bit / 8 + 1`
            // part of the storage.
            let x = unsafe { (self.words.as_ptr() as *const u8).add(bit >> 3).cast::<u16>().read_unaligned() };
            (x >> (bit & 7)) as u8 & mask(self.bits) as u8
        }
        #[cfg(not(target_endian = "little"))]
        {
            let (w, s) = (bit >> 6, bit & 63);
            // SAFETY: `i < len` and the padding word make `w + 1` a valid index.
            let x = unsafe { (*self.words.get_unchecked(w + 1) as u128) << 64 | *self.words.get_unchecked(w) as u128 };
            (x >> s) as u8 & mask(self.bits) as u8
        }
    }

    /// Writes item `i` (< storage), any `v` (masked).
    #[inline(always)]
    fn set_raw(&mut self, i: usize, v: u8) {
        let bit = i * self.bits as usize;
        debug_assert!((bit >> 6) + 1 < self.words.len(), "item {i} past the storage");
        // Widths up to four bits use one or two bytes. Wider items use aligned word accesses,
        // including a second word when they cross a boundary, to avoid overlapping narrow writes
        // followed by partially overlapping reads.
        #[cfg(target_endian = "little")]
        if self.bits <= 4 {
            let (s, m) = (bit & 7, mask(self.bits) as u16);
            // SAFETY: the item is in the storage and the padding word follows it: bytes `bit / 8`
            // and `bit / 8 + 1` are ours
            unsafe {
                let p = (self.words.as_mut_ptr() as *mut u8).add(bit >> 3);
                if s + self.bits as usize <= 8 {
                    *p = *p & !((m as u8) << s) | (v & m as u8) << s;
                } else {
                    let p = p.cast::<u16>();
                    p.write_unaligned(p.read_unaligned() & !(m << s) | (v as u16 & m) << s);
                }
            }
            return;
        }
        let (k, s, m) = (bit >> 6, bit & 63, mask(self.bits));
        let v = v as u64 & m;
        // SAFETY: the item is in the storage and the padding word follows it: words `k` and `k + 1`
        // are ours
        unsafe {
            let w = self.words.get_unchecked_mut(k);
            *w = *w & !(m << s) | v << s;
            if s + self.bits as usize > 64 {
                let w = self.words.get_unchecked_mut(k + 1);
                *w = *w & !(m >> (64 - s)) | v >> (64 - s);
            }
        }
    }

    /// Items `[from, from + src.len())` from `src` through `f` (the storage holds them). The items
    /// before the first whole group of 8 and after the last one go one by one; a group is `bits`
    /// bytes, built as one word in a register and written at once - 8 bytes while that stays inside
    /// the groups (the next group writes over the rest), its own bytes only at the end.
    fn write_items<V: Copy>(&mut self, from: usize, src: &[V], f: impl Fn(V) -> u8) {
        let head = ((8 - (from & 7)) & 7).min(src.len());
        for (k, &v) in src[..head].iter().enumerate() {
            self.set_raw(from + k, f(v));
        }
        let groups = &src[head..head + (src.len() - head) / 8 * 8];
        #[cfg(target_endian = "little")]
        if !groups.is_empty() {
            let b = self.bits as usize;
            // SAFETY: the groups are items in the storage; every write stays inside their bytes
            unsafe {
                let p = (self.words.as_mut_ptr() as *mut u8).add((from + head) * b / 8);
                by_width!(b, pack_groups, pack_groups_bmi2, p, groups, &f)
            }
        }
        #[cfg(not(target_endian = "little"))]
        for (k, &v) in groups.iter().enumerate() {
            self.set_raw(from + head + k, f(v));
        }
        let done = head + groups.len();
        for (k, &v) in src[done..].iter().enumerate() {
            self.set_raw(from + done + k, f(v));
        }
    }

    /// Items `[from, from + dst.len())` (`< len`) into `dst` through `f`: the items before the
    /// first whole group of 8 and after the last one by one; a group - `bits` bytes - by one 8-byte
    /// load and 8 shifts.
    fn read_items<V>(&self, from: usize, dst: &mut [V], f: impl Fn(u8) -> V) {
        let head = ((8 - (from & 7)) & 7).min(dst.len());
        for (k, d) in dst[..head].iter_mut().enumerate() {
            *d = f(self.get_raw(from + k));
        }
        let gl = (dst.len() - head) / 8 * 8;
        #[cfg(target_endian = "little")]
        {
            let b = self.bits as usize;
            let dst = &mut dst[head..head + gl];
            // SAFETY: an 8-byte load from a group's first byte stays in the storage: the padding
            // word follows the items
            unsafe {
                let p = (self.words.as_ptr() as *const u8).add((from + head) * b / 8);
                by_width!(b, unpack_groups, unpack_groups_bmi2, p, dst, &f)
            }
        }
        #[cfg(not(target_endian = "little"))]
        for (k, d) in dst[head..head + gl].iter_mut().enumerate() {
            *d = f(self.get_raw(from + head + k));
        }
        let done = head + gl;
        for (k, d) in dst[done..].iter_mut().enumerate() {
            *d = f(self.get_raw(from + done + k));
        }
    }

    /// The first (`last`: the last) item equal to `v`: pieces of the items read a group of 8 at a
    /// time into bytes on the stack, 32 bytes compared at once into a mask.
    #[inline(never)]
    fn find(&self, v: u8, last: bool) -> Option<usize> {
        let mut buf = [0u8; BULK];
        let mut at = if last { self.len.saturating_sub(BULK) } else { 0 };
        loop {
            let k = (self.len - at).min(BULK);
            let b = &mut buf[..k];
            self.read_items(at, b, |b| b);
            let m = |c: &[u8]| c.iter().enumerate().fold(0u32, |m, (i, &b)| m | ((b == v) as u32) << i);
            if last {
                for (j, c) in b.chunks(32).enumerate().rev() {
                    let m = m(c);
                    if m != 0 {
                        return Some(at + j * 32 + 31 - m.leading_zeros() as usize);
                    }
                }
                if at == 0 {
                    return None;
                }
                at = at.saturating_sub(BULK);
            } else {
                for (j, c) in b.chunks(32).enumerate() {
                    let m = m(c);
                    if m != 0 {
                        return Some(at + j * 32 + m.trailing_zeros() as usize);
                    }
                }
                at += k;
                if at == self.len {
                    return None;
                }
            }
        }
    }

    /// Clears the bits of items `[from, len)`: keeps "zero past the end" before `len` shrinks.
    fn clear_from(&mut self, from: usize) {
        if from >= self.len {
            return;
        }
        let bit = from * self.bits as usize;
        let w = bit >> 6;
        self.words[w] &= mask((bit & 63) as u32);
        let end = self.words_for(self.len).min(self.words.len());
        self.words[w + 1..end].fill(0);
    }

    /// Shortens the list to `len` items.
    pub fn truncate(&mut self, len: usize) {
        self.clear_from(len);
        self.len = self.len.min(len);
    }

    /// Removes all items, keeping the storage.
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// The `items` items from `item` on as one word (`items * bits <= 64`), item `item` lowest:
    /// exactly how they travel.
    pub fn raw(&self, item: usize, items: usize) -> u64 {
        let n = items * self.bits as usize;
        assert!(n <= 64 && item + items <= self.len, "raw range out of bounds");
        if n == 0 {
            return 0;
        }
        let bit = item * self.bits as usize;
        let (w, s) = (bit >> 6, bit & 63);
        let x = (self.words[w + 1] as u128) << 64 | self.words[w] as u128;
        let v = (x >> s) as u64;
        if n == 64 {
            v
        } else {
            v & ((1u64 << n) - 1)
        }
    }

    /// Writes `items` items from `item` on from the low `items * bits` bits of `src`; the list
    /// grows to `item + items` if needed (the gap before `item` is zeros, not the default).
    pub fn set_raw_items(&mut self, item: usize, items: usize, src: u64) {
        let n = items * self.bits as usize;
        assert!(n <= 64, "more than 64 bits");
        if n == 0 {
            return;
        }
        let end = item + items;
        if end > self.len {
            self.reserve(end - self.len);
            self.len = end;
        }
        let m: u128 = if n == 64 { u64::MAX as u128 } else { ((1u64 << n) - 1) as u128 };
        let bit = item * self.bits as usize;
        let (w, s) = (bit >> 6, bit & 63);
        let x = (self.words[w + 1] as u128) << 64 | self.words[w] as u128;
        let x = x & !(m << s) | (src as u128 & m) << s;
        self.words[w] = x as u64;
        self.words[w + 1] = (x >> 64) as u64;
    }

    /// The bytes holding items `[item, item + items)`, native byte order; meaningful when
    /// `item * bits` and `items * bits` are multiples of 8.
    pub fn bytes(&self, item: usize, items: usize) -> &[u8] {
        let (from, n) = ((item * self.bits as usize) >> 3, (items * self.bits as usize) >> 3);
        let all = unsafe { core::slice::from_raw_parts(self.words.as_ptr() as *const u8, self.words.len() * 8) };
        &all[from..from + n]
    }

    /// As [`bytes`](Self::bytes), writable; the list grows to `item + items` first.
    pub fn bytes_mut(&mut self, item: usize, items: usize) -> &mut [u8] {
        let end = item + items;
        if end > self.len {
            self.reserve(end - self.len);
            self.len = end;
        }
        let (from, n) = ((item * self.bits as usize) >> 3, (items * self.bits as usize) >> 3);
        let all = unsafe { core::slice::from_raw_parts_mut(self.words.as_mut_ptr() as *mut u8, self.words.len() * 8) };
        &mut all[from..from + n]
    }

    /// Inserts a gap of one item at `i` (< len): bits from `i * bits` on move up by `bits`. The
    /// storage must hold `len + 1` items; the bits shifted into the padding word are zeros.
    fn shift_up(&mut self, i: usize) {
        let n = self.bits;
        let b0 = i * n as usize;
        let (w0, s0) = (b0 >> 6, (b0 & 63) as u32);
        let end = self.words_for(self.len + 1);
        let w = &mut self.words;
        for k in (w0 + 1..end).rev() {
            w[k] = w[k] << n | w[k - 1] >> (64 - n);
        }
        let x = w[w0];
        let keep = mask(s0);
        w[w0] = x & keep | (x & !keep) << n;
    }

    /// Removes item `i`: bits from `(i + 1) * bits` on move down by `bits`.
    fn shift_down(&mut self, i: usize) {
        let n = self.bits;
        let b0 = i * n as usize;
        let (w0, s0) = (b0 >> 6, (b0 & 63) as u32);
        let end = self.words_for(self.len).min(self.words.len());
        let w = &mut self.words;
        let keep = mask(s0);
        let x = w[w0];
        w[w0] = x & keep | (x >> n | w[w0 + 1] << (64 - n)) & !keep;
        for k in w0 + 1..end - 1 {
            w[k] = w[k] >> n | w[k + 1] << (64 - n);
        }
        if end - 1 > w0 {
            w[end - 1] >>= n;
        }
    }
}

impl<T: ByteKey> BitsList<T> {
    /// An empty list that fills gaps with `default_value`.
    pub fn with_default(bits_per_item: u32, default_value: T) -> Self {
        let mut l = Self::new(bits_per_item);
        l.default_value = default_value.to_byte() & mask(bits_per_item) as u8;
        l
    }

    /// `len` copies of `value`, which also fills later gaps.
    pub fn filled(bits_per_item: u32, value: T, len: usize) -> Self {
        let mut l = Self::with_default(bits_per_item, value);
        l.set_len(len);
        l
    }

    #[inline]
    pub fn default_value(&self) -> T {
        T::from_byte(self.default_value)
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<T> {
        (i < self.len).then(|| T::from_byte(self.get_raw(i)))
    }

    #[inline]
    pub fn last(&self) -> Option<T> {
        self.len.checked_sub(1).map(|i| T::from_byte(self.get_raw(i)))
    }

    /// Fills items `[from, to)` with the default value (a no-op for a zero default).
    fn fill_default(&mut self, from: usize, to: usize) {
        if self.default_value == 0 || from >= to {
            return;
        }
        if to - from <= 8 {
            for i in from..to {
                self.set_raw(i, self.default_value);
            }
            return;
        }

        // Repeat enough whole items to cover a word at any phase of the item width.
        let mut pattern = self.default_value as u128;
        let mut period = self.bits;
        while period < 64 {
            pattern |= pattern << period;
            period *= 2;
        }
        let bits = self.bits as usize;
        let (start, end) = (from * bits, to * bits);
        for w in start / 64..=(end - 1) / 64 {
            let base = w * 64;
            let lo = start.saturating_sub(base);
            let hi = (end - base).min(64);
            let range_mask = (u64::MAX >> (64 - (hi - lo))) << lo;
            let value = (pattern >> (base % bits)) as u64;
            self.words[w] = self.words[w] & !range_mask | value & range_mask;
        }
    }

    /// Sets item `i`, keeping the low `bits` of the value; past the end the list grows to `i + 1`,
    /// the gap filled with the default value.
    #[inline]
    pub fn set(&mut self, i: usize, value: T) {
        if i >= self.len {
            self.grow_to(i);
        }
        self.set_raw(i, value.to_byte());
    }

    /// [`set`](Self::set) past the end: the list grows to `i + 1`, the gap filled with the default
    /// value.
    #[cold]
    #[inline(never)]
    fn grow_to(&mut self, i: usize) {
        self.reserve(i + 1 - self.len);
        self.fill_default(self.len, i);
        self.len = i + 1;
    }

    /// Sets items from `from` on to `values`.
    pub fn set_slice(&mut self, from: usize, values: &[T]) {
        if values.is_empty() {
            return;
        }
        let end = from + values.len();
        if end > self.len {
            self.reserve(end - self.len);
            self.fill_default(self.len, from);
            self.len = end;
        }
        self.write_items(from, values, |v: T| v.to_byte());
    }

    /// Appends `values` (`Vec::extend_from_slice`): a group of 8 items at a time.
    pub fn push_slice(&mut self, values: &[T]) {
        let from = self.len;
        self.reserve(values.len());
        self.len += values.len();
        self.write_items(from, values, |v: T| v.to_byte());
    }

    /// Items from `from` on into `dst` (`[T]::copy_from_slice`): a group of 8 items at a time.
    ///
    /// # Panics
    ///
    /// If `from + dst.len()` is past `len`.
    pub fn get_slice(&self, from: usize, dst: &mut [T]) {
        assert!(from + dst.len() <= self.len, "items {from}..{} past the length {}", from + dst.len(), self.len);
        self.read_items(from, dst, T::from_byte);
    }

    #[inline]
    pub fn push(&mut self, value: T) {
        let bit = self.len * self.bits as usize;
        let (k, s) = (bit >> 6, bit & 63);
        // the item's word and the padding word after it (`words_for(len + 1)` is at most that)
        if self.words.len() < k + 2 {
            self.words = Self::grown(core::mem::take(&mut self.words), k + 2);
        }
        // The bits past the end are zero: an OR into the aligned word, and into the next one when
        // the item crosses. Always the whole word at its own address: the next push reads what this
        // one wrote, passed on whole - bytes and `u16` written one after another half overlap, and
        // a half-overlapping read waits for the write to reach the cache.
        let v = value.to_byte() as u64 & mask(self.bits);
        // SAFETY: words `k` and `k + 1` are in the storage
        unsafe {
            *self.words.get_unchecked_mut(k) |= v << s;
            if s + self.bits as usize > 64 {
                *self.words.get_unchecked_mut(k + 1) |= v >> (64 - s);
            }
        }
        self.len += 1;
    }

    /// `words` grown to at least `need` (by half at least), with zeros. By value: a call given
    /// `&mut self` would make the compiler reload the fields from memory on every push of a loop.
    #[cold]
    #[inline(never)]
    fn grown(mut words: Vec<u64>, need: usize) -> Vec<u64> {
        let n = need.max(words.len() + words.len() / 2);
        words.resize(n, 0);
        words
    }

    pub fn pop(&mut self) -> Option<T> {
        let v = self.last()?;
        self.truncate(self.len - 1);
        Some(v)
    }

    /// Inserts `value` at `i`, moving the items from `i` on up by one; past the end the list grows
    /// to `i + 1` as `set` does.
    pub fn insert(&mut self, i: usize, value: T) {
        if i >= self.len {
            self.set(i, value);
            return;
        }
        self.reserve(1);
        self.shift_up(i);
        self.len += 1;
        self.set_raw(i, value.to_byte());
    }

    /// Removes item `i`, moving the items above down by one.
    ///
    /// # Panics
    ///
    /// If `i >= len`.
    pub fn remove(&mut self, i: usize) -> T {
        assert!(i < self.len, "index {i} out of range for length {}", self.len);
        let v = T::from_byte(self.get_raw(i));
        if i + 1 == self.len {
            self.set_raw(i, 0);
        } else {
            self.shift_down(i);
        }
        self.len -= 1;
        v
    }

    /// Sets the length: shortens, or grows with the default value.
    pub fn set_len(&mut self, len: usize) {
        if len <= self.len {
            self.truncate(len);
            return;
        }
        self.reserve(len - self.len);
        self.fill_default(self.len, len);
        self.len = len;
    }

    /// Short lists item by item, longer ones by [`find`](Self::find) (faster from about 20 items).
    #[inline]
    pub fn index_of(&self, value: T) -> Option<usize> {
        let v = value.to_byte() & mask(self.bits) as u8;
        if self.len < 20 {
            return (0..self.len).find(|&i| self.get_raw(i) == v);
        }
        self.find(v, false)
    }

    #[inline]
    pub fn last_index_of(&self, value: T) -> Option<usize> {
        let v = value.to_byte() & mask(self.bits) as u8;
        if self.len < 20 {
            return (0..self.len).rev().find(|&i| self.get_raw(i) == v);
        }
        self.find(v, true)
    }

    #[inline]
    pub fn contains(&self, value: T) -> bool {
        self.index_of(value).is_some()
    }

    /// Keeps only the items for which `f` returns `true`, in order; one pass.
    pub fn retain<F: FnMut(T) -> bool>(&mut self, mut f: F) {
        let mut kept = 0;
        for i in 0..self.len {
            let v = self.get_raw(i);
            if f(T::from_byte(v)) {
                if kept != i {
                    self.set_raw(kept, v);
                }
                kept += 1;
            }
        }
        self.truncate(kept);
    }

    /// Removes every item equal to `value`; returns how many.
    pub fn remove_all(&mut self, value: T) -> usize {
        let before = self.len;
        let m = mask(self.bits) as u8;
        let v = value.to_byte() & m;
        self.retain(|x| x.to_byte() & m != v);
        before - self.len
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, T> {
        Iter { list: self, pos: 0, end: self.len }
    }
}

impl<T> Clone for BitsList<T> {
    fn clone(&self) -> Self {
        Self { words: self.words.clone(), len: self.len, bits: self.bits, default_value: self.default_value, _item: PhantomData }
    }
}

/// Equal widths and items; the default value does not count.
impl<T> PartialEq for BitsList<T> {
    fn eq(&self, other: &Self) -> bool {
        if self.bits != other.bits || self.len != other.len {
            return false;
        }
        let n = (self.len * self.bits as usize).div_ceil(64);
        self.words[..n] == other.words[..n]
    }
}

impl<T> Eq for BitsList<T> {}

impl<T> Hash for BitsList<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bits.hash(state);
        self.len.hash(state);
        let n = (self.len * self.bits as usize).div_ceil(64);
        self.words[..n].hash(state);
    }
}

impl<T: ByteKey + fmt::Debug> fmt::Debug for BitsList<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: ByteKey> Extend<T> for BitsList<T> {
    /// A piece of the items at a time onto the stack (a plain copy the compiler vectorizes), then
    /// written a group of 8 at a time.
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let mut it = iter.into_iter();
        let mut buf = [0u8; BULK];
        loop {
            let mut n = 0;
            for (slot, v) in buf.iter_mut().zip(&mut it) {
                *slot = v.to_byte();
                n += 1;
            }
            let from = self.len;
            self.reserve(n);
            self.len += n;
            self.write_items(from, &buf[..n], |b| b);
            if n < BULK {
                return;
            }
        }
    }
}

// iterator

#[derive(Clone)]
pub struct Iter<'a, T> {
    list: &'a BitsList<T>,
    pos: usize,
    end: usize,
}

impl<T: ByteKey> Iterator for Iter<'_, T> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        if self.pos == self.end {
            return None;
        }
        self.pos += 1;
        Some(T::from_byte(self.list.get_raw(self.pos - 1)))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.end - self.pos;
        (n, Some(n))
    }

    /// Pieces of the items read a group of 8 at a time into bytes on the stack, folded as a `[u8]`
    /// - the loop the compiler vectorizes.
    fn fold<B, F: FnMut(B, T) -> B>(self, mut acc: B, mut f: F) -> B {
        let mut buf = [0u8; BULK];
        let mut at = self.pos;
        while at < self.end {
            let k = (self.end - at).min(BULK);
            self.list.read_items(at, &mut buf[..k], |b| b);
            acc = buf[..k].iter().fold(acc, |a, &b| f(a, T::from_byte(b)));
            at += k;
        }
        acc
    }
}

impl<T: ByteKey> DoubleEndedIterator for Iter<'_, T> {
    #[inline]
    fn next_back(&mut self) -> Option<T> {
        if self.pos == self.end {
            return None;
        }
        self.end -= 1;
        Some(T::from_byte(self.list.get_raw(self.end)))
    }
}

impl<T: ByteKey> ExactSizeIterator for Iter<'_, T> {}
impl<T: ByteKey> FusedIterator for Iter<'_, T> {}

impl<'a, T: ByteKey> IntoIterator for &'a BitsList<T> {
    type Item = T;
    type IntoIter = Iter<'a, T>;
    #[inline]
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}
