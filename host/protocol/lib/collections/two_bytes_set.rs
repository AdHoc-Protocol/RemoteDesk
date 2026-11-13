//! Set of two-byte keys backed by [`TwoBytesMap`] with zero-sized values.
//!
//! Small sets use a hash table with `u16` indices. Above 512 keys, storage switches to an 8 KiB
//! presence bitset covering all 65,536 unsigned key values.

use super::keys::TwoBytesKey;
use super::two_bytes_map::{self, TwoBytesMap};
use super::Token;
use core::fmt;
use core::iter::FusedIterator;

pub struct TwoBytesSet<K = u16> {
    map: TwoBytesMap<K, ()>,
}

impl<K> TwoBytesSet<K> {
    /// First lo/hi slot or bitset key token; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> { self.map.token() }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> { self.map.next_token(token) }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool { self.map.contains_token(token) }

    /// Does not allocate.
    #[inline]
    pub fn new() -> Self {
        Self { map: TwoBytesMap::new() }
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { map: TwoBytesMap::with_capacity(capacity) }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// `true` in the bitset layout.
    #[inline]
    pub fn is_flat(&self) -> bool {
        self.map.is_flat()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.map.capacity()
    }

    /// Removes all keys, keeping the layout and its memory.
    #[inline]
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Hash layout: the lo region, then the hi region. Bitset layout: ascending keys.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K> {
        Iter(self.map.iter())
    }
}

impl<K> Default for TwoBytesSet<K> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: TwoBytesKey> TwoBytesSet<K> {
    #[inline(always)]
    pub fn token_of(&self, key: K) -> Option<Token> { self.map.token_of(key) }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<K> { self.map.key_by_token(token) }

    /// The `n`-th key of a pass key after key (the generated sending): `at` keeps its place
    /// ([`super::token::seq_token`]).
    ///
    /// # Safety
    /// As [`TwoBytesMap::entry_seq`]: `at` is the cursor of this pass over this set.
    #[inline]
    pub unsafe fn key_seq(&self, n: usize, at: &mut super::token::Seq) -> K {
        // SAFETY: the caller keeps the contract of this pass, the same for the map of the keys
        unsafe { *self.map.entry_seq(n, at).0 }
    }

    #[inline(always)]
    pub fn contains(&self, k: K) -> bool {
        self.map.contains_key(k)
    }

    /// `true` if the key was not present.
    #[inline(always)]
    pub fn insert(&mut self, k: K) -> bool {
        self.map.insert(k, ()).is_none()
    }

    /// `true` if the key was present.
    #[inline]
    pub fn remove(&mut self, k: K) -> bool {
        self.map.remove(k).is_some()
    }

    /// Keeps only the keys for which `f` returns `true`.
    pub fn retain<F: FnMut(K) -> bool>(&mut self, mut f: F) {
        self.map.retain(|k, _| f(k));
    }

    pub fn reserve(&mut self, additional: usize) {
        self.map.reserve(additional);
    }

    /// Back to the hash layout when it is the smaller one; trims the hash table.
    pub fn shrink_to_fit(&mut self) {
        self.map.shrink_to_fit();
    }

    // set algebra

    /// Removes the keys absent from `other`.
    pub fn intersect_with(&mut self, other: &Self) {
        self.retain(|k| other.contains(k));
    }

    /// Removes the keys present in `other`.
    pub fn difference_with<I: IntoIterator<Item = K>>(&mut self, other: I) {
        for k in other {
            self.remove(k);
        }
    }

    /// Toggles every key of `other`: removes it if present, inserts it otherwise.
    pub fn symmetric_difference_with<I: IntoIterator<Item = K>>(&mut self, other: I) {
        for k in other {
            if !self.remove(k) {
                self.insert(k);
            }
        }
    }

    pub fn is_subset(&self, other: &Self) -> bool {
        self.len() <= other.len() && self.iter().all(|k| other.contains(k))
    }

    pub fn is_superset(&self, other: &Self) -> bool {
        other.is_subset(self)
    }

    /// Walks the smaller set.
    pub fn is_disjoint(&self, other: &Self) -> bool {
        let (small, large) = if self.len() <= other.len() { (self, other) } else { (other, self) };
        small.iter().all(|k| !large.contains(k))
    }
}

impl<K> Clone for TwoBytesSet<K> {
    fn clone(&self) -> Self {
        Self { map: self.map.clone() }
    }
}

impl<K: TwoBytesKey> PartialEq for TwoBytesSet<K> {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl<K: TwoBytesKey> Eq for TwoBytesSet<K> {}

impl<K: TwoBytesKey + fmt::Debug> fmt::Debug for TwoBytesSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<K: TwoBytesKey> Extend<K> for TwoBytesSet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        self.map.extend(iter.into_iter().map(|k| (k, ())));
    }
}

impl<K: TwoBytesKey> FromIterator<K> for TwoBytesSet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        let mut s = Self::new();
        s.extend(iter);
        s
    }
}

// iterators

pub struct Iter<'a, K>(two_bytes_map::Iter<'a, K, ()>);

impl<K> Clone for Iter<'_, K> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<K: TwoBytesKey> Iterator for Iter<'_, K> {
    type Item = K;

    #[inline]
    fn next(&mut self) -> Option<K> {
        self.0.next().map(|(k, _)| k)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn fold<B, F: FnMut(B, K) -> B>(self, init: B, mut f: F) -> B {
        self.0.fold(init, |acc, (k, _)| f(acc, k))
    }
}

impl<K: TwoBytesKey> ExactSizeIterator for Iter<'_, K> {}
impl<K: TwoBytesKey> FusedIterator for Iter<'_, K> {}

pub struct IntoIter<K>(two_bytes_map::IntoIter<K, ()>);

impl<K: TwoBytesKey> Iterator for IntoIter<K> {
    type Item = K;

    #[inline]
    fn next(&mut self) -> Option<K> {
        self.0.next().map(|(k, _)| k)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K: TwoBytesKey> ExactSizeIterator for IntoIter<K> {}
impl<K: TwoBytesKey> FusedIterator for IntoIter<K> {}

impl<'a, K: TwoBytesKey> IntoIterator for &'a TwoBytesSet<K> {
    type Item = K;
    type IntoIter = Iter<'a, K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: TwoBytesKey> IntoIterator for TwoBytesSet<K> {
    type Item = K;
    type IntoIter = IntoIter<K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter(self.map.into_iter())
    }
}
