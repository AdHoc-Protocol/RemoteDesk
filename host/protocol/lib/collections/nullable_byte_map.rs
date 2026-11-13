//! [`ByteMap`] with separate storage for a `None` key.
//!
//! Ordinary iteration visits the optional entry first, then keys in unsigned order. Token traversal
//! uses the ordering documented in [`super::token`].

use super::byte_map::{self, ByteMap};
use super::keys::ByteKey;
use super::token::{or_null, Token, NULL_TOKEN};
use core::fmt;
use core::iter::FusedIterator;

pub struct NullableByteMap<K, V> {
    map: ByteMap<K, V>,
    null_value: Option<V>,
}

impl<K, V> NullableByteMap<K, V> {
    /// Keyed entries first, then `NULL_TOKEN`; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> { or_null(self.map.token(), self.null_value.is_some()) }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        if token == NULL_TOKEN || !self.map.contains_token(token) { return None; }
        or_null(self.map.next_token(token), self.null_value.is_some())
    }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        if token == NULL_TOKEN { self.null_value.is_some() } else { self.map.contains_token(token) }
    }

    #[inline(always)]
    pub fn value_by_token(&self, token: Token) -> Option<&V> {
        if token == NULL_TOKEN { self.null_value.as_ref() } else { self.map.value_by_token(token) }
    }

    #[inline(always)]
    pub fn value_by_token_mut(&mut self, token: Token) -> Option<&mut V> {
        if token == NULL_TOKEN { self.null_value.as_mut() } else { self.map.value_by_token_mut(token) }
    }

    /// Does not allocate.
    #[inline]
    pub const fn new() -> Self {
        Self { map: ByteMap::new(), null_value: None }
    }

    /// Room for `capacity` keyed entries (at most 256).
    pub fn with_capacity(capacity: usize) -> Self {
        Self { map: ByteMap::with_capacity(capacity), null_value: None }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.map.len() + self.null_value.is_some() as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.null_value.is_none() && self.map.is_empty()
    }

    #[inline]
    pub fn is_flat(&self) -> bool {
        self.map.is_flat()
    }

    /// The entries of the non-`None` keys.
    #[inline]
    pub fn keyed(&self) -> &ByteMap<K, V> {
        &self.map
    }

    #[inline]
    pub fn keyed_mut(&mut self) -> &mut ByteMap<K, V> {
        &mut self.map
    }

    pub fn clear(&mut self) {
        self.null_value = None;
        self.map.clear();
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter { null: self.null_value.as_ref(), inner: self.map.iter() }
    }

    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut { null: self.null_value.as_mut(), inner: self.map.iter_mut() }
    }
}

impl<K, V> Default for NullableByteMap<K, V> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: ByteKey, V> NullableByteMap<K, V> {
    #[inline(always)]
    pub fn token_of(&self, key: Option<K>) -> Option<Token> {
        match key { Some(k) => self.map.token_of(k), None => self.null_value.is_some().then_some(NULL_TOKEN) }
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<Option<K>> {
        if token == NULL_TOKEN { self.null_value.is_some().then_some(None) }
        else { self.map.key_by_token(token).map(Some) }
    }

    #[inline(always)]
    pub fn entry_by_token(&self, token: Token) -> Option<(Option<K>, &V)> {
        if token == NULL_TOKEN { self.null_value.as_ref().map(|v| (None, v)) }
        else { self.map.entry_by_token(token).map(|(k, v)| (Some(k), v)) }
    }

    /// The `n`-th entry of a non-`None` key of a pass entry after entry (the generated sending),
    /// see [`ByteMap::entry_seq`].
    ///
    /// # Safety
    /// As [`ByteMap::entry_seq`].
    #[inline]
    pub unsafe fn entry_seq(&self, n: usize, at: &mut super::token::Seq) -> (super::token::Key<K>, &V) {
        // SAFETY: the caller keeps the contract of this pass, the same for the keyed map
        unsafe { self.map.entry_seq(n, at) }
    }

    /// The value of the `None` key.
    #[inline(always)]
    pub fn null_value(&self) -> Option<&V> {
        self.null_value.as_ref()
    }

    /// Room for `additional` more keyed entries, see [`ByteMap::reserve`].
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.map.reserve(additional);
    }

    #[inline]
    pub fn contains_key(&self, key: Option<K>) -> bool {
        match key {
            Some(k) => self.map.contains_key(k),
            None => self.null_value.is_some(),
        }
    }

    /// The value of a key without `Option`; the null key — `null_value`.
    #[inline(always)]
    pub fn get_key(&self, k: K) -> Option<&V> { self.map.get(k) }

    #[inline(always)]
    pub fn get(&self, key: Option<K>) -> Option<&V> {
        match key {
            Some(k) => self.map.get(k),
            None => self.null_value.as_ref(),
        }
    }

    #[inline]
    pub fn get_mut(&mut self, key: Option<K>) -> Option<&mut V> {
        match key {
            Some(k) => self.map.get_mut(k),
            None => self.null_value.as_mut(),
        }
    }

    /// Key without `Option`, the null key separately: the serializer and the parser neither wrap a key nor check it for null.
    #[inline(always)]
    pub fn insert_key(&mut self, k: K, value: V) -> Option<V> { self.map.insert(k, value) }

    #[inline(always)]
    pub fn insert_null_key(&mut self, value: V) -> Option<V> { self.null_value.replace(value) }

    /// Inserts or replaces the value of `key` and gives the token of its entry: the value is then reached by
    /// [`Self::value_by_token_mut`] without the key (a value received in parts).
    #[inline]
    pub fn insert_token(&mut self, key: Option<K>, value: V) -> Token {
        self.insert(key, value);
        self.token_of(key).unwrap()
    }

    /// Returns the previous value of the key.
    #[inline(always)]
    pub fn insert(&mut self, key: Option<K>, value: V) -> Option<V> {
        match key {
            Some(k) => self.map.insert(k, value),
            None => self.null_value.replace(value),
        }
    }

    /// Returns a reference to the value, inserting `f()` if the key is absent.
    #[inline]
    pub fn get_or_insert_with<F: FnOnce() -> V>(&mut self, key: Option<K>, f: F) -> &mut V {
        match key {
            Some(k) => self.map.get_or_insert_with(k, f),
            None => self.null_value.get_or_insert_with(f),
        }
    }

    #[inline]
    pub fn remove(&mut self, key: Option<K>) -> Option<V> {
        match key {
            Some(k) => self.map.remove(k),
            None => self.null_value.take(),
        }
    }

    /// Keeps only the entries for which `f` returns `true`.
    pub fn retain<F: FnMut(Option<K>, &mut V) -> bool>(&mut self, mut f: F) {
        if let Some(v) = self.null_value.as_mut() {
            if !f(None, v) {
                self.null_value = None;
            }
        }
        self.map.retain(|k, v| f(Some(k), v));
    }

    pub fn shrink_to_fit(&mut self) {
        self.map.shrink_to_fit();
    }

    #[inline]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = Option<K>> + FusedIterator + '_ {
        self.iter().map(|(k, _)| k)
    }

    #[inline]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + FusedIterator + '_ {
        self.iter().map(|(_, v)| v)
    }
}

impl<K: ByteKey, V: Clone> Clone for NullableByteMap<K, V> {
    fn clone(&self) -> Self {
        Self { map: self.map.clone(), null_value: self.null_value.clone() }
    }
}

impl<K: ByteKey, V: PartialEq> PartialEq for NullableByteMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.null_value == other.null_value && self.map == other.map
    }
}

impl<K: ByteKey, V: Eq> Eq for NullableByteMap<K, V> {}

impl<K: ByteKey + fmt::Debug, V: fmt::Debug> fmt::Debug for NullableByteMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: ByteKey, V> Extend<(Option<K>, V)> for NullableByteMap<K, V> {
    fn extend<I: IntoIterator<Item = (Option<K>, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: ByteKey, V> FromIterator<(Option<K>, V)> for NullableByteMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (Option<K>, V)>>(iter: I) -> Self {
        let mut m = Self::new();
        m.extend(iter);
        m
    }
}

// iterators

/// The `None` key first, then ascending keys.
pub struct Iter<'a, K, V> {
    null: Option<&'a V>,
    inner: byte_map::Iter<'a, K, V>,
}

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Self { null: self.null, inner: self.inner.clone() }
    }
}

impl<'a, K: ByteKey, V> Iterator for Iter<'a, K, V> {
    type Item = (Option<K>, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(v) = self.null.take() {
            return Some((None, v));
        }
        self.inner.next().map(|(k, v)| (Some(k), v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.inner.len() + self.null.is_some() as usize;
        (n, Some(n))
    }
}

impl<K: ByteKey, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K: ByteKey, V> FusedIterator for Iter<'_, K, V> {}

pub struct IterMut<'a, K, V> {
    null: Option<&'a mut V>,
    inner: byte_map::IterMut<'a, K, V>,
}

impl<'a, K: ByteKey, V> Iterator for IterMut<'a, K, V> {
    type Item = (Option<K>, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(v) = self.null.take() {
            return Some((None, v));
        }
        self.inner.next().map(|(k, v)| (Some(k), v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.inner.len() + self.null.is_some() as usize;
        (n, Some(n))
    }
}

impl<K: ByteKey, V> ExactSizeIterator for IterMut<'_, K, V> {}
impl<K: ByteKey, V> FusedIterator for IterMut<'_, K, V> {}

pub struct IntoIter<K: ByteKey, V> {
    null: Option<V>,
    inner: byte_map::IntoIter<K, V>,
}

impl<K: ByteKey, V> Iterator for IntoIter<K, V> {
    type Item = (Option<K>, V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(v) = self.null.take() {
            return Some((None, v));
        }
        self.inner.next().map(|(k, v)| (Some(k), v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.inner.len() + self.null.is_some() as usize;
        (n, Some(n))
    }
}

impl<K: ByteKey, V> ExactSizeIterator for IntoIter<K, V> {}
impl<K: ByteKey, V> FusedIterator for IntoIter<K, V> {}

impl<'a, K: ByteKey, V> IntoIterator for &'a NullableByteMap<K, V> {
    type Item = (Option<K>, &'a V);
    type IntoIter = Iter<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, K: ByteKey, V> IntoIterator for &'a mut NullableByteMap<K, V> {
    type Item = (Option<K>, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<K: ByteKey, V> IntoIterator for NullableByteMap<K, V> {
    type Item = (Option<K>, V);
    type IntoIter = IntoIter<K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter { null: self.null_value, inner: self.map.into_iter() }
    }
}
