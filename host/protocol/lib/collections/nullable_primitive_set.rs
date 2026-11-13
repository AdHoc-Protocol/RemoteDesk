//! [`PrimitiveSet`] with separate storage for a `None` key.
//!
//! Ordinary iteration visits the optional entry first. Token traversal uses the ordering documented
//! in [`super::token`].

use super::primitive_set::{self, PrimitiveSet};
use super::token::{or_null, Token, NULL_TOKEN};
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use rustc_hash::FxBuildHasher;

pub struct NullablePrimitiveSet<K, S = FxBuildHasher> {
    set: PrimitiveSet<K, S>,
    has_null: bool,
}

impl<K, S> NullablePrimitiveSet<K, S> {
    /// Keyed slots first, then `NULL_TOKEN`; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> { or_null(self.set.token(), self.has_null) }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        if token == NULL_TOKEN || !self.set.contains_token(token) { return None; }
        or_null(self.set.next_token(token), self.has_null)
    }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        if token == NULL_TOKEN { self.has_null } else { self.set.contains_token(token) }
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<Option<K>> where K: Copy {
        if token == NULL_TOKEN { self.has_null.then_some(None) }
        else { self.set.key_by_token(token).map(Some) }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.set.len() + self.has_null as usize
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.set.capacity()
    }

    #[inline]
    pub fn hasher(&self) -> &S {
        self.set.hasher()
    }

    /// The non-`None` keys.
    #[inline]
    pub fn keyed(&self) -> &PrimitiveSet<K, S> {
        &self.set
    }

    /// The `n`-th non-`None` key, O(1): the pass by number of a pack being sent.
    #[inline]
    pub fn key_at(&self, n: usize) -> K
    where
        K: Copy,
    {
        self.set.key_at(n)
    }

    #[inline(always)]
    pub fn contains_null(&self) -> bool {
        self.has_null
    }

    /// `true` if the `None` key was not present.
    #[inline]
    pub fn insert_null(&mut self) -> bool {
        !core::mem::replace(&mut self.has_null, true)
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, K> {
        Iter { null: self.has_null, inner: self.set.iter() }
    }
}

impl<K: Copy + Eq + Hash> NullablePrimitiveSet<K, FxBuildHasher> {
    #[inline]
    pub fn new() -> Self {
        Self::with_hasher(FxBuildHasher)
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, FxBuildHasher)
    }
}

impl<K: Copy + Eq + Hash> Default for NullablePrimitiveSet<K, FxBuildHasher> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher> NullablePrimitiveSet<K, S> {
    /// Removes all keys, keeping the allocated memory.
    #[inline]
    pub fn clear(&mut self) {
        self.has_null = false;
        self.set.clear();
    }

    #[inline(always)]
    pub fn token_of(&self, key: Option<K>) -> Option<Token> {
        match key { Some(k) => self.set.token_of(k), None => self.has_null.then_some(NULL_TOKEN) }
    }

    /// Does not allocate.
    #[inline]
    pub fn with_hasher(hasher: S) -> Self {
        Self { set: PrimitiveSet::with_hasher(hasher), has_null: false }
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self { set: PrimitiveSet::with_capacity_and_hasher(capacity, hasher), has_null: false }
    }

    #[inline(always)]
    pub fn contains(&self, key: Option<K>) -> bool {
        match key {
            Some(k) => self.set.contains(k),
            None => self.has_null,
        }
    }

    /// Key without `Option`, the null key separately: the serializer and the parser neither wrap a key nor check it for null.
    #[inline(always)]
    pub fn insert_key(&mut self, k: K) -> bool { self.set.insert(k) }

    /// `true` if the key was not present.
    #[inline]
    pub fn insert(&mut self, key: Option<K>) -> bool {
        match key {
            Some(k) => self.set.insert(k),
            None => !core::mem::replace(&mut self.has_null, true),
        }
    }

    /// `true` if the key was present.
    #[inline]
    pub fn remove(&mut self, key: Option<K>) -> bool {
        match key {
            Some(k) => self.set.remove(k),
            None => core::mem::take(&mut self.has_null),
        }
    }

    pub fn reserve(&mut self, additional: usize) {
        self.set.reserve(additional);
    }

    pub fn shrink_to_fit(&mut self) {
        self.set.shrink_to_fit();
    }

    /// Keeps only the keys for which `f` returns `true`.
    pub fn retain<F: FnMut(Option<K>) -> bool>(&mut self, mut f: F) {
        if self.has_null && !f(None) {
            self.has_null = false;
        }
        self.set.retain(|k| f(Some(k)));
    }

    // set algebra

    /// Removes the keys absent from `other`.
    pub fn intersect_with<S2: BuildHasher>(&mut self, other: &NullablePrimitiveSet<K, S2>) {
        self.has_null &= other.has_null;
        self.set.intersect_with(&other.set);
    }

    /// Removes the keys present in `other`.
    pub fn difference_with<I: IntoIterator<Item = Option<K>>>(&mut self, other: I) {
        for k in other {
            self.remove(k);
        }
    }

    /// Toggles every key of `other`: removes it if present, inserts it otherwise.
    pub fn symmetric_difference_with<I: IntoIterator<Item = Option<K>>>(&mut self, other: I) {
        for k in other {
            if !self.remove(k) {
                self.insert(k);
            }
        }
    }

    pub fn is_subset<S2: BuildHasher>(&self, other: &NullablePrimitiveSet<K, S2>) -> bool {
        (!self.has_null || other.has_null) && self.set.is_subset(&other.set)
    }

    pub fn is_superset<S2: BuildHasher>(&self, other: &NullablePrimitiveSet<K, S2>) -> bool {
        other.is_subset(self)
    }

    pub fn is_disjoint<S2: BuildHasher>(&self, other: &NullablePrimitiveSet<K, S2>) -> bool {
        !(self.has_null && other.has_null) && self.set.is_disjoint(&other.set)
    }
}

impl<K: Clone, S: Clone> Clone for NullablePrimitiveSet<K, S> {
    fn clone(&self) -> Self {
        Self { set: self.set.clone(), has_null: self.has_null }
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher> PartialEq for NullablePrimitiveSet<K, S> {
    fn eq(&self, other: &Self) -> bool {
        self.has_null == other.has_null && self.set == other.set
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher> Eq for NullablePrimitiveSet<K, S> {}

impl<K: Copy + fmt::Debug, S> fmt::Debug for NullablePrimitiveSet<K, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher> Extend<Option<K>> for NullablePrimitiveSet<K, S> {
    fn extend<I: IntoIterator<Item = Option<K>>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for k in iter {
            self.insert(k);
        }
    }
}

impl<K: Copy + Eq + Hash, S: BuildHasher + Default> FromIterator<Option<K>> for NullablePrimitiveSet<K, S> {
    fn from_iter<I: IntoIterator<Item = Option<K>>>(iter: I) -> Self {
        let mut s = Self::with_hasher(S::default());
        s.extend(iter);
        s
    }
}

// iterators

/// Order: `None`, then the keys.
#[derive(Clone)]
pub struct Iter<'a, K> {
    null: bool,
    inner: primitive_set::Iter<'a, K>,
}

impl<K: Copy> Iterator for Iter<'_, K> {
    type Item = Option<K>;

    #[inline]
    fn next(&mut self) -> Option<Option<K>> {
        if self.null {
            self.null = false;
            return Some(None);
        }
        self.inner.next().map(Some)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.inner.len() + self.null as usize;
        (n, Some(n))
    }

    #[inline]
    fn fold<B, F: FnMut(B, Option<K>) -> B>(self, init: B, mut f: F) -> B {
        let mut acc = init;
        if self.null {
            acc = f(acc, None);
        }
        self.inner.fold(acc, |acc, k| f(acc, Some(k)))
    }
}

impl<K: Copy> ExactSizeIterator for Iter<'_, K> {}
impl<K: Copy> FusedIterator for Iter<'_, K> {}

pub struct IntoIter<K> {
    null: bool,
    inner: primitive_set::IntoIter<K>,
}

impl<K> Iterator for IntoIter<K> {
    type Item = Option<K>;

    #[inline]
    fn next(&mut self) -> Option<Option<K>> {
        if self.null {
            self.null = false;
            return Some(None);
        }
        self.inner.next().map(Some)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.inner.len() + self.null as usize;
        (n, Some(n))
    }
}

impl<K> ExactSizeIterator for IntoIter<K> {}
impl<K> FusedIterator for IntoIter<K> {}

impl<'a, K: Copy, S> IntoIterator for &'a NullablePrimitiveSet<K, S> {
    type Item = Option<K>;
    type IntoIter = Iter<'a, K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K, S> IntoIterator for NullablePrimitiveSet<K, S> {
    type Item = Option<K>;
    type IntoIter = IntoIter<K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter { null: self.has_null, inner: self.set.into_iter() }
    }
}
