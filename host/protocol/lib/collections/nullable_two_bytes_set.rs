//! [`TwoBytesSet`] with separate storage for a `None` key.
//!
//! Ordinary iteration visits the optional entry first. Token traversal uses the ordering documented
//! in [`super::token`].

use super::keys::TwoBytesKey;
use super::token::{or_null, Token, NULL_TOKEN};
use super::two_bytes_set::{self, TwoBytesSet};
use core::fmt;
use core::iter::FusedIterator;

pub struct NullableTwoBytesSet<K = u16> {
    set: TwoBytesSet<K>,
    has_null: bool,
}

impl<K> NullableTwoBytesSet<K> {
    /// Keyed entries first, then `NULL_TOKEN`; see [`super::token`].
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

    /// Does not allocate.
    #[inline]
    pub fn new() -> Self {
        Self { set: TwoBytesSet::new(), has_null: false }
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { set: TwoBytesSet::with_capacity(capacity), has_null: false }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.set.len() + self.has_null as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.has_null && self.set.is_empty()
    }

    #[inline]
    pub fn is_flat(&self) -> bool {
        self.set.is_flat()
    }

    /// The non-`None` keys.
    #[inline]
    pub fn keyed(&self) -> &TwoBytesSet<K> {
        &self.set
    }

    #[inline]
    pub fn clear(&mut self) {
        self.has_null = false;
        self.set.clear();
    }

    /// `None` first.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K> {
        Iter { null: self.has_null, inner: self.set.iter() }
    }
}

impl<K> Default for NullableTwoBytesSet<K> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: TwoBytesKey> NullableTwoBytesSet<K> {
    #[inline(always)]
    pub fn token_of(&self, key: Option<K>) -> Option<Token> {
        match key { Some(k) => self.set.token_of(k), None => self.has_null.then_some(NULL_TOKEN) }
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<Option<K>> {
        if token == NULL_TOKEN { self.has_null.then_some(None) }
        else { self.set.key_by_token(token).map(Some) }
    }

    /// The `n`-th non-`None` key of a pass key after key (the generated sending), see
    /// [`TwoBytesSet::key_seq`].
    ///
    /// # Safety
    /// As [`TwoBytesSet::key_seq`].
    #[inline]
    pub unsafe fn key_seq(&self, n: usize, at: &mut super::token::Seq) -> K {
        // SAFETY: the caller keeps the contract of this pass, the same for the keyed set
        unsafe { self.set.key_seq(n, at) }
    }

    #[inline(always)]
    pub fn contains_null(&self) -> bool {
        self.has_null
    }

    /// `true` if `None` was not present.
    #[inline(always)]
    pub fn insert_null(&mut self) -> bool {
        !core::mem::replace(&mut self.has_null, true)
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
    #[inline(always)]
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

    /// Keeps only the keys for which `f` returns `true`.
    pub fn retain<F: FnMut(Option<K>) -> bool>(&mut self, mut f: F) {
        if self.has_null && !f(None) {
            self.has_null = false;
        }
        self.set.retain(|k| f(Some(k)));
    }

    pub fn reserve(&mut self, additional: usize) {
        self.set.reserve(additional);
    }

    pub fn shrink_to_fit(&mut self) {
        self.set.shrink_to_fit();
    }

    // set algebra

    /// Removes the keys absent from `other`.
    pub fn intersect_with(&mut self, other: &Self) {
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

    pub fn is_subset(&self, other: &Self) -> bool {
        (!self.has_null || other.has_null) && self.set.is_subset(&other.set)
    }

    pub fn is_superset(&self, other: &Self) -> bool {
        other.is_subset(self)
    }

    pub fn is_disjoint(&self, other: &Self) -> bool {
        !(self.has_null && other.has_null) && self.set.is_disjoint(&other.set)
    }
}

impl<K> Clone for NullableTwoBytesSet<K> {
    fn clone(&self) -> Self {
        Self { set: self.set.clone(), has_null: self.has_null }
    }
}

impl<K: TwoBytesKey> PartialEq for NullableTwoBytesSet<K> {
    fn eq(&self, other: &Self) -> bool {
        self.has_null == other.has_null && self.set == other.set
    }
}

impl<K: TwoBytesKey> Eq for NullableTwoBytesSet<K> {}

impl<K: TwoBytesKey + fmt::Debug> fmt::Debug for NullableTwoBytesSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<K: TwoBytesKey> Extend<Option<K>> for NullableTwoBytesSet<K> {
    fn extend<I: IntoIterator<Item = Option<K>>>(&mut self, iter: I) {
        for k in iter {
            self.insert(k);
        }
    }
}

impl<K: TwoBytesKey> FromIterator<Option<K>> for NullableTwoBytesSet<K> {
    fn from_iter<I: IntoIterator<Item = Option<K>>>(iter: I) -> Self {
        let mut s = Self::new();
        s.extend(iter);
        s
    }
}

// iterators

/// `None` first, then the keys.
pub struct Iter<'a, K> {
    null: bool,
    inner: two_bytes_set::Iter<'a, K>,
}

impl<K> Clone for Iter<'_, K> {
    fn clone(&self) -> Self {
        Self { null: self.null, inner: self.inner.clone() }
    }
}

impl<K: TwoBytesKey> Iterator for Iter<'_, K> {
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

impl<K: TwoBytesKey> ExactSizeIterator for Iter<'_, K> {}
impl<K: TwoBytesKey> FusedIterator for Iter<'_, K> {}

pub struct IntoIter<K> {
    null: bool,
    inner: two_bytes_set::IntoIter<K>,
}

impl<K: TwoBytesKey> Iterator for IntoIter<K> {
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

impl<K: TwoBytesKey> ExactSizeIterator for IntoIter<K> {}
impl<K: TwoBytesKey> FusedIterator for IntoIter<K> {}

impl<'a, K: TwoBytesKey> IntoIterator for &'a NullableTwoBytesSet<K> {
    type Item = Option<K>;
    type IntoIter = Iter<'a, K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: TwoBytesKey> IntoIterator for NullableTwoBytesSet<K> {
    type Item = Option<K>;
    type IntoIter = IntoIter<K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter { null: self.has_null, inner: self.set.into_iter() }
    }
}
