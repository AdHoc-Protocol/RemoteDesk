//! Growable bit list with a counted leading run of ones and implicit trailing zeros.
//!
//! Bits below `toc` are represented by the count. Remaining full words live in `words`, followed by
//! an inline `tail` word. Bits past the stored words are zero. Appending can extend the run or fill
//! the tail without allocating; full tail words move into the vector.
//!
//! Insertion and removal from the run's last word on shift `words` and `tail` in place; other
//! operations that shift bits materialize the affected words and rebuild the run. Equality and
//! hashing use the logical bits, independent of unused trailing words. Indices are `usize`;
//! unsuccessful searches return `None`. Forward searches include their starting index, backward
//! searches exclude it, and `rank` counts ones through its index inclusively.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::iter::FusedIterator;
use core::ops::{Index, Range};

#[inline(always)]
fn mask(n: usize) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

// word-vector primitives
//
// A word vector holds bits LSB first: bit `r` is `w[r / 64] >> (r % 64) & 1`.

/// Inserts bit `v` at `r`; the bits from `r` on move up by one, the top bit of the last word drops
/// out (the caller leaves room).
fn insert_bit(w: &mut [u64], r: usize, v: bool) {
    let (i, b) = (r / 64, r % 64);
    let x = w[i];
    let mut carry = x >> 63;
    w[i] = x & mask(b) | (x & !mask(b)) << 1 | (v as u64) << b;
    for y in &mut w[i + 1..] {
        let top = *y >> 63;
        *y = *y << 1 | carry;
        carry = top;
    }
}

/// Removes bit `r` and returns it; the bits above move down by one, a zero comes in at the top.
fn remove_bit(w: &mut [u64], r: usize) -> bool {
    let (i, b) = (r / 64, r % 64);
    if i >= w.len() {
        return false;
    }
    let x = w[i];
    let m = mask(b);
    let mut cur = x & m | x >> 1 & !m;
    for j in i + 1..w.len() {
        let y = w[j];
        w[j - 1] = cur | y << 63;
        cur = y >> 1;
    }
    let last = w.len() - 1;
    w[last] = cur;
    x >> b & 1 != 0
}

#[derive(Clone, Copy)]
enum Fill {
    Zeros,
    Ones,
    Flip,
}

/// Applies `op` to bits `[from, to)`; grows `w` for ones and flips. May leave a zero last word.
fn fill_words(w: &mut Vec<u64>, from: usize, to: usize, op: Fill) {
    if from >= to {
        return;
    }
    let to = match op {
        Fill::Zeros => to.min(w.len() * 64),
        _ => {
            let need = (to - 1) / 64 + 1;
            if w.len() < need {
                w.resize(need, 0);
            }
            to
        }
    };
    if from >= to {
        return;
    }
    let (fi, li) = (from / 64, (to - 1) / 64);
    for (i, x) in w[fi..=li].iter_mut().enumerate() {
        let lo = if i == 0 { from % 64 } else { 0 };
        let hi = if fi + i == li { (to - 1) % 64 + 1 } else { 64 };
        let m = mask(hi - lo) << lo;
        match op {
            Fill::Zeros => *x &= !m,
            Fill::Ones => *x |= m,
            Fill::Flip => *x ^= m,
        }
    }
}

#[inline]
fn trim(w: &mut Vec<u64>) {
    while let Some(&0) = w.last() {
        w.pop();
    }
}

// BitList

#[derive(Clone, Default)]
pub struct BitList {
    /// The logical number of bits.
    len: usize,
    /// Bits `[0, toc)` are ones; the run may go on past it in `words` (`push` does not count it).
    /// `toc <= len`.
    toc: usize,
    /// The words from word `toc / 64` on, each at its place: `words[k]` holds bits
    /// `[(toc / 64 + k) * 64...)`; the first of them keeps the ones of the run below `toc` too.
    /// Bits from `len` on are 0. Trailing zero words may stay.
    /// `toc / 64 + words.len() <= len / 64`: they end at the word of the end or before it.
    words: Vec<u64>,
    /// Word `toc / 64 + words.len()`, right after `words`; past it all is 0. `push` writes here
    /// while the end is in it.
    tail: u64,
}

impl BitList {
    /// Does not allocate.
    #[inline]
    pub const fn new() -> Self {
        Self {
            len: 0,
            toc: 0,
            words: Vec::new(),
            tail: 0,
        }
    }

    /// An empty list with room for `bits` bits.
    pub fn with_capacity(bits: usize) -> Self {
        Self {
            len: 0,
            toc: 0,
            words: Vec::with_capacity(bits / 64),
            tail: 0,
        }
    }

    /// `len` copies of `value`: no allocation either way.
    pub fn filled(value: bool, len: usize) -> Self {
        if value {
            Self {
                len,
                toc: len,
                words: Vec::new(),
                tail: mask(len & 63),
            }
        } else {
            Self { len, ..Self::new() }
        }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bits addressable without reallocation: the run plus the allocated words and the tail.
    #[inline]
    pub fn capacity(&self) -> usize {
        (self.base() << 6) + (self.words.capacity() + 1) * 64
    }

    /// The first word `words` keeps: the word `toc` is in.
    #[inline(always)]
    fn base(&self) -> usize {
        self.toc >> 6
    }

    /// Word `a` of the list: ones below `words`, then `words`, `tail`, zeros.
    #[inline]
    fn word(&self, a: usize) -> u64 {
        let base = self.base();
        if a < base {
            return u64::MAX;
        }
        let k = a - base;
        match self.words.get(k) {
            Some(&w) => w,
            None if k == self.words.len() => self.tail,
            None => 0,
        }
    }

    /// Place `k` of `words` and `tail` (`k <= words.len()`).
    #[inline(always)]
    fn at(&self, k: usize) -> u64 {
        if k < self.words.len() {
            self.words[k]
        } else {
            self.tail
        }
    }

    /// Place `k` of `words` and `tail`, `None` past `tail`.
    #[inline(always)]
    fn at_opt(&self, k: usize) -> Option<u64> {
        if k <= self.words.len() {
            Some(self.at(k))
        } else {
            None
        }
    }

    /// One past the word of `tail`: from there on all words are zeros.
    #[inline]
    fn words_end(&self) -> usize {
        self.base() + self.words.len() + 1
    }

    /// Place `k` of `words` and `tail` for writing; past `tail` `words` grows to it with zeros
    /// (`toc / 64 + k <= len / 64`).
    fn word_mut(&mut self, k: usize) -> &mut u64 {
        if k > self.words.len() {
            self.words.push(self.tail);
            self.words.resize(k, 0);
            self.tail = 0;
        }
        if k < self.words.len() {
            &mut self.words[k]
        } else {
            &mut self.tail
        }
    }

    /// Makes `tail` the word the end is in: past implicit zeros `words` grows to it.
    #[inline]
    fn open_end(&mut self) {
        let k = (self.len >> 6) - self.base();
        if k != self.words.len() {
            self.word_mut(k);
        }
    }

    /// The list taken apart: the words from word `a0` (`<= toc / 64`) to `tail`, the ones of the
    /// run below `words` filled in. [`install`](Self::install) puts it back. The vector is `words`'
    /// own when `a0` is `toc / 64`.
    fn take(&mut self, a0: usize) -> Vec<u64> {
        let pre = self.base() - a0;
        let mut ex = core::mem::take(&mut self.words);
        ex.push(self.tail);
        if pre > 0 {
            ex.splice(0..0, core::iter::repeat_n(u64::MAX, pre));
        }
        ex
    }

    /// Puts the list back from `ex`, the words from word `a0` on: below them all ones, from `len`
    /// on all zeros. The run is found anew - its full words leave `ex` - and the last word that is
    /// not zero becomes `tail`.
    fn install(&mut self, a0: usize, mut ex: Vec<u64>) {
        let mut k = 0;
        while k < ex.len() && ex[k] == u64::MAX {
            k += 1;
        }
        let ones = ((a0 + k) << 6) + ex.get(k).map_or(0, |w| w.trailing_ones() as usize);
        self.toc = ones.min(self.len);
        let drop = (self.base() - a0).min(ex.len());
        ex.drain(..drop);
        trim(&mut ex);
        self.tail = ex.pop().unwrap_or(0);
        self.words = ex;
    }

    // reading

    /// The bit at `bit`; `false` beyond the end.
    #[inline]
    pub fn get(&self, bit: usize) -> bool {
        // In `words`: one compare. Below them `k` wraps past any word. A mask, not a shift by
        // `bit`: one `bt`.
        let (k, m) = ((bit >> 6).wrapping_sub(self.base()), 1u64 << (bit & 63));
        if let Some(&w) = self.words.get(k) {
            return w & m != 0;
        }
        bit < self.toc || k == self.words.len() && self.tail & m != 0
    }

    /// The 64 bits from `bit` on, LSB first: bit `bit` in the lowest. Bits beyond the end are 0.
    #[inline]
    pub fn get64(&self, bit: usize) -> u64 {
        let (a, s) = (bit >> 6, bit & 63);
        let lo = self.word(a) >> s;
        if s == 0 {
            lo
        } else {
            lo | self.word(a + 1) << (64 - s)
        }
    }

    /// The `dst.len() * 8` bits from `from` on into `dst`, LSB first, as they travel (`get64` for
    /// any length): the words are copied as they are when `from` is at a word, else one shift each.
    /// Bits past the end are 0.
    pub fn get_bytes(&self, from: usize, dst: &mut [u8]) {
        let mut chunks = dst.chunks_exact_mut(8);
        let mut at = from;
        while at >> 6 < self.base() {
            // the run
            match chunks.next() {
                Some(c) => c.copy_from_slice(&self.get64(at).to_le_bytes()),
                None => break,
            }
            at += 64;
        }
        if self.base() <= at >> 6 {
            let (s, src) = ((at & 63) as u32, self.words.get((at >> 6) - self.base()..).unwrap_or(&[]));
            let mut done = 0;
            if s == 0 {
                for (w, c) in src.iter().zip(&mut chunks) {
                    c.copy_from_slice(&w.to_le_bytes());
                    done += 1;
                }
            } else {
                for (p, c) in src.windows(2).zip(&mut chunks) {
                    c.copy_from_slice(&(p[0] >> s | p[1] << (64 - s)).to_le_bytes());
                    done += 1;
                }
            }
            at += done << 6;
        }
        for c in &mut chunks {
            // the last word, `tail`, the zeros
            c.copy_from_slice(&self.get64(at).to_le_bytes());
            at += 64;
        }
        let mut rest = chunks.into_remainder();
        if !rest.is_empty() {
            // 4, 2, 1 bytes: a copy of a length not known is a `memcpy` call
            let mut w = self.get64(at);
            if rest.len() >= 4 {
                rest[..4].copy_from_slice(&(w as u32).to_le_bytes());
                (rest, w) = (&mut rest[4..], w >> 32);
            }
            if rest.len() >= 2 {
                rest[..2].copy_from_slice(&(w as u16).to_le_bytes());
                (rest, w) = (&mut rest[2..], w >> 16);
            }
            if let Some(d) = rest.first_mut() {
                *d = w as u8;
            }
        }
    }

    /// Appends the `src.len() * 8` bits of `src`, LSB first (`push_bits` for any length): while the
    /// list is its run words of ones counted and 8 bytes at a time through `push_bits`, then a word
    /// per 8 bytes onto `words`.
    pub fn push_bytes(&mut self, src: &[u8]) {
        let le = |c: &[u8]| match c.try_into() {
            Ok(b) => u64::from_le_bytes(b),
            Err(_) => c.iter().rev().fold(0, |w, &b| w << 8 | b as u64),
        };
        let mut s = src;
        while !s.is_empty() && self.len == self.toc {
            let k = s.len().min(8);
            let v = le(&s[..k]);
            if v == u64::MAX {
                // words of ones run the run on: counted, a word at a time; `tail`, the run's ones
                // below `toc`'s word, stays
                let n = 8 + (s[8..].chunks_exact(8).take_while(|c| u64::from_le_bytes((*c).try_into().unwrap()) == u64::MAX).count() << 3);
                self.toc += n << 3;
                self.len = self.toc;
                s = &s[n..];
                continue;
            }
            self.push_bits(v, (k << 3) as u32);
            s = &s[k..];
        }
        if s.is_empty() {
            return;
        }
        self.open_end();
        let sh = (self.len & 63) as u32;
        self.len += s.len() << 3;
        let chunks = s.chunks_exact(8);
        let rest = chunks.remainder();
        // `extend` of an iterator of known length: no capacity check per word
        if sh == 0 {
            // the end is at a word: `tail` is empty
            self.words.extend(chunks.map(|c| u64::from_le_bytes(c.try_into().unwrap())));
            self.tail = le(rest);
        } else {
            let mut t = self.tail;
            self.words.extend(chunks.map(|c| {
                let v = u64::from_le_bytes(c.try_into().unwrap());
                let w = t | v << sh;
                t = v >> (64 - sh);
                w
            }));
            if !rest.is_empty() {
                let v = le(rest);
                if sh as usize + (rest.len() << 3) >= 64 {
                    self.words.push(t | v << sh);
                    t = v >> (64 - sh);
                } else {
                    t |= v << sh;
                }
            }
            self.tail = t;
        }
    }

    /// The `n` bits (1..=64) from `bit` on, LSB first: exactly how they travel.
    #[inline]
    pub fn raw(&self, bit: usize, n: u32) -> u64 {
        self.get64(bit) & mask(n as usize)
    }

    /// Copies bits `[from, to)` (clamped to the list) into `dst` from its bit 0; returns the number
    /// of bits copied, at most `dst.len() * 64`.
    pub fn get_words(&self, dst: &mut [u64], from: usize, to: usize) -> usize {
        let to = to.min(self.len);
        if from >= to {
            return 0;
        }
        let n = (to - from).min(dst.len() * 64);
        for (i, d) in dst[..n.div_ceil(64)].iter_mut().enumerate() {
            *d = self.get64(from + i * 64);
        }
        if n & 63 != 0 {
            dst[n / 64] &= mask(n % 64);
        }
        n
    }

    /// The first one at an index `>= from`.
    pub fn next_one(&self, from: usize) -> Option<usize> {
        if from >= self.len {
            return None;
        }
        if from < self.toc {
            return Some(from);
        }
        let base = self.base();
        let mut k = (from >> 6) - base;
        let mut w = self.at_opt(k)? & u64::MAX << (from & 63);
        loop {
            if w != 0 {
                return Some(((base + k) << 6) + w.trailing_zeros() as usize);
            }
            k += 1;
            w = self.at_opt(k)?;
        }
    }

    /// The first zero at an index `>= from`.
    pub fn next_zero(&self, from: usize) -> Option<usize> {
        let pos = from.max(self.toc);
        if pos >= self.len {
            return None;
        }
        let base = self.base();
        let mut k = (pos >> 6) - base;
        let mut w = match self.at_opt(k) {
            Some(x) => !x & u64::MAX << (pos & 63),
            None => return Some(pos), // past `tail`
        };
        loop {
            if w != 0 {
                let p = ((base + k) << 6) + w.trailing_zeros() as usize;
                return (p < self.len).then_some(p);
            }
            k += 1;
            match self.at_opt(k) {
                Some(x) => w = !x,
                None => {
                    let p = (base + k) << 6;
                    return (p < self.len).then_some(p);
                }
            }
        }
    }

    /// The last one at an index `< before` (`before` is clamped to `len`).
    pub fn prev_one(&self, before: usize) -> Option<usize> {
        let end = before.min(self.len);
        if end == 0 {
            return None;
        }
        let last = end - 1;
        if last < self.toc {
            return Some(last);
        }
        let base = self.base();
        let lk = (last >> 6) - base;
        let (mut k, mut w) = if lk <= self.words.len() {
            (lk, self.at(lk) & mask((last & 63) + 1))
        } else {
            (self.words.len(), self.tail)
        };
        loop {
            if w != 0 {
                return Some(((base + k) << 6) + 63 - w.leading_zeros() as usize);
            }
            if k == 0 {
                return self.toc.checked_sub(1);
            }
            k -= 1;
            w = self.words[k];
        }
    }

    /// The last zero at an index `< before` (`before` is clamped to `len`).
    pub fn prev_zero(&self, before: usize) -> Option<usize> {
        let end = before.min(self.len);
        if end == 0 {
            return None;
        }
        let last = end - 1;
        if last < self.toc {
            return None;
        }
        let base = self.base();
        let mut k = (last >> 6) - base;
        if k > self.words.len() {
            return Some(last); // past `tail`
        }
        let mut w = !self.at(k) & mask((last & 63) + 1);
        loop {
            if w != 0 {
                return Some(((base + k) << 6) + 63 - w.leading_zeros() as usize);
            }
            if k == 0 {
                return None; // the ones of the run fill the first word below `toc`
            }
            k -= 1;
            w = !self.words[k];
        }
    }

    /// The index of the last one.
    pub fn last_one(&self) -> Option<usize> {
        let base = self.base();
        if self.tail != 0 {
            return Some(((base + self.words.len()) << 6) + 63 - self.tail.leading_zeros() as usize);
        }
        for (k, &w) in self.words.iter().enumerate().rev() {
            if w != 0 {
                return Some(((base + k) << 6) + 63 - w.leading_zeros() as usize);
            }
        }
        self.toc.checked_sub(1)
    }

    /// One past the last one; 0 when there is none.
    #[inline]
    fn ones_end(&self) -> usize {
        self.last_one().map_or(0, |i| i + 1)
    }

    /// No ones at all.
    #[inline]
    pub fn is_all_zeros(&self) -> bool {
        self.toc == 0 && self.tail == 0 && self.words.iter().all(|&w| w == 0)
    }

    /// The number of ones.
    pub fn count_ones(&self) -> usize {
        (self.base() << 6) + self.words.iter().map(|w| w.count_ones() as usize).sum::<usize>() + self.tail.count_ones() as usize
    }

    /// The number of ones in `[0, bit]`, inclusive; `bit` is clamped to the list.
    pub fn rank(&self, bit: usize) -> usize {
        if self.len == 0 {
            return 0;
        }
        let bit = bit.min(self.len - 1);
        if bit < self.toc {
            return bit + 1;
        }
        let base = self.base();
        let k = (bit >> 6) - base;
        let full = k.min(self.words.len());
        let sum = (base << 6) + self.words[..full].iter().map(|w| w.count_ones() as usize).sum::<usize>();
        sum + if k <= self.words.len() {
            (self.at(k) & mask((bit & 63) + 1)).count_ones()
        } else {
            self.tail.count_ones()
        } as usize
    }

    /// The index of the `n`-th one, from 0.
    pub fn nth_one(&self, n: usize) -> Option<usize> {
        if n < self.toc {
            return Some(n);
        }
        let base = self.base();
        let mut n = n - (base << 6); // the ones of the run in the first word count here
        for (k, w) in self.words.iter().copied().chain(core::iter::once(self.tail)).enumerate() {
            let c = w.count_ones() as usize;
            if n < c {
                let mut w = w;
                for _ in 0..n {
                    w &= w - 1;
                }
                return Some(((base + k) << 6) + w.trailing_zeros() as usize);
            }
            n -= c;
        }
        None
    }

    /// The leading run of ones from index 0: `toc` and the ones `push` ran on past it.
    #[inline]
    pub fn trailing_ones(&self) -> usize {
        self.next_zero(self.toc).unwrap_or(self.len)
    }

    /// The zeros from index 0 up to the first one; `len` when there is none.
    pub fn trailing_zeros(&self) -> usize {
        self.next_one(0).unwrap_or(self.len)
    }

    /// The zeros after the last one.
    #[inline]
    pub fn leading_zeros(&self) -> usize {
        self.len - self.ones_end()
    }

    /// The run of ones that ends at `len - 1`; 0 when the last bit is 0.
    pub fn leading_ones(&self) -> usize {
        match self.last_one() {
            Some(l) if l + 1 == self.len => match self.prev_zero(l) {
                Some(z) => l - z,
                None => self.len,
            },
            _ => 0,
        }
    }

    /// The first index where `self` and `other` differ, or the shorter length.
    pub fn find_first_difference(&self, other: &BitList) -> usize {
        let limit = self.len.min(other.len);
        let (a, b) = (self.trailing_ones(), other.trailing_ones());
        let mut bit = a.min(b);
        if a != b {
            return bit.min(limit);
        }
        while bit < limit {
            let x = self.get64(bit) ^ other.get64(bit);
            if x != 0 {
                return (bit + x.trailing_zeros() as usize).min(limit);
            }
            bit += 64;
        }
        limit
    }

    /// `true` if some index holds a one in both lists.
    pub fn intersects(&self, other: &BitList) -> bool {
        let limit = self.len.min(other.len);
        if limit == 0 {
            return false;
        }
        if self.toc > 0 && other.toc > 0 {
            return true;
        }
        // One run (at most) is not empty: a one of the other list inside it intersects.
        if self.toc > 0 && other.next_one(0).is_some_and(|i| i < self.toc) {
            return true;
        }
        if other.toc > 0 && self.next_one(0).is_some_and(|i| i < other.toc) {
            return true;
        }
        let end = self.ones_end().min(other.ones_end());
        let mut bit = self.toc.max(other.toc);
        while bit < end {
            if self.get64(bit) & other.get64(bit) != 0 {
                return true;
            }
            bit += 64;
        }
        false
    }

    /// The bits in order.
    #[inline]
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            list: self,
            a: 0,
            cur: 0,
            left: 0,
            rest: self.len,
        }
    }

    /// The indices of the ones, ascending.
    #[inline]
    pub fn ones(&self) -> Ones<'_> {
        let base = self.base();
        Ones {
            list: self,
            pos: 0,
            a: base,
            cur: self.word(base) & !mask(self.toc & 63),
            remaining: self.count_ones(),
        }
    }

    // writing

    /// Sets the bit at `bit`; the list grows to `bit + 1` if needed.
    #[inline]
    pub fn set(&mut self, bit: usize, value: bool) {
        // From `toc` on and before the end, in `words` or `tail`: the bit is written without a
        // branch on it (a one at `toc` only runs the run on past `toc` - allowed).
        if self.toc <= bit && bit < self.len {
            let (k, s) = ((bit >> 6) - self.base(), bit & 63);
            if let Some(w) = self.words.get_mut(k) {
                *w = *w & !(1 << s) | (value as u64) << s;
                return;
            }
            if k == self.words.len() {
                self.tail = self.tail & !(1 << s) | (value as u64) << s;
                return;
            }
        }
        self.set_slow(bit, value);
    }

    fn set_slow(&mut self, bit: usize, value: bool) {
        if self.len <= bit {
            self.len = bit + 1;
            if !value {
                return; // zeros past the last one are implicit
            }
        }
        let s = bit & 63;
        if value && bit == self.toc && self.words.is_empty() && self.tail >> s == 0 {
            // a one at `toc` with nothing past it: the run takes it (a list set one by one stays a
            // count)
            self.toc += 1;
            self.tail = if s == 63 { 0 } else { self.tail | 1 << s };
            return;
        }
        if self.toc <= bit {
            // past `tail`, among the implicit zeros
            let w = self.word_mut((bit >> 6) - self.base());
            *w = *w & !(1 << s) | (value as u64) << s;
            return;
        }
        // inside the run: a one is there; a zero splits it
        if value {
            return;
        }
        let a0 = bit >> 6;
        let mut ex = self.take(a0);
        ex[0] &= !(1 << s);
        self.install(a0, ex);
    }

    /// Sets `bit` to one; the list grows to `bit + 1` if needed.
    #[inline]
    pub fn set1(&mut self, bit: usize) {
        self.set(bit, true)
    }

    /// Sets `bit` to zero; the list grows to `bit + 1` if needed.
    #[inline]
    pub fn set0(&mut self, bit: usize) {
        self.set(bit, false)
    }

    /// Sets bits `range` to `value`; the list grows to `range.end` if needed.
    pub fn set_range(&mut self, range: Range<usize>, value: bool) {
        let Range { start: from, end: to } = range;
        if from >= to {
            return;
        }
        self.len = self.len.max(to);
        let op = if value { Fill::Ones } else { Fill::Zeros };
        if self.toc <= from {
            // in place, in `words` and `tail`
            if value {
                self.word_mut(((to - 1) >> 6) - self.base());
            }
            let b = self.base() << 6;
            self.words.push(self.tail);
            fill_words(&mut self.words, from - b, to - b, op);
            self.tail = self.words.pop().unwrap_or(0);
            return;
        }
        let a0 = from >> 6;
        let mut ex = self.take(a0);
        fill_words(&mut ex, from - (a0 << 6), to - (a0 << 6), op);
        self.install(a0, ex);
    }

    /// Inverts the bit at `bit`; the list grows to `bit + 1` if needed.
    #[inline]
    pub fn flip(&mut self, bit: usize) {
        self.set(bit, !self.get(bit));
    }

    /// Inverts bits `range`; the list grows to `range.end` if needed.
    pub fn flip_range(&mut self, range: Range<usize>) {
        let Range { start: from, end: to } = range;
        if from >= to {
            return;
        }
        self.len = self.len.max(to);
        let a0 = (from >> 6).min(self.base());
        let mut ex = self.take(a0);
        fill_words(&mut ex, from - (a0 << 6), to - (a0 << 6), Fill::Flip);
        self.install(a0, ex);
    }

    /// Sets the bits from `index` on to `values`: packed by SIMD into bytes on the stack, a piece
    /// at a time, then written a word at a time ([`set_bits`](Self::set_bits)); the last bools
    /// short of a byte one by one.
    pub fn set_bools(&mut self, index: usize, values: &[bool]) {
        let mut buf = [0u8; BULK / 8];
        let whole = values.len() & !7;
        let mut at = index;
        for c in values[..whole].chunks(BULK) {
            let b = &mut buf[..c.len() / 8];
            pack_bools(c, b);
            for w in b.chunks(8) {
                let mut x = [0u8; 8];
                x[..w.len()].copy_from_slice(w);
                self.set_bits(at, u64::from_le_bytes(x), w.len() << 3);
                at += w.len() << 3;
            }
        }
        for &v in &values[whole..] {
            self.set(at, v);
            at += 1;
        }
    }

    /// Sets bits `[bit, bit + n)` (`n` 1..=64) to the low `n` bits of `w`. Past the bit after the
    /// run and before the end: one or two masked word writes. Anywhere else bit by bit.
    pub fn set_bits(&mut self, bit: usize, w: u64, n: usize) {
        debug_assert!((1..=64).contains(&n));
        if self.toc <= bit && bit + n <= self.len {
            let (k, s, m) = ((bit >> 6) - self.base(), bit & 63, mask(n));
            let w = w & m;
            let x = self.word_mut(k);
            *x = *x & !(m << s) | w << s;
            if s + n > 64 {
                let r = 64 - s;
                let y = self.word_mut(k + 1);
                *y = *y & !(m >> r) | w >> r;
            }
            return;
        }
        for k in 0..n {
            self.set(bit + k, w >> k & 1 != 0);
        }
    }

    /// Appends a bit.
    #[inline]
    pub fn push(&mut self, value: bool) {
        let len = self.len;
        // The end in `tail` (one compare): the bit is OR-ed into the register-held `tail`, which
        // goes to `words` when full - or, all ones with nothing before it in `words`, to the run.
        if (len >> 6) - self.base() == self.words.len() {
            self.tail |= (value as u64) << (len & 63);
            self.len = len + 1;
            if len & 63 == 63 {
                if self.words.is_empty() && self.tail == u64::MAX {
                    self.toc = len + 1;
                } else {
                    self.words.push(self.tail);
                }
                self.tail = 0;
            }
            return;
        }
        self.tail = Self::push_rare(&mut self.words, len, self.toc, self.tail, value);
        self.len = len + 1;
    }

    /// [`push`](Self::push) of a bit past implicit zeros: `words` grows to the end's word; the new
    /// `tail`. It gets `words` and the values of the fields, not `self`: a call that may change any
    /// field would make the compiler reload `tail` on every push.
    #[cold]
    #[inline(never)]
    fn push_rare(words: &mut Vec<u64>, len: usize, toc: usize, tail: u64, value: bool) -> u64 {
        words.push(tail);
        words.resize((len >> 6) - (toc >> 6), 0);
        let tail = (value as u64) << (len & 63);
        if len & 63 == 63 {
            words.push(tail);
            return 0;
        }
        tail
    }

    /// Appends the `n` (1..=64) lowest bits of `src`, LSB first: how a receiver fills a list. A
    /// leading run of ones only extends `toc`; the rest is OR-ed into `tail`.
    pub fn push_bits(&mut self, mut src: u64, n: u32) {
        debug_assert!((1..=64).contains(&n));
        let mut n = n as usize;
        src &= mask(n);
        if self.len == self.toc {
            // the list is its run: `words` is empty, `tail` is the run's last word
            let ones = (src.trailing_ones() as usize).min(n);
            self.toc += ones;
            self.len += ones;
            self.tail = mask(self.toc & 63);
            if ones == n {
                return;
            }
            src >>= ones;
            n -= ones;
        }
        self.open_end();
        let sh = self.len & 63;
        self.len += n;
        self.tail |= src << sh;
        if sh + n >= 64 {
            self.words.push(self.tail);
            self.tail = if sh == 0 { 0 } else { src >> (64 - sh) };
        }
    }

    /// Inserts `value` at `bit`, moving the bits from `bit` on up by one. Past the end the list
    /// grows to `bit + 1`, the gap filled with zeros.
    pub fn insert(&mut self, bit: usize, value: bool) {
        if bit >= self.len {
            self.set(bit, value);
            return;
        }
        // a one anywhere in the run is a one at its end
        let at = if value && bit <= self.toc { self.toc } else { bit };
        self.len += 1;
        if self.base() <= at >> 6 {
            // from `toc`'s word on: shifted in place through `words` and `tail`, whose top bit
            // opens the next word; a zero inside the run ends the run there
            self.toc = self.toc.min(at);
            let k = (at >> 6) - self.base();
            let carry = self.tail >> 63;
            match self.words.len().cmp(&k) {
                core::cmp::Ordering::Greater => {
                    let top = self.words[self.words.len() - 1] >> 63;
                    insert_bit(&mut self.words[k..], at & 63, value);
                    self.tail = self.tail << 1 | top;
                }
                core::cmp::Ordering::Equal => {
                    let (x, m) = (self.tail, mask(at & 63));
                    self.tail = x & m | (x & !m) << 1 | (value as u64) << (at & 63);
                }
                // among the zeros past `tail`
                core::cmp::Ordering::Less => {
                    return if value {
                        self.set(at, true)
                    }
                }
            }
            if carry != 0 {
                self.words.push(self.tail);
                self.tail = 1;
            }
            return;
        }
        let a0 = (at >> 6).min(self.base());
        let mut ex = self.take(a0);
        let need = (self.len - 1) / 64 + 1 - a0;
        if ex.len() < need {
            ex.resize(need, 0);
        }
        insert_bit(&mut ex, at - (a0 << 6), value);
        self.install(a0, ex);
    }

    /// Removes the bit at `bit`, moving the bits above down by one; `None` past the end.
    pub fn remove(&mut self, bit: usize) -> Option<bool> {
        if bit >= self.len {
            return None;
        }
        // a one of the run is its last one
        let at = if bit < self.toc { self.toc - 1 } else { bit };
        if self.base() <= at >> 6 {
            // from `toc`'s word on: shifted in place through `words` and `tail`; a one of the run
            // shortens it
            let k = (at >> 6) - self.base();
            self.len -= 1;
            self.toc -= (at < self.toc) as usize;
            return Some(match self.words.len().cmp(&k) {
                core::cmp::Ordering::Greater => {
                    let last = self.words.len() - 1;
                    let v = remove_bit(&mut self.words[k..], at & 63);
                    self.words[last] |= self.tail << 63;
                    self.tail >>= 1;
                    if self.len >> 6 < self.base() + self.words.len() {
                        // the end went down into the last word: it is `tail` now (the old one is 0)
                        self.tail = self.words.pop().unwrap();
                    }
                    v
                }
                core::cmp::Ordering::Equal => {
                    let (x, m) = (self.tail, mask(at & 63));
                    self.tail = x & m | x >> 1 & !m;
                    x >> (at & 63) & 1 != 0
                }
                core::cmp::Ordering::Less => false, // among the zeros past `tail`
            });
        }
        let a0 = (at >> 6).min(self.base());
        let mut ex = self.take(a0);
        let v = remove_bit(&mut ex, at - (a0 << 6));
        self.len -= 1;
        self.install(a0, ex);
        Some(v)
    }

    /// Removes the last bit.
    pub fn pop(&mut self) -> Option<bool> {
        let last = self.len.checked_sub(1)?;
        let v = self.get(last);
        self.truncate(last);
        Some(v)
    }

    /// Shortens the list to `len` bits; no effect if it is not longer.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        self.len = len;
        if len <= self.toc {
            self.toc = len;
            self.words.clear();
            self.tail = mask(len & 63);
            return;
        }
        let k = (len >> 6) - self.base();
        if k <= self.words.len() {
            let w = self.at(k);
            self.words.truncate(k);
            self.tail = w & mask(len & 63);
        }
    }

    /// Sets the length: shortens, or grows with zeros.
    pub fn set_len(&mut self, len: usize) {
        if len < self.len {
            self.truncate(len);
        } else {
            self.len = len;
        }
    }

    /// Drops the zeros after the last one.
    pub fn trim(&mut self) {
        self.truncate(self.ones_end());
        self.words.shrink_to_fit();
    }

    /// Frees the unused word capacity.
    pub fn shrink_to_fit(&mut self) {
        self.words.shrink_to_fit();
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.toc = 0;
        self.words.clear();
        self.tail = 0;
    }

    // bulk operations

    // Each operation computes the result words from a word below which the result is all ones; past
    // the end the operands are zeros, and so is the result. `install` finds the run.

    /// `self &= other`. The length stays; bits past `other`'s end count as zeros.
    pub fn and(&mut self, other: &BitList) {
        let a0 = self.toc.min(other.toc) >> 6;
        let end = self.words_end().min(other.words_end()).max(a0);
        let ex: Vec<u64> = (a0..end).map(|a| self.word(a) & other.word(a)).collect();
        self.install(a0, ex);
    }

    /// `self |= other`. The length becomes the longer one.
    pub fn or(&mut self, other: &BitList) {
        let a0 = self.toc.max(other.toc) >> 6;
        let end = self.words_end().max(other.words_end());
        let ex: Vec<u64> = (a0..end).map(|a| self.word(a) | other.word(a)).collect();
        self.len = self.len.max(other.len);
        self.install(a0, ex);
    }

    /// `self ^= other`. The length becomes the longer one.
    pub fn xor(&mut self, other: &BitList) {
        let end = self.words_end().max(other.words_end());
        let ex: Vec<u64> = (0..end).map(|a| self.word(a) ^ other.word(a)).collect();
        self.len = self.len.max(other.len);
        self.install(0, ex);
    }

    /// `self &= !other`. The length stays.
    pub fn and_not(&mut self, other: &BitList) {
        let a0 = if other.toc > 0 { 0 } else { self.toc.min(other.next_one(0).unwrap_or(usize::MAX)) >> 6 };
        let end = self.words_end().max(a0);
        let ex: Vec<u64> = (a0..end).map(|a| self.word(a) & !other.word(a)).collect();
        self.install(a0, ex);
    }
}

impl PartialEq for BitList {
    /// The same bits: neither where `toc` stopped nor trailing zero words count.
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        if self.base() == other.base() {
            // `words` side by side: the longer one's rest against the shorter one's `tail` and
            // zeros, then the common part as slices
            let (a, b) = if self.words.len() <= other.words.len() { (self, other) } else { (other, self) };
            let n = a.words.len();
            return match b.words[n..].split_first() {
                None => a.tail == b.tail,
                Some((&w, rest)) => w == a.tail && b.tail == 0 && rest.iter().all(|&w| w == 0),
            } && match n {
                // one word: no `memcmp` call
                0 => true,
                1 => a.words[0] == b.words[0],
                _ => a.words[..] == b.words[..n],
            };
        }
        (self.base().min(other.base())..self.words_end().max(other.words_end())).all(|a| self.word(a) == other.word(a))
    }
}

impl Eq for BitList {}

impl Hash for BitList {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // the bits as `PartialEq` sees them: the whole run, then the words up to the last one
        let run = self.trailing_ones();
        self.len.hash(state);
        run.hash(state);
        let mut end = self.words_end();
        while run >> 6 < end && self.word(end - 1) == 0 {
            end -= 1;
        }
        for a in run >> 6..end {
            self.word(a).hash(state);
        }
    }
}

#[cfg(test)]
impl BitList {
    pub(crate) fn assert_canonical(&self) {
        assert!(self.toc <= self.len, "toc {} > len {}", self.toc, self.len);
        let m = mask(self.toc & 63);
        assert_eq!(self.word(self.base()) & m, m, "the ones of the run in its last word");
        assert!(self.base() + self.words.len() <= self.len >> 6, "words past the word of the end");
        assert!(self.ones_end() <= self.len, "a one past the end");
    }
}

impl fmt::Debug for BitList {
    /// Formats logical bits in index order, for example `[1, 0, 1,...]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter().map(|b| b as u8)).finish()
    }
}

impl Index<usize> for BitList {
    type Output = bool;
    #[inline]
    fn index(&self, bit: usize) -> &bool {
        if self.get(bit) {
            &true
        } else {
            &false
        }
    }
}

impl Extend<bool> for BitList {
    /// A piece of the bools at a time onto the stack (a plain copy the compiler vectorizes), then
    /// [`push_bools`](BitList::push_bools).
    fn extend<I: IntoIterator<Item = bool>>(&mut self, iter: I) {
        let mut it = iter.into_iter();
        let mut buf = [false; BULK];
        loop {
            let mut n = 0;
            for (slot, b) in buf.iter_mut().zip(&mut it) {
                *slot = b;
                n += 1;
            }
            self.push_bools(&buf[..n]);
            if n < BULK {
                return;
            }
        }
    }
}

/// The low bits of 8 bytes of 0 / 1 into one byte, byte 0 lowest: each byte's bit lands in the top
/// byte of the product.
#[inline(always)]
fn gather8(b: u64) -> u64 {
    b.wrapping_mul(0x0102_0408_1020_4080) >> 56
}

/// One byte into 8 bytes of 0 / 1, bit 0 lowest: broadcast, keep bit k in byte k, carry it to bit
/// 7, down to bit 0.
#[inline(always)]
fn spread8(b: u64) -> u64 {
    let t = (b & 0xFF).wrapping_mul(0x0101_0101_0101_0101) & 0x8040_2010_0804_0201;
    ((t + 0x7F7F_7F7F_7F7F_7F7F) >> 7) & 0x0101_0101_0101_0101
}

/// `src` (`dst.len() * 8` bools) into `dst`, 8 to a byte, bool 0 lowest: 32 at a time (AVX2) or 16
/// (SSE2) by the top bit of each byte, the rest 8 at a time by one multiply.
fn pack_bools(src: &[bool], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len() * 8);
    // SAFETY: the features are checked; the lengths are the caller's
    #[cfg(target_arch = "x86_64")]
    let done = unsafe {
        if std::is_x86_feature_detected!("avx2") {
            pack_avx2(src, dst)
        } else {
            pack_sse2(src, dst)
        }
    };
    #[cfg(not(target_arch = "x86_64"))]
    let done = 0;
    for (d, e) in dst[done..].iter_mut().zip(src[done * 8..].chunks_exact(8)) {
        // SAFETY: a bool is one byte, 0 or 1
        *d = gather8(u64::from_le_bytes(unsafe { *(e.as_ptr() as *const [u8; 8]) })) as u8;
    }
}

/// `src` into `dst` (`src.len() * 8` bools), bit 0 lowest: 32 at a time (AVX2), else a byte into 8
/// by one multiply.
fn spread_bytes(src: &[u8], dst: &mut [bool]) {
    debug_assert_eq!(src.len() * 8, dst.len());
    #[allow(unused_mut)]
    let mut done = 0;
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: the feature is checked; the lengths are the caller's
        done = unsafe { spread_avx2(src, dst) };
    }
    for (&b, e) in src[done..].iter().zip(dst[done * 8..].chunks_exact_mut(8)) {
        let v = spread8(b as u64).to_le_bytes();
        // SAFETY: each byte is 0 or 1 - a bool
        e.copy_from_slice(unsafe { &*(v.as_ptr() as *const [bool; 8]) });
    }
}

/// The bytes [`pack_bools`] made, 4 per 32 bools.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn pack_avx2(src: &[bool], dst: &mut [u8]) -> usize {
    use std::arch::x86_64::*;
    let n = dst.len() / 4;
    for i in 0..n {
        let v = _mm256_loadu_si256(src.as_ptr().add(i * 32) as *const __m256i);
        let m = _mm256_movemask_epi8(_mm256_slli_epi16(v, 7)) as u32;
        dst.as_mut_ptr().add(i * 4).cast::<[u8; 4]>().write_unaligned(m.to_le_bytes());
    }
    n * 4
}

/// The bytes [`pack_bools`] made, 2 per 16 bools.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn pack_sse2(src: &[bool], dst: &mut [u8]) -> usize {
    use std::arch::x86_64::*;
    let n = dst.len() / 2;
    for i in 0..n {
        let v = _mm_loadu_si128(src.as_ptr().add(i * 16) as *const __m128i);
        let m = _mm_movemask_epi8(_mm_slli_epi16(v, 7)) as u16;
        dst.as_mut_ptr().add(i * 2).cast::<[u8; 2]>().write_unaligned(m.to_le_bytes());
    }
    n * 2
}

/// The bytes [`spread_bytes`] spread, 4 per 32 bools: each byte copied to its 8 lanes, lane k keeps
/// bit k.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn spread_avx2(src: &[u8], dst: &mut [bool]) -> usize {
    use std::arch::x86_64::*;
    let shuf = _mm256_setr_epi8(0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3);
    let sel = _mm256_set1_epi64x(0x8040_2010_0804_0201u64 as i64);
    let one = _mm256_set1_epi8(1);
    let n = src.len() / 4;
    for i in 0..n {
        let b = _mm256_set1_epi32(src.as_ptr().add(i * 4).cast::<i32>().read_unaligned());
        let m = _mm256_cmpeq_epi8(_mm256_and_si256(_mm256_shuffle_epi8(b, shuf), sel), sel);
        _mm256_storeu_si256(dst.as_mut_ptr().add(i * 32) as *mut __m256i, _mm256_and_si256(m, one));
    }
    n * 4
}

/// Bools a bulk call packs or spreads through the stack at a time.
const BULK: usize = 2048;

impl BitList {
    /// Appends `src` (`Vec::extend_from_slice` of `[bool]`): packed by SIMD into bytes on the
    /// stack, a piece at a time, then [`push_bytes`](Self::push_bytes); the last bools short of a
    /// byte by `push_bits`.
    pub fn push_bools(&mut self, src: &[bool]) {
        let mut buf = [0u8; BULK / 8];
        let whole = src.len() & !7;
        for c in src[..whole].chunks(BULK) {
            let b = &mut buf[..c.len() / 8];
            pack_bools(c, b);
            self.push_bytes(b);
        }
        let rest = &src[whole..];
        if !rest.is_empty() {
            let w = rest.iter().enumerate().fold(0u64, |w, (k, &b)| w | (b as u64) << k);
            self.push_bits(w, rest.len() as u32);
        }
    }

    /// The bits from `from` on into `dst` (`[bool]::copy_from_slice`):
    /// [`get_bytes`](Self::get_bytes) into the stack a piece at a time, spread by SIMD. Bits past
    /// the end are `false`.
    pub fn get_bools(&self, from: usize, dst: &mut [bool]) {
        let mut buf = [0u8; BULK / 8];
        let whole = dst.len() & !7;
        let mut at = from;
        for c in dst[..whole].chunks_mut(BULK) {
            let b = &mut buf[..c.len() / 8];
            self.get_bytes(at, b);
            spread_bytes(b, c);
            at += c.len();
        }
        let rest = &mut dst[whole..];
        if !rest.is_empty() {
            let w = self.get64(at);
            for (k, e) in rest.iter_mut().enumerate() {
                *e = w >> k & 1 != 0;
            }
        }
    }
}

impl FromIterator<bool> for BitList {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Self {
        let mut l = Self::new();
        l.extend(iter);
        l
    }
}

// iterators

/// The bits in order, a word at a time as the list keeps them.
#[derive(Clone)]
pub struct Iter<'a> {
    list: &'a BitList,
    /// The next word.
    a: usize,
    /// The bits of the current word not given yet, lowest first, and their number.
    cur: u64,
    left: u32,
    /// The bits after the current word.
    rest: usize,
}

impl Iter<'_> {
    /// Takes the next word; false at the end.
    #[inline]
    fn refill(&mut self) -> bool {
        if self.rest == 0 {
            return false;
        }
        let n = self.rest.min(64);
        self.cur = self.list.word(self.a);
        self.a += 1;
        self.left = n as u32;
        self.rest -= n;
        true
    }
}

impl Iterator for Iter<'_> {
    type Item = bool;

    #[inline]
    fn next(&mut self) -> Option<bool> {
        if self.left == 0 && !self.refill() {
            return None;
        }
        let b = self.cur & 1 != 0;
        self.cur >>= 1;
        self.left -= 1;
        Some(b)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.rest + self.left as usize;
        (n, Some(n))
    }

    #[inline]
    /// The rest of the current word bit by bit, then pieces spread by SIMD into bools on the stack
    /// ([`BitList::get_bools`]), folded as a `[bool]` - the loop the compiler vectorizes.
    fn fold<B, F: FnMut(B, bool) -> B>(self, mut acc: B, mut f: F) -> B {
        for j in 0..self.left {
            acc = f(acc, self.cur >> j & 1 != 0);
        }
        let end = self.list.len;
        let mut at = end - self.rest;
        let mut buf = [false; BULK];
        while at < end {
            let k = (end - at).min(BULK);
            self.list.get_bools(at, &mut buf[..k]);
            acc = buf[..k].iter().fold(acc, |a, &b| f(a, b));
            at += k;
        }
        acc
    }
}

impl ExactSizeIterator for Iter<'_> {}
impl FusedIterator for Iter<'_> {}

impl<'a> IntoIterator for &'a BitList {
    type Item = bool;
    type IntoIter = Iter<'a>;
    #[inline]
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

/// The indices of the ones: the run, then `tzcnt` over the words (the ones of the run in its last
/// word left out).
#[derive(Clone)]
pub struct Ones<'a> {
    list: &'a BitList,
    /// The next index of the run.
    pos: usize,
    /// The word of `cur`.
    a: usize,
    cur: u64,
    remaining: usize,
}

impl Iterator for Ones<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        if self.pos < self.list.toc {
            self.pos += 1;
            self.remaining -= 1;
            return Some(self.pos - 1);
        }
        while self.cur == 0 {
            self.a += 1;
            if self.list.words_end() <= self.a {
                return None;
            }
            self.cur = self.list.word(self.a);
        }
        let i = (self.a << 6) + self.cur.trailing_zeros() as usize;
        self.cur &= self.cur - 1;
        self.remaining -= 1;
        Some(i)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Ones<'_> {}
impl FusedIterator for Ones<'_> {}
