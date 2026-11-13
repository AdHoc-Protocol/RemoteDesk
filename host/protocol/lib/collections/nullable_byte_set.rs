//! [`ByteSet`] with separate storage for a `None` key.
//!
//! Ordinary iteration visits the optional entry first, then keys in unsigned order. Token traversal
//! uses the ordering documented in [`super::token`].

use super::byte_set::{self, ByteSet};
use super::keys::ByteKey;
use super::token::{or_null, Token, NULL_TOKEN};
use core::fmt;
use core::hash::{Hash, Hasher};
use core::iter::FusedIterator;

pub struct NullableByteSet<K = u8> {
    set: ByteSet<K>,
    has_null: bool,
}

impl<K> Clone for NullableByteSet<K> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for NullableByteSet<K> {}

impl<K> Default for NullableByteSet<K> {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}

impl<K> PartialEq for NullableByteSet<K> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.has_null == other.has_null && self.set == other.set
    }
}

impl<K> Eq for NullableByteSet<K> {}

impl<K> Hash for NullableByteSet<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.set.hash(state);
        self.has_null.hash(state);
    }
}

impl<K> NullableByteSet<K> {
    /// Keyed entries first, then `NULL_TOKEN`; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> {
        or_null(self.set.token(), self.has_null)
    }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        if token == NULL_TOKEN || !self.set.contains_token(token) {
            return None;
        }
        or_null(self.set.next_token(token), self.has_null)
    }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        if token == NULL_TOKEN {
            self.has_null
        } else {
            self.set.contains_token(token)
        }
    }

    #[inline(always)]
    pub const fn new() -> Self {
        Self { set: ByteSet::new(), has_null: false }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.set.len() + self.has_null as usize
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        !self.has_null && self.set.is_empty()
    }

    #[inline(always)]
    pub fn clear(&mut self) {
        self.set.clear();
        self.has_null = false;
    }

    /// The non-`None` keys.
    #[inline(always)]
    pub fn keyed(&self) -> &ByteSet<K> {
        &self.set
    }

    #[inline(always)]
    pub fn keyed_mut(&mut self) -> &mut ByteSet<K> {
        &mut self.set
    }

    #[inline(always)]
    pub const fn contains_null(&self) -> bool {
        self.has_null
    }

    /// `true` if `None` was not present.
    #[inline(always)]
    pub fn insert_null(&mut self) -> bool {
        !core::mem::replace(&mut self.has_null, true)
    }

    /// Nothing to do: every key has its bit.
    #[inline(always)]
    pub fn reserve(&mut self, _additional: usize) {}

    #[inline]
    pub fn is_subset(&self, other: &Self) -> bool {
        (!self.has_null || other.has_null) && self.set.is_subset(&other.set)
    }

    #[inline]
    pub fn is_superset(&self, other: &Self) -> bool {
        other.is_subset(self)
    }

    #[inline]
    pub fn is_disjoint(&self, other: &Self) -> bool {
        !(self.has_null && other.has_null) && self.set.is_disjoint(&other.set)
    }

    #[inline]
    pub fn union_with(&mut self, other: &Self) {
        self.has_null |= other.has_null;
        self.set.union_with(&other.set);
    }

    #[inline]
    pub fn intersect_with(&mut self, other: &Self) {
        self.has_null &= other.has_null;
        self.set.intersect_with(&other.set);
    }

    #[inline]
    pub fn difference_with(&mut self, other: &Self) {
        self.has_null &= !other.has_null;
        self.set.difference_with(&other.set);
    }

    #[inline]
    pub fn symmetric_difference_with(&mut self, other: &Self) {
        self.has_null ^= other.has_null;
        self.set.symmetric_difference_with(&other.set);
    }

    /// `None` first, then ascending keys.
    #[inline]
    pub fn iter(&self) -> Iter<K> {
        Iter {
            null: self.has_null,
            inner: self.set.iter(),
        }
    }
}

impl<K: ByteKey> NullableByteSet<K> {
    #[inline(always)]
    pub fn token_of(&self, key: Option<K>) -> Option<Token> {
        match key {
            Some(k) => self.set.token_of(k),
            None => self.has_null.then_some(NULL_TOKEN),
        }
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<Option<K>> {
        if token == NULL_TOKEN {
            self.has_null.then_some(None)
        } else {
            self.set.key_by_token(token).map(Some)
        }
    }

    /// The `n`-th non-`None` key of a pass key after key (the generated sending), see
    /// [`ByteSet::key_seq`].
    ///
    /// # Safety
    /// As [`ByteSet::key_seq`].
    #[inline]
    pub unsafe fn key_seq(&self, n: usize, at: &mut super::token::Seq) -> K {
        // SAFETY: the caller keeps the contract of this pass, the same for the keyed set
        unsafe { self.set.key_seq(n, at) }
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
    pub fn insert_key(&mut self, k: K) -> bool {
        self.set.insert(k)
    }

    /// `true` if the key was not present.
    #[inline(always)]
    pub fn insert(&mut self, key: Option<K>) -> bool {
        match key {
            Some(k) => self.set.insert(k),
            None => !core::mem::replace(&mut self.has_null, true),
        }
    }

    /// `true` if the key was present.
    #[inline(always)]
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
}

impl<K: ByteKey + fmt::Debug> fmt::Debug for NullableByteSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<K: ByteKey> Extend<Option<K>> for NullableByteSet<K> {
    fn extend<I: IntoIterator<Item = Option<K>>>(&mut self, iter: I) {
        for k in iter {
            self.insert(k);
        }
    }
}

impl<K: ByteKey> FromIterator<Option<K>> for NullableByteSet<K> {
    fn from_iter<I: IntoIterator<Item = Option<K>>>(iter: I) -> Self {
        let mut s = Self::new();
        s.extend(iter);
        s
    }
}

// iterator

/// `None` first, then ascending keys.
pub struct Iter<K> {
    null: bool,
    inner: byte_set::Iter<K>,
}

impl<K> Clone for Iter<K> {
    fn clone(&self) -> Self {
        Self {
            null: self.null,
            inner: self.inner.clone(),
        }
    }
}

impl<K: ByteKey> Iterator for Iter<K> {
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

impl<K: ByteKey> ExactSizeIterator for Iter<K> {}
impl<K: ByteKey> FusedIterator for Iter<K> {}

impl<K: ByteKey> IntoIterator for NullableByteSet<K> {
    type Item = Option<K>;
    type IntoIter = Iter<K>;
    #[inline]
    fn into_iter(self) -> Iter<K> {
        self.iter()
    }
}

impl<K: ByteKey> IntoIterator for &NullableByteSet<K> {
    type Item = Option<K>;
    type IntoIter = Iter<K>;
    #[inline]
    fn into_iter(self) -> Iter<K> {
        self.iter()
    }
}
