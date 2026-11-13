//! Hash map for cheap `Copy` keys using separate key and value arrays.
//!
//! Keys are passed by value and rehashed during relocation. A per-bucket filter rejects missing
//! hash classes, and the maximum table load factor is 0.5. `retain` rebuilds the table in
//! O(capacity). For expensive keys use [`Map`](super::Map), which caches hashes; for a `None` key
//! use [`NullablePrimitiveMap`](super::NullablePrimitiveMap).

use super::lohi::{RawIntoIter, RawIter, RawIterMut, RawTable};
use super::Token;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use core::mem;
use core::ops::Index;
use rustc_hash::FxBuildHasher;

pub struct PrimitiveMap<K, V, S = FxBuildHasher> {
    raw: RawTable<K, V>,
    hasher: S,
}

impl<K, V, S> PrimitiveMap<K, V, S> {
    /// First physical slot; see [`super::token`] for validity and iteration order.
    #[inline(always)]
    pub fn token(&self) -> Option<Token> { self.raw.token() }

    /// Next live slot, skipping the gap between lo and hi.
    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> { self.raw.next_token(token) }

    /// Checks slot occupancy, without detecting a stale or foreign token.
    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool { self.raw.contains_token(token) }

    /// Direct slot access without hashing; token is not an ordinal `entry_at` index.
    #[inline(always)]
    pub fn entry_by_token(&self, token: Token) -> Option<(K, &V)> where K: Copy {
        self.raw.get_by_token(token).map(|(k, v)| (*k, v))
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<K> where K: Copy {
        self.raw.get_by_token(token).map(|(k, _)| *k)
    }

    #[inline(always)]
    pub fn value_by_token(&self, token: Token) -> Option<&V> {
        self.raw.get_by_token(token).map(|(_, v)| v)
    }

    /// Changing a value preserves the token and traversal position.
    #[inline(always)]
    pub fn value_by_token_mut(&mut self, token: Token) -> Option<&mut V> {
        self.raw.get_by_token_mut(token)
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
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter(self.raw.iter())
    }

    /// The `n`-th entry in the order of [`Self::iter`], O(1): the pass by number of a pack being
    /// sent.
    #[inline]
    pub fn entry_at(&self, n: usize) -> (&K, &V) {
        self.raw.at(n)
    }

    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut(self.raw.iter_mut())
    }

    #[inline]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = K> + FusedIterator + Clone + '_
    where
        K: Copy,
    {
        self.iter().map(|(k, _)| k)
    }

    #[inline]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + FusedIterator + Clone + '_
    where
        K: Copy,
    {
        self.iter().map(|(_, v)| v)
    }

    #[inline]
    pub fn values_mut(&mut self) -> impl ExactSizeIterator<Item = &mut V> + FusedIterator + '_
    where
        K: Copy,
    {
        self.iter_mut().map(|(_, v)| v)
    }
}

impl<K: Copy + Eq + Hash, V> PrimitiveMap<K, V, FxBuildHasher> {
    #[inline]
    pub fn new() -> Self {
        Self::with_hasher(FxBuildHasher)
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, FxBuildHasher)
    }
}

impl<K: Copy + Eq + Hash, V> Default for PrimitiveMap<K, V, FxBuildHasher> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq + Hash, V, S: BuildHasher> PrimitiveMap<K, V, S> {
    /// Removes all entries, keeping the memory; a few entries in a large table cost themselves, not
    /// the capacity.
    #[inline]
    pub fn clear(&mut self) {
        let hasher = &self.hasher;
        self.raw.clear_by(|k| hasher.hash_one(*k));
    }

    /// Finds the physical slot once; subsequent token access does not hash the key.
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
    pub fn get(&self, k: K) -> Option<&V> {
        match self.raw.get(self.hash(k), |e| *e == k) {
            Some((_, v)) => Some(v),
            None => None,
        }
    }

    #[inline(always)]
    pub fn get_mut(&mut self, k: K) -> Option<&mut V> {
        let h = self.hash(k);
        self.raw.get_mut(h, |e| *e == k)
    }

    #[inline(always)]
    pub fn contains_key(&self, k: K) -> bool {
        self.raw.find(self.hash(k), |e| *e == k).is_some()
    }

    /// Returns the previous value of the key.
    #[inline]
    pub fn insert(&mut self, k: K, value: V) -> Option<V> {
        let h = self.hash(k);
        if let Some(i) = self.raw.find_for_insert(h, |e| *e == k) {
            return Some(mem::replace(unsafe { self.raw.at_mut(i) }, value));
        }
        let hasher = &self.hasher;
        self.raw.insert_unique(h, k, value, |e| hasher.hash_one(*e));
        None
    }

    /// Inserts or replaces the value of `k` and gives the token of its entry: the value is then
    /// reached by [`Self::value_by_token_mut`] without the key (a value received in parts). Valid
    /// until the next insertion.
    #[inline]
    pub fn insert_token(&mut self, k: K, value: V) -> Token {
        let h = self.hash(k);
        if let Some(i) = self.raw.find_for_insert(h, |e| *e == k) {
            *unsafe { self.raw.at_mut(i) } = value;
            return i;
        }
        let hasher = &self.hasher;
        self.raw.insert_unique_token(h, k, value, |e| hasher.hash_one(*e))
    }

    /// Returns a reference to the value, inserting `f()` if the key is absent.
    #[inline]
    pub fn get_or_insert_with<F: FnOnce() -> V>(&mut self, k: K, f: F) -> &mut V {
        let h = self.hash(k);
        let hasher = &self.hasher;
        self.raw.find_or_insert(h, |e| *e == k, || (k, f()), |e| hasher.hash_one(*e)).1
    }

    #[inline]
    pub fn remove(&mut self, k: K) -> Option<V> {
        self.remove_entry(k).map(|(_, v)| v)
    }

    #[inline]
    pub fn remove_entry(&mut self, k: K) -> Option<(K, V)> {
        let h = self.hash(k);
        let hasher = &self.hasher;
        self.raw.remove(h, |e| *e == k, |e| hasher.hash_one(*e))
    }

    pub fn reserve(&mut self, additional: usize) {
        let hasher = &self.hasher;
        self.raw.reserve(additional, |e| hasher.hash_one(*e));
    }

    pub fn shrink_to_fit(&mut self) {
        let hasher = &self.hasher;
        self.raw.shrink_to_fit(|e| hasher.hash_one(*e));
    }

    /// Keeps only the entries for which `f` returns `true`.
    pub fn retain<F: FnMut(K, &mut V) -> bool>(&mut self, mut f: F) {
        let hasher = &self.hasher;
        self.raw.retain(|k, v| f(*k, v), |e| hasher.hash_one(*e));
    }
}

impl<K: Clone, V: Clone, S: Clone> Clone for PrimitiveMap<K, V, S> {
    fn clone(&self) -> Self {
        Self { raw: self.raw.clone(), hasher: self.hasher.clone() }
    }
}

impl<K: Copy + Eq + Hash, V: PartialEq, S: BuildHasher> PartialEq for PrimitiveMap<K, V, S> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl<K: Copy + Eq + Hash, V: Eq, S: BuildHasher> Eq for PrimitiveMap<K, V, S> {}

impl<K: Copy + fmt::Debug, V: fmt::Debug, S> fmt::Debug for PrimitiveMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Panics if the key is absent.
impl<K: Copy + Eq + Hash, V, S: BuildHasher> Index<K> for PrimitiveMap<K, V, S> {
    type Output = V;
    #[inline]
    fn index(&self, k: K) -> &V {
        self.get(k).expect("key not found")
    }
}

impl<K: Copy + Eq + Hash, V, S: BuildHasher> Extend<(K, V)> for PrimitiveMap<K, V, S> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: Copy + Eq + Hash, V, S: BuildHasher + Default> FromIterator<(K, V)> for PrimitiveMap<K, V, S> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut m = Self::with_hasher(S::default());
        m.extend(iter);
        m
    }
}

impl<K: Copy + Eq + Hash, V, const N: usize> From<[(K, V); N]> for PrimitiveMap<K, V, FxBuildHasher> {
    fn from(entries: [(K, V); N]) -> Self {
        entries.into_iter().collect()
    }
}

// iterators

/// Order: the lo region, then the hi region.
pub struct Iter<'a, K, V>(RawIter<'a, K, V>);

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<'a, K: Copy, V> Iterator for Iter<'a, K, V> {
    type Item = (K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, v)| (*k, v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn fold<B, F: FnMut(B, Self::Item) -> B>(self, init: B, mut f: F) -> B {
        self.0.fold(init, |acc, (k, v)| f(acc, (*k, v)))
    }
}

impl<K: Copy, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K: Copy, V> FusedIterator for Iter<'_, K, V> {}

pub struct IterMut<'a, K, V>(RawIterMut<'a, K, V>);

impl<'a, K: Copy, V> Iterator for IterMut<'a, K, V> {
    type Item = (K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, v)| (*k, v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K: Copy, V> ExactSizeIterator for IterMut<'_, K, V> {}
impl<K: Copy, V> FusedIterator for IterMut<'_, K, V> {}

pub struct IntoIter<K, V>(RawIntoIter<K, V>);

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<(K, V)> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V> ExactSizeIterator for IntoIter<K, V> {}
impl<K, V> FusedIterator for IntoIter<K, V> {}

impl<'a, K: Copy, V, S> IntoIterator for &'a PrimitiveMap<K, V, S> {
    type Item = (K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, K: Copy, V, S> IntoIterator for &'a mut PrimitiveMap<K, V, S> {
    type Item = (K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<K, V, S> IntoIterator for PrimitiveMap<K, V, S> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter(self.raw.into_iter())
    }
}
