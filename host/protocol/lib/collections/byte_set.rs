//! Set of one-byte keys stored in four `u64` words.
//!
//! [`ByteSet`] is `Copy` and does not allocate. `len` counts the bits of the four words: constant time, no count kept.
//! Iterators own a copy of the bitset and yield keys in unsigned byte order.

use super::keys::ByteKey;
use super::Token;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::iter::FusedIterator;
use core::marker::PhantomData;
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign, Sub, SubAssign};

pub struct ByteSet<K = u8> {
    /// Keys 0-63, 64-127, 128-191, 192-255.
    bits: [u64; 4],
    _key: PhantomData<fn() -> K>,
}

impl<K> Clone for ByteSet<K> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for ByteSet<K> {}

impl<K> Default for ByteSet<K> {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}

impl<K> PartialEq for ByteSet<K> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.bits == other.bits
    }
}

impl<K> Eq for ByteSet<K> {}

impl<K> Hash for ByteSet<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bits.hash(state);
    }
}

impl<K> ByteSet<K> {
    /// First unsigned key token; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> {
        self.next_from(0).map(|b| b as usize)
    }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        if !self.contains_token(token) {
            return None;
        }
        self.next_from(token + 1).map(|b| b as usize)
    }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        token < 256 && self.contains_byte(token as u8)
    }

    #[inline(always)]
    pub const fn new() -> Self {
        Self { bits: [0; 4], _key: PhantomData }
    }

    /// Sets all 256 byte-domain bits. For 256 distinct keys, `K::from_byte` must distinguish every
    /// byte value.
    #[inline(always)]
    pub const fn full() -> Self {
        Self {
            bits: [u64::MAX; 4],
            _key: PhantomData,
        }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        (self.bits[0].count_ones() + self.bits[1].count_ones() + self.bits[2].count_ones() + self.bits[3].count_ones()) as usize
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.bits[0] | self.bits[1] | self.bits[2] | self.bits[3] == 0
    }

    #[inline(always)]
    pub fn clear(&mut self) {
        self.bits = [0; 4];
    }

    /// Nothing to do: every key has its bit (the API of the hash sets).
    #[inline(always)]
    pub fn reserve(&mut self, _additional: usize) {}

    /// The raw 64-bit word `i` (0..4): keys `64 * i..= 64 * i + 63`.
    #[inline(always)]
    pub const fn word(&self, i: usize) -> u64 {
        self.bits[i]
    }

    #[inline(always)]
    pub(crate) fn contains_byte(&self, b: u8) -> bool {
        self.bits[(b >> 6) as usize] >> (b & 63) & 1 != 0
    }

    /// `true` if the bit was clear.
    #[inline(always)]
    pub(crate) fn set1(&mut self, b: u8) -> bool {
        let w = &mut self.bits[(b >> 6) as usize];
        let bit = 1u64 << (b & 63);
        let was = *w & bit == 0;
        *w |= bit;
        was
    }

    /// `true` if the bit was set.
    #[inline(always)]
    pub(crate) fn set0(&mut self, b: u8) -> bool {
        let w = &mut self.bits[(b >> 6) as usize];
        let bit = 1u64 << (b & 63);
        let was = *w & bit != 0;
        *w &= !bit;
        was
    }

    /// `true` if no key is `≥ b`: `b` would be the last. A mask a word, no branch.
    #[inline(always)]
    pub(crate) fn none_from(&self, b: u8) -> bool {
        let s = b as u32;
        let m = |j: u32| {
            if s < j << 6 {
                u64::MAX
            } else if s < j + 1 << 6 {
                u64::MAX << (s & 63)
            } else {
                0
            }
        };
        self.bits[0] & m(0) | self.bits[1] & m(1) | self.bits[2] & m(2) | self.bits[3] & m(3) == 0
    }

    /// Number of keys `< b`: the position of `b` in the sorted set.
    #[inline]
    pub(crate) fn rank_below(&self, b: u8) -> usize {
        let i = (b >> 6) as usize;
        let mut r = (self.bits[i] & ((1u64 << (b & 63)) - 1)).count_ones();
        // only the words below `i`: none for the first word
        match i {
            3 => r += self.bits[2].count_ones() + self.bits[1].count_ones() + self.bits[0].count_ones(),
            2 => r += self.bits[1].count_ones() + self.bits[0].count_ones(),
            1 => r += self.bits[0].count_ones(),
            _ => {}
        }
        r as usize
    }

    /// The four words of the keys.
    #[inline(always)]
    pub(crate) fn words(&self) -> &[u64; 4] {
        &self.bits
    }

    /// Smallest set byte ≥ `from`.
    #[inline]
    pub(crate) fn next_from(&self, from: usize) -> Option<u8> {
        if from >= 256 {
            return None;
        }
        let mut i = from >> 6;
        let mut w = self.bits[i] & (u64::MAX << (from & 63));
        loop {
            if w != 0 {
                return Some((i << 6 | w.trailing_zeros() as usize) as u8);
            }
            i += 1;
            if i == 4 {
                return None;
            }
            w = self.bits[i];
        }
    }

    /// Largest set byte ≤ `to`.
    #[inline]
    pub(crate) fn prev_from(&self, to: u8) -> Option<u8> {
        let mut i = (to >> 6) as usize;
        let mut w = self.bits[i] & (u64::MAX >> (63 - (to & 63)));
        loop {
            if w != 0 {
                return Some(((i << 6) | (63 - w.leading_zeros() as usize)) as u8);
            }
            if i == 0 {
                return None;
            }
            i -= 1;
            w = self.bits[i];
        }
    }

    #[inline]
    pub fn is_subset(&self, other: &Self) -> bool {
        (0..4).all(|i| self.bits[i] & !other.bits[i] == 0)
    }

    #[inline]
    pub fn is_superset(&self, other: &Self) -> bool {
        other.is_subset(self)
    }

    #[inline]
    pub fn is_disjoint(&self, other: &Self) -> bool {
        (0..4).all(|i| self.bits[i] & other.bits[i] == 0)
    }

    #[inline]
    pub fn union_with(&mut self, other: &Self) {
        *self |= *other;
    }

    #[inline]
    pub fn intersect_with(&mut self, other: &Self) {
        *self &= *other;
    }

    #[inline]
    pub fn difference_with(&mut self, other: &Self) {
        *self -= *other;
    }

    #[inline]
    pub fn symmetric_difference_with(&mut self, other: &Self) {
        *self ^= *other;
    }

    /// Ascending keys.
    #[inline]
    pub fn iter(&self) -> Iter<K> {
        Iter {
            bits: self.bits,
            word: 0,
            cur: self.bits[0],
            _key: PhantomData,
        }
    }
}

impl<K: ByteKey> ByteSet<K> {
    #[inline(always)]
    pub fn token_of(&self, key: K) -> Option<Token> {
        let byte = key.to_byte();
        self.contains_byte(byte).then_some(byte as usize)
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<K> {
        self.contains_token(token).then(|| K::from_byte(token as u8))
    }

    /// The `n`-th key of a pass key after key (the generated sending): `at` keeps its place
    /// ([`super::token::Seq`]).
    ///
    /// # Safety
    /// `at` is the cursor of this pass over this set: the pass starts at `n` 0 with any `at`, and
    /// the set is not changed between its steps. A cursor of another collection reads past the bits.
    #[inline(always)]
    pub unsafe fn key_seq(&self, n: usize, at: &mut super::token::Seq) -> K {
        K::from_byte(super::token::seq_bit(n, at, &self.bits) as u8)
    }

    #[inline(always)]
    pub fn contains(&self, k: K) -> bool {
        self.contains_byte(k.to_byte())
    }

    /// `true` if the key was not present.
    #[inline(always)]
    pub fn insert(&mut self, k: K) -> bool {
        self.set1(k.to_byte())
    }

    /// `true` if the key was present.
    #[inline(always)]
    pub fn remove(&mut self, k: K) -> bool {
        self.set0(k.to_byte())
    }

    /// Number of keys ≤ `k` (the statistical rank): in `{2, 5, 8}` `rank(5) == 2`, `rank(6) == 2`.
    #[inline]
    pub fn rank(&self, k: K) -> usize {
        let b = k.to_byte();
        self.rank_below(b) + self.contains_byte(b) as usize
    }

    /// Number of keys < `k`: the index `k` has (or would have) in the sorted set.
    #[inline]
    pub fn count_less_than(&self, k: K) -> usize {
        self.rank_below(k.to_byte())
    }

    /// The smallest key.
    #[inline]
    pub fn first(&self) -> Option<K> {
        self.next_from(0).map(K::from_byte)
    }

    /// The largest key.
    #[inline]
    pub fn last(&self) -> Option<K> {
        self.prev_from(255).map(K::from_byte)
    }

    /// The smallest key greater than `k`.
    #[inline]
    pub fn next_after(&self, k: K) -> Option<K> {
        self.next_from(k.to_byte() as usize + 1).map(K::from_byte)
    }

    /// The largest key less than `k`.
    #[inline]
    pub fn prev_before(&self, k: K) -> Option<K> {
        match k.to_byte() {
            0 => None,
            b => self.prev_from(b - 1).map(K::from_byte),
        }
    }

    /// Keeps only the keys for which `f` returns `true`.
    pub fn retain<F: FnMut(K) -> bool>(&mut self, mut f: F) {
        for k in self.iter() {
            if !f(k) {
                self.set0(k.to_byte());
            }
        }
    }
}

macro_rules! bit_op {
    ($Op:ident, $op:ident, $OpAssign:ident, $op_assign:ident, |$a:ident, $b:ident| $e:expr) => {
        impl<K> $Op for ByteSet<K> {
            type Output = Self;
            #[inline]
            fn $op(mut self, rhs: Self) -> Self {
                self.$op_assign(rhs);
                self
            }
        }

        impl<K> $OpAssign for ByteSet<K> {
            #[inline]
            fn $op_assign(&mut self, rhs: Self) {
                for i in 0..4 {
                    let ($a, $b) = (self.bits[i], rhs.bits[i]);
                    self.bits[i] = $e;
                }
            }
        }
    };
}

bit_op!(BitOr, bitor, BitOrAssign, bitor_assign, |a, b| a | b);
bit_op!(BitAnd, bitand, BitAndAssign, bitand_assign, |a, b| a & b);
bit_op!(BitXor, bitxor, BitXorAssign, bitxor_assign, |a, b| a ^ b);
bit_op!(Sub, sub, SubAssign, sub_assign, |a, b| a & !b);

impl<K: ByteKey + fmt::Debug> fmt::Debug for ByteSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<K: ByteKey> Extend<K> for ByteSet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        for k in iter {
            self.set1(k.to_byte());
        }
    }
}

impl<K: ByteKey> FromIterator<K> for ByteSet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        let mut s = Self::new();
        s.extend(iter);
        s
    }
}

impl<K: ByteKey, const N: usize> From<[K; N]> for ByteSet<K> {
    fn from(keys: [K; N]) -> Self {
        keys.into_iter().collect()
    }
}

// iterator

/// Ascending keys. Owns a copy of the bits: the per-key step is `tzcnt` and a bit clear, a word
/// boundary is crossed at most four times.
pub struct Iter<K> {
    bits: [u64; 4],
    word: usize,
    /// Bits of `bits[word]` not yet yielded.
    cur: u64,
    _key: PhantomData<fn() -> K>,
}

impl<K> Clone for Iter<K> {
    fn clone(&self) -> Self {
        Self { ..*self }
    }
}

impl<K: ByteKey> Iterator for Iter<K> {
    type Item = K;

    #[inline(always)]
    fn next(&mut self) -> Option<K> {
        // the state in registers, written back once a step
        let (mut cur, mut word) = (self.cur, self.word);
        while cur == 0 {
            if word == 3 {
                self.cur = 0;
                return None;
            }
            word += 1;
            cur = self.bits[word];
        }
        self.cur = cur & (cur - 1);
        self.word = word;
        Some(K::from_byte(((word << 6) as u32 | cur.trailing_zeros()) as u8))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        // counted when asked, not kept up on every step
        let mut n = self.cur.count_ones() as usize;
        for w in self.word + 1..4 {
            n += self.bits[w].count_ones() as usize;
        }
        (n, Some(n))
    }

    #[inline]
    fn fold<B, F: FnMut(B, K) -> B>(self, init: B, mut f: F) -> B {
        let mut acc = init;
        let mut cur = self.cur;
        for w in self.word..4 {
            if w != self.word {
                cur = self.bits[w];
            }
            while cur != 0 {
                acc = f(acc, K::from_byte((w << 6) as u8 | cur.trailing_zeros() as u8));
                cur &= cur - 1;
            }
        }
        acc
    }
}

impl<K: ByteKey> ExactSizeIterator for Iter<K> {}
impl<K: ByteKey> FusedIterator for Iter<K> {}

impl<K: ByteKey> IntoIterator for ByteSet<K> {
    type Item = K;
    type IntoIter = Iter<K>;
    #[inline]
    fn into_iter(self) -> Iter<K> {
        self.iter()
    }
}

impl<K: ByteKey> IntoIterator for &ByteSet<K> {
    type Item = K;
    type IntoIter = Iter<K>;
    #[inline]
    fn into_iter(self) -> Iter<K> {
        self.iter()
    }
}
