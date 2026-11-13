//! Hash set for cheap `Copy` keys using the split table in `lohi.rs`.
//!
//! Keys are passed by value and rehashed during relocation. The maximum table load factor is 0.5;
//! `retain` rebuilds in O(capacity). For expensive keys, use [`NullableSet`](super::NullableSet),
//! which caches hashes.

use super::lohi::{RawIntoIter, RawIter, RawTable};
use super::Token;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use rustc_hash::FxBuildHasher;

pub struct PrimitiveSet<K, S = FxBuildHasher> {
    raw: RawTable<K>,
    hasher: S,
}

impl<K, S> PrimitiveSet<K, S> {
    /// First physical slot; see [`super::token`] for validity and iteration order.
    #[inline(always)]
    pub fn token(&self) -> Option<Token> { self.raw.token() }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> { self.raw.next_token(token) }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool { self.raw.contains_token(token) }

    /// Direct slot access without hashing; differs from ordinal `key_at`.
    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<K> where K: Copy {
        self.raw.get_by_token(token).map(|(k, _)| *k)
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.raw.len() == 0
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.raw.capacity()
    }

    #[inline]
    pub fn hasher(&self) -> &S {
        &self.hasher
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, K> {
        Iter(self.raw.iter())
    }

    /// The `n`-th key in the order of [`Self::iter`], O(1): the pass by number of a pack being
    /// sent.
    #[inline]
    pub fn key_at(&self, n: usize) -> K
    where
        K: Copy,
    {
        *self.raw.at(n).0
    }
}

impl<K: Copy + Eq + Hash> PrimitiveSet<K, FxBuildHasher> {
    #[inline]
    pub fn new() -> Self {
        Self::with_hasher(FxBuildHasher)
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, FxBuildHasher)
    }
}

impl<K: Copy + Eq + Hash> Default for PrimitiveSet<K, FxBuildHasher> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher> PrimitiveSet<K, S> {
    /// Removes all keys, keeping the memory; a few keys in a large table cost themselves, not the
    /// capacity.
    #[inline]
    pub fn clear(&mut self) {
        let hasher = &self.hasher;
        self.raw.clear_by(|e| hasher.hash_one(*e));
    }

    #[inline(always)]
    pub fn token_of(&self, key: K) -> Option<Token> {
        self.raw.find(self.hash(key), |e| *e == key)
    }

    /// Does not allocate.
    #[inline]
    pub fn with_hasher(hasher: S) -> Self {
        Self { raw: RawTable::new(), hasher }
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self { raw: RawTable::with_capacity(capacity), hasher }
    }

    #[inline(always)]
    fn hash(&self, k: K) -> u64 {
        self.hasher.hash_one(k)
    }

    #[inline(always)]
    pub fn contains(&self, k: K) -> bool {
        self.raw.find(self.hash(k), |e| *e == k).is_some()
    }

    /// `true` if the key was not present.
    #[inline]
    pub fn insert(&mut self, k: K) -> bool {
        let h = self.hash(k);
        if self.raw.find_for_insert(h, |e| *e == k).is_some() {
            return false;
        }
        let hasher = &self.hasher;
        self.raw.insert_unique(h, k, (), |e| hasher.hash_one(*e));
        true
    }

    /// `true` if the key was present.
    #[inline]
    pub fn remove(&mut self, k: K) -> bool {
        let h = self.hash(k);
        let hasher = &self.hasher;
        self.raw.remove(h, |e| *e == k, |e| hasher.hash_one(*e)).is_some()
    }

    pub fn reserve(&mut self, additional: usize) {
        let hasher = &self.hasher;
        self.raw.reserve(additional, |e| hasher.hash_one(*e));
    }

    pub fn shrink_to_fit(&mut self) {
        let hasher = &self.hasher;
        self.raw.shrink_to_fit(|e| hasher.hash_one(*e));
    }

    /// Keeps only the keys for which `f` returns `true`.
    pub fn retain<F: FnMut(K) -> bool>(&mut self, mut f: F) {
        let hasher = &self.hasher;
        self.raw.retain(|e, _| f(*e), |e| hasher.hash_one(*e));
    }

    // set algebra

    /// Removes the keys absent from `other`.
    pub fn intersect_with<S2: BuildHasher>(&mut self, other: &PrimitiveSet<K, S2>) {
        if self.is_empty() {
            return;
        }
        if other.is_empty() {
            self.clear();
            return;
        }
        self.retain(|k| other.contains(k));
    }

    /// Removes the keys present in `other`.
    pub fn difference_with<I: IntoIterator<Item = K>>(&mut self, other: I) {
        for k in other {
            if self.is_empty() {
                return;
            }
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

    pub fn is_subset<S2: BuildHasher>(&self, other: &PrimitiveSet<K, S2>) -> bool {
        self.len() <= other.len() && self.iter().all(|k| other.contains(k))
    }

    pub fn is_superset<S2: BuildHasher>(&self, other: &PrimitiveSet<K, S2>) -> bool {
        other.is_subset(self)
    }

    /// Walks the smaller set.
    pub fn is_disjoint<S2: BuildHasher>(&self, other: &PrimitiveSet<K, S2>) -> bool {
        if self.len() <= other.len() {
            self.iter().all(|k| !other.contains(k))
        } else {
            other.iter().all(|k| !self.contains(k))
        }
    }
}

impl<K: Clone, S: Clone> Clone for PrimitiveSet<K, S> {
    fn clone(&self) -> Self {
        Self { raw: self.raw.clone(), hasher: self.hasher.clone() }
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher> PartialEq for PrimitiveSet<K, S> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|k| other.contains(k))
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher> Eq for PrimitiveSet<K, S> {}

impl<K: Copy + fmt::Debug, S> fmt::Debug for PrimitiveSet<K, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher> Extend<K> for PrimitiveSet<K, S> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for k in iter {
            self.insert(k);
        }
    }
}

impl<'a, K: Copy + Eq + Hash + 'a, S: BuildHasher> Extend<&'a K> for PrimitiveSet<K, S> {
    fn extend<I: IntoIterator<Item = &'a K>>(&mut self, iter: I) {
        self.extend(iter.into_iter().copied());
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher + Default> FromIterator<K> for PrimitiveSet<K, S> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        let mut s = Self::with_hasher(S::default());
        s.extend(iter);
        s
    }
}

impl<K: Copy + Eq + Hash, const N: usize> From<[K; N]> for PrimitiveSet<K, FxBuildHasher> {
    fn from(keys: [K; N]) -> Self {
        keys.into_iter().collect()
    }
}

// iterators

/// Order: the lo region, then the hi region.
#[derive(Clone)]
pub struct Iter<'a, K>(RawIter<'a, K, ()>);

impl<K: Copy> Iterator for Iter<'_, K> {
    type Item = K;

    #[inline]
    fn next(&mut self) -> Option<K> {
        self.0.next().map(|(k, _)| *k)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn fold<B, F: FnMut(B, K) -> B>(self, init: B, mut f: F) -> B {
        self.0.fold(init, |acc, (k, _)| f(acc, *k))
    }
}

impl<K: Copy> ExactSizeIterator for Iter<'_, K> {}
impl<K: Copy> FusedIterator for Iter<'_, K> {}

pub struct IntoIter<K>(RawIntoIter<K, ()>);

impl<K> Iterator for IntoIter<K> {
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

impl<K> ExactSizeIterator for IntoIter<K> {}
impl<K> FusedIterator for IntoIter<K> {}

impl<'a, K: Copy, S> IntoIterator for &'a PrimitiveSet<K, S> {
    type Item = K;
    type IntoIter = Iter<'a, K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K, S> IntoIterator for PrimitiveSet<K, S> {
    type Item = K;
    type IntoIter = IntoIter<K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter(self.raw.into_iter())
    }
}
