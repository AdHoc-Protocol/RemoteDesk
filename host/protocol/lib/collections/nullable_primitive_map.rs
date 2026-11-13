//! Hash map for cheap `Copy` keys with a separate value for the `None` key.
//!
//! Keyed entries use [`PrimitiveMap`](super::PrimitiveMap), with separate key/value arrays and
//! per-bucket hash filters. The optional key does not occupy a hash-table entry.

use super::lohi::{RawIter, RawIterMut, RawTable};
use super::token::{or_null, Token, NULL_TOKEN};
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use core::mem;
use rustc_hash::FxBuildHasher;

pub struct NullablePrimitiveMap<K, V, S = FxBuildHasher> {
    raw: RawTable<K, V>,
    null_value: Option<V>,
    hasher: S,
}

impl<K, V, S> NullablePrimitiveMap<K, V, S> {
    /// Keyed lo/hi slots first, then `NULL_TOKEN`; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> {
        or_null(self.raw.token(), self.null_value.is_some())
    }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        if token == NULL_TOKEN || !self.raw.contains_token(token) {
            return None;
        }
        or_null(self.raw.next_token(token), self.null_value.is_some())
    }

    /// Checks slot occupancy, without detecting a stale or foreign token.
    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        if token == NULL_TOKEN {
            self.null_value.is_some()
        } else {
            self.raw.contains_token(token)
        }
    }

    /// Direct access without hashing. The inner `None` key denotes the null slot.
    #[inline(always)]
    pub fn entry_by_token(&self, token: Token) -> Option<(Option<K>, &V)>
    where
        K: Copy,
    {
        if token == NULL_TOKEN {
            return self.null_value.as_ref().map(|v| (None, v));
        }
        self.raw.get_by_token(token).map(|(k, v)| (Some(*k), v))
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<Option<K>>
    where
        K: Copy,
    {
        self.entry_by_token(token).map(|(k, _)| k)
    }

    #[inline(always)]
    pub fn value_by_token(&self, token: Token) -> Option<&V> {
        if token == NULL_TOKEN {
            return self.null_value.as_ref();
        }
        self.raw.get_by_token(token).map(|(_, v)| v)
    }

    #[inline(always)]
    pub fn value_by_token_mut(&mut self, token: Token) -> Option<&mut V> {
        if token == NULL_TOKEN {
            return self.null_value.as_mut();
        }
        self.raw.get_by_token_mut(token)
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.raw.capacity()
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.raw.len() + self.null_value.is_some() as usize
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `n`-th entry of a non-`None` key in the order of [`Self::iter`], O(1): the pass by
    /// number of a pack being sent.
    #[inline]
    pub fn entry_at(&self, n: usize) -> (&K, &V) {
        self.raw.at(n)
    }

    /// The value of the `None` key.
    #[inline(always)]
    pub fn null_value(&self) -> Option<&V> {
        self.null_value.as_ref()
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            null: self.null_value.as_ref(),
            inner: self.raw.iter(),
        }
    }

    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut {
            null: self.null_value.as_mut(),
            inner: self.raw.iter_mut(),
        }
    }

    #[inline]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = Option<K>> + FusedIterator + '_
    where
        K: Copy,
    {
        self.iter().map(|(k, _)| k)
    }

    #[inline]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + FusedIterator + '_
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

impl<K: Copy + Eq + Hash, V> NullablePrimitiveMap<K, V, FxBuildHasher> {
    #[inline]
    pub fn new() -> Self {
        Self::with_hasher(FxBuildHasher)
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, FxBuildHasher)
    }
}

impl<K: Copy + Eq + Hash, V> Default for NullablePrimitiveMap<K, V, FxBuildHasher> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq + Hash, V, S: BuildHasher> NullablePrimitiveMap<K, V, S> {
    /// Lookup once; `None` uses the separate null slot.
    #[inline(always)]
    pub fn token_of(&self, key: Option<K>) -> Option<Token> {
        match key {
            Some(k) => self.find(k),
            None => self.null_value.is_some().then_some(NULL_TOKEN),
        }
    }

    /// Does not allocate.
    #[inline]
    pub fn with_hasher(hasher: S) -> Self {
        Self {
            raw: RawTable::new(),
            null_value: None,
            hasher,
        }
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self {
            raw: RawTable::with_capacity(capacity),
            null_value: None,
            hasher,
        }
    }

    /// Removes all entries, keeping the memory; a few entries in a large table cost themselves, not
    /// the capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.null_value = None;
        let hasher = &self.hasher;
        self.raw.clear_by(|k| hasher.hash_one(*k));
    }

    #[inline(always)]
    fn hash(&self, k: K) -> u64 {
        self.hasher.hash_one(k)
    }

    // lookup

    #[inline(always)]
    fn find(&self, k: K) -> Option<usize> {
        self.raw.find(self.hash(k), |e| *e == k)
    }

    #[inline(always)]
    pub fn get_key(&self, k: K) -> Option<&V> {
        match self.raw.get(self.hash(k), |e| *e == k) {
            Some((_, v)) => Some(v),
            None => None,
        }
    }

    #[inline(always)]
    pub fn get_key_mut(&mut self, k: K) -> Option<&mut V> {
        let h = self.hash(k);
        self.raw.get_mut(h, |e| *e == k)
    }

    #[inline(always)]
    pub fn get(&self, key: Option<K>) -> Option<&V> {
        match key {
            Some(k) => self.get_key(k),
            None => self.null_value.as_ref(),
        }
    }

    #[inline(always)]
    pub fn get_mut(&mut self, key: Option<K>) -> Option<&mut V> {
        match key {
            Some(k) => self.get_key_mut(k),
            None => self.null_value.as_mut(),
        }
    }

    #[inline(always)]
    pub fn contains_key(&self, key: Option<K>) -> bool {
        match key {
            Some(k) => self.find(k).is_some(),
            None => self.null_value.is_some(),
        }
    }

    // insertion

    /// Key without `Option`, the null key separately: the serializer and the parser neither wrap a key nor check it for null.
    #[inline(always)]
    pub fn insert_null_key(&mut self, value: V) -> Option<V> {
        self.null_value.replace(value)
    }

    #[inline(always)]
    pub fn insert(&mut self, key: Option<K>, value: V) -> Option<V> {
        match key {
            Some(k) => self.insert_key(k, value),
            None => self.null_value.replace(value),
        }
    }

    /// Inserts or replaces the value of `key` and gives the token of its entry (`NULL_TOKEN` for
    /// `None`): the value is then reached by [`Self::value_by_token_mut`] without the key (a value
    /// received in parts). Valid until the next insertion.
    #[inline]
    pub fn insert_token(&mut self, key: Option<K>, value: V) -> Token {
        let k = match key {
            Some(k) => k,
            None => {
                self.null_value = Some(value);
                return NULL_TOKEN;
            }
        };
        let h = self.hash(k);
        if let Some(i) = self.raw.find_for_insert(h, |e| *e == k) {
            *unsafe { self.raw.at_mut(i) } = value;
            return i;
        }
        let hasher = &self.hasher;
        self.raw.insert_unique_token(h, k, value, |e| hasher.hash_one(*e))
    }

    #[inline]
    pub fn insert_key(&mut self, k: K, value: V) -> Option<V> {
        let h = self.hash(k);
        if let Some(i) = self.raw.find_for_insert(h, |e| *e == k) {
            return Some(mem::replace(unsafe { self.raw.at_mut(i) }, value));
        }
        let hasher = &self.hasher;
        self.raw.insert_unique(h, k, value, |e| hasher.hash_one(*e));
        None
    }

    /// Returns a reference to the value, inserting `f()` if the key is absent.
    #[inline]
    pub fn get_or_insert_with<F: FnOnce() -> V>(&mut self, key: Option<K>, f: F) -> &mut V {
        let k = match key {
            Some(k) => k,
            None => return self.null_value.get_or_insert_with(f),
        };
        let h = self.hash(k);
        let hasher = &self.hasher;
        self.raw.find_or_insert(h, |e| *e == k, || (k, f()), |e| hasher.hash_one(*e)).1
    }

    pub fn reserve(&mut self, additional: usize) {
        let hasher = &self.hasher;
        self.raw.reserve(additional, |e| hasher.hash_one(*e));
    }

    pub fn shrink_to_fit(&mut self) {
        let hasher = &self.hasher;
        self.raw.shrink_to_fit(|e| hasher.hash_one(*e));
    }

    // removal

    #[inline(always)]
    pub fn remove(&mut self, key: Option<K>) -> Option<V> {
        match key {
            Some(k) => self.remove_key(k),
            None => self.null_value.take(),
        }
    }

    #[inline]
    pub fn remove_key(&mut self, k: K) -> Option<V> {
        let h = self.hash(k);
        let hasher = &self.hasher;
        match self.raw.remove(h, |e| *e == k, |e| hasher.hash_one(*e)) {
            Some((_, v)) => Some(v),
            None => None,
        }
    }

    /// Keeps only the entries for which `f` returns `true`.
    pub fn retain<F: FnMut(Option<K>, &mut V) -> bool>(&mut self, mut f: F) {
        if let Some(v) = self.null_value.as_mut() {
            if !f(None, v) {
                self.null_value = None;
            }
        }
        let hasher = &self.hasher;
        self.raw.retain(|k, v| f(Some(*k), v), |e| hasher.hash_one(*e));
    }
}

impl<K: Copy, V: Clone, S: Clone> Clone for NullablePrimitiveMap<K, V, S> {
    /// Same capacity and the same layout: no rehashing.
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            null_value: self.null_value.clone(),
            hasher: self.hasher.clone(),
        }
    }
}

/// Equal key sets with equal values, the null key included; layout and capacity do not count.
impl<K: Copy + Eq + Hash, V: PartialEq, S: BuildHasher> PartialEq for NullablePrimitiveMap<K, V, S> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl<K: Copy + Eq + Hash, V: Eq, S: BuildHasher> Eq for NullablePrimitiveMap<K, V, S> {}

impl<K: Copy + fmt::Debug, V: fmt::Debug, S> fmt::Debug for NullablePrimitiveMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Copy + Eq + Hash, V, S: BuildHasher> Extend<(Option<K>, V)> for NullablePrimitiveMap<K, V, S> {
    fn extend<I: IntoIterator<Item = (Option<K>, V)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: Copy + Eq + Hash, V, S: BuildHasher + Default> FromIterator<(Option<K>, V)> for NullablePrimitiveMap<K, V, S> {
    fn from_iter<I: IntoIterator<Item = (Option<K>, V)>>(iter: I) -> Self {
        let mut m = Self::with_hasher(S::default());
        m.extend(iter);
        m
    }
}

// iterators

/// Order: the `None` key, then the keys in the order of the table (lo, then hi).
pub struct Iter<'a, K, V> {
    null: Option<&'a V>,
    inner: RawIter<'a, K, V>,
}

impl<'a, K: Copy, V> Iterator for Iter<'a, K, V> {
    type Item = (Option<K>, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(v) = self.null.take() {
            return Some((None, v));
        }
        self.inner.next().map(|(k, v)| (Some(*k), v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.inner.len() + self.null.is_some() as usize;
        (n, Some(n))
    }

    #[inline]
    fn fold<B, F: FnMut(B, Self::Item) -> B>(self, init: B, mut f: F) -> B {
        let mut acc = init;
        if let Some(v) = self.null {
            acc = f(acc, (None, v));
        }
        self.inner.fold(acc, |acc, (k, v)| f(acc, (Some(*k), v)))
    }
}

impl<K: Copy, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K: Copy, V> FusedIterator for Iter<'_, K, V> {}

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Self {
            null: self.null,
            inner: self.inner.clone(),
        }
    }
}

pub struct IterMut<'a, K, V> {
    null: Option<&'a mut V>,
    inner: RawIterMut<'a, K, V>,
}

impl<'a, K: Copy, V> Iterator for IterMut<'a, K, V> {
    type Item = (Option<K>, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(v) = self.null.take() {
            return Some((None, v));
        }
        self.inner.next().map(|(k, v)| (Some(*k), v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.inner.len() + self.null.is_some() as usize;
        (n, Some(n))
    }
}

impl<K: Copy, V> ExactSizeIterator for IterMut<'_, K, V> {}
impl<K: Copy, V> FusedIterator for IterMut<'_, K, V> {}

impl<'a, K: Copy, V, S> IntoIterator for &'a NullablePrimitiveMap<K, V, S> {
    type Item = (Option<K>, &'a V);
    type IntoIter = Iter<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, K: Copy, V, S> IntoIterator for &'a mut NullablePrimitiveMap<K, V, S> {
    type Item = (Option<K>, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

#[cfg(test)]
#[path = "nullable_primitive_map_filter_tests.rs"]
mod filter_tests;
