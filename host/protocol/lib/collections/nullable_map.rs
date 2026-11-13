//! Hash map with cached hashes and a separate `None` key slot.
//!
//! Keyed entries use [`Map`], including borrowed-key lookups. The ordinary iterator yields the
//! `None` entry first when present.

use super::map::{self, Map};
use super::token::{or_null, Token, NULL_TOKEN};
use core::borrow::Borrow;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use rustc_hash::FxBuildHasher;

pub struct NullableMap<K, V, S = FxBuildHasher> {
    map: Map<K, V, S>,
    null_value: Option<V>,
}

impl<K, V, S> NullableMap<K, V, S> {
    /// Keyed slots first, then `NULL_TOKEN`; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> {
        or_null(self.map.token(), self.null_value.is_some())
    }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        if token == NULL_TOKEN || !self.map.contains_token(token) {
            return None;
        }
        or_null(self.map.next_token(token), self.null_value.is_some())
    }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        if token == NULL_TOKEN {
            self.null_value.is_some()
        } else {
            self.map.contains_token(token)
        }
    }

    #[inline(always)]
    pub fn entry_by_token(&self, token: Token) -> Option<(Option<&K>, &V)> {
        if token == NULL_TOKEN {
            self.null_value.as_ref().map(|v| (None, v))
        } else {
            self.map.entry_by_token(token).map(|(k, v)| (Some(k), v))
        }
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<Option<&K>> {
        self.entry_by_token(token).map(|(k, _)| k)
    }

    #[inline(always)]
    pub fn value_by_token(&self, token: Token) -> Option<&V> {
        if token == NULL_TOKEN {
            self.null_value.as_ref()
        } else {
            self.map.value_by_token(token)
        }
    }

    #[inline(always)]
    pub fn value_by_token_mut(&mut self, token: Token) -> Option<&mut V> {
        if token == NULL_TOKEN {
            self.null_value.as_mut()
        } else {
            self.map.value_by_token_mut(token)
        }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.map.len() + self.null_value.is_some() as usize
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.map.capacity()
    }

    /// Removes all entries, keeping the allocated memory.
    #[inline]
    pub fn clear(&mut self) {
        self.null_value = None;
        self.map.clear();
    }

    /// [`Self::clear`] that gives each key to `give`, see [`Map::clear_keys_to`].
    #[inline]
    pub fn clear_keys_to(&mut self, give: impl FnMut(K)) {
        self.null_value = None;
        self.map.clear_keys_to(give);
    }

    #[inline]
    pub fn hasher(&self) -> &S {
        self.map.hasher()
    }

    /// The entries of the non-`None` keys.
    #[inline]
    pub fn keyed(&self) -> &Map<K, V, S> {
        &self.map
    }

    /// The `n`-th entry of a non-`None` key, O(1): the pass by number of a pack being sent.
    #[inline]
    pub fn entry_at(&self, n: usize) -> (&K, &V) {
        self.map.entry_at(n)
    }

    /// The value of the `None` key: `get(None)` without naming the borrowed key type.
    #[inline]
    pub fn null_value(&self) -> Option<&V> {
        self.null_value.as_ref()
    }

    #[inline]
    pub fn null_value_mut(&mut self) -> Option<&mut V> {
        self.null_value.as_mut()
    }

    /// `remove(None)` without naming the borrowed key type.
    #[inline]
    pub fn remove_null(&mut self) -> Option<V> {
        self.null_value.take()
    }

    pub fn reserve(&mut self, additional: usize) {
        self.map.reserve(additional);
    }

    pub fn shrink_to_fit(&mut self) {
        self.map.shrink_to_fit();
    }

    /// Keeps only the entries for which `f` returns `true`.
    pub fn retain<F: FnMut(Option<&K>, &mut V) -> bool>(&mut self, mut f: F) {
        if let Some(v) = self.null_value.as_mut() {
            if !f(None, v) {
                self.null_value = None;
            }
        }
        self.map.retain(|k, v| f(Some(k), v));
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            null: self.null_value.as_ref(),
            inner: self.map.iter(),
        }
    }

    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut {
            null: self.null_value.as_mut(),
            inner: self.map.iter_mut(),
        }
    }

    #[inline]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = Option<&K>> + FusedIterator + Clone + '_ {
        self.iter().map(|(k, _)| k)
    }

    #[inline]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + FusedIterator + Clone + '_ {
        self.iter().map(|(_, v)| v)
    }

    #[inline]
    pub fn values_mut(&mut self) -> impl ExactSizeIterator<Item = &mut V> + FusedIterator + '_ {
        self.iter_mut().map(|(_, v)| v)
    }
}

impl<K: Eq + Hash, V> NullableMap<K, V, FxBuildHasher> {
    #[inline]
    pub fn new() -> Self {
        Self::with_hasher(FxBuildHasher)
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, FxBuildHasher)
    }
}

impl<K: Eq + Hash, V> Default for NullableMap<K, V, FxBuildHasher> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash, V, S: BuildHasher> NullableMap<K, V, S> {
    #[inline(always)]
    pub fn token_of<Q>(&self, key: Option<&Q>) -> Option<Token>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match key {
            Some(k) => self.map.token_of(k),
            None => self.null_value.is_some().then_some(NULL_TOKEN),
        }
    }

    /// Does not allocate.
    #[inline]
    pub fn with_hasher(hasher: S) -> Self {
        Self {
            map: Map::with_hasher(hasher),
            null_value: None,
        }
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self {
            map: Map::with_capacity_and_hasher(capacity, hasher),
            null_value: None,
        }
    }

    /// The value of a key without `Option`; the null key — `null_value`.
    #[inline]
    pub fn get_key<Q>(&self, q: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get(q)
    }

    #[inline]
    pub fn get<Q>(&self, key: Option<&Q>) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match key {
            Some(q) => self.map.get(q),
            None => self.null_value.as_ref(),
        }
    }

    #[inline]
    pub fn get_mut<Q>(&mut self, key: Option<&Q>) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match key {
            Some(q) => self.map.get_mut(q),
            None => self.null_value.as_mut(),
        }
    }

    #[inline]
    pub fn contains_key<Q>(&self, key: Option<&Q>) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match key {
            Some(q) => self.map.contains_key(q),
            None => self.null_value.is_some(),
        }
    }

    /// Key without `Option`, the null key separately: the serializer and the parser neither wrap a key nor check it for null.
    #[inline(always)]
    pub fn insert_key(&mut self, k: K, value: V) -> Option<V> {
        self.map.insert(k, value)
    }

    #[inline(always)]
    pub fn insert_null_key(&mut self, value: V) -> Option<V> {
        self.null_value.replace(value)
    }

    /// Returns the previous value of the key.
    #[inline]
    pub fn insert(&mut self, key: Option<K>, value: V) -> Option<V> {
        match key {
            Some(k) => self.map.insert(k, value),
            None => self.null_value.replace(value),
        }
    }

    /// [`Map::insert_token`]; the `None` key gives `NULL_TOKEN`.
    #[inline]
    pub fn insert_token(&mut self, key: Option<K>, value: V) -> Token {
        match key {
            Some(k) => self.map.insert_token(k, value),
            None => {
                self.null_value = Some(value);
                NULL_TOKEN
            }
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
    pub fn remove<Q>(&mut self, key: Option<&Q>) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match key {
            Some(q) => self.map.remove(q),
            None => self.null_value.take(),
        }
    }
}

impl<K: Clone, V: Clone, S: Clone> Clone for NullableMap<K, V, S> {
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
            null_value: self.null_value.clone(),
        }
    }
}

impl<K: Eq + Hash, V: PartialEq, S: BuildHasher> PartialEq for NullableMap<K, V, S> {
    fn eq(&self, other: &Self) -> bool {
        self.null_value == other.null_value && self.map == other.map
    }
}

impl<K: Eq + Hash, V: Eq, S: BuildHasher> Eq for NullableMap<K, V, S> {}

/// Order-independent, as the equality is: each entry hashed alone, the hashes summed, then the
/// count.
impl<K: Hash, V: Hash, S> Hash for NullableMap<K, V, S> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.null_value.hash(state);
        self.map.hash(state);
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S> fmt::Debug for NullableMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Eq + Hash, V, S: BuildHasher> Extend<(Option<K>, V)> for NullableMap<K, V, S> {
    fn extend<I: IntoIterator<Item = (Option<K>, V)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Default> FromIterator<(Option<K>, V)> for NullableMap<K, V, S> {
    fn from_iter<I: IntoIterator<Item = (Option<K>, V)>>(iter: I) -> Self {
        let mut m = Self::with_hasher(S::default());
        m.extend(iter);
        m
    }
}

// iterators

/// Order: the `None` key, then the keyed entries.
pub struct Iter<'a, K, V> {
    null: Option<&'a V>,
    inner: map::Iter<'a, K, V>,
}

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Self {
            null: self.null,
            inner: self.inner.clone(),
        }
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (Option<&'a K>, &'a V);

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

    #[inline]
    fn fold<B, F: FnMut(B, Self::Item) -> B>(self, init: B, mut f: F) -> B {
        let mut acc = init;
        if let Some(v) = self.null {
            acc = f(acc, (None, v));
        }
        self.inner.fold(acc, |acc, (k, v)| f(acc, (Some(k), v)))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K, V> FusedIterator for Iter<'_, K, V> {}

pub struct IterMut<'a, K, V> {
    null: Option<&'a mut V>,
    inner: map::IterMut<'a, K, V>,
}

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (Option<&'a K>, &'a mut V);

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

impl<K, V> ExactSizeIterator for IterMut<'_, K, V> {}
impl<K, V> FusedIterator for IterMut<'_, K, V> {}

pub struct IntoIter<K, V> {
    null: Option<V>,
    inner: map::IntoIter<K, V>,
}

impl<K, V> Iterator for IntoIter<K, V> {
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

impl<K, V> ExactSizeIterator for IntoIter<K, V> {}
impl<K, V> FusedIterator for IntoIter<K, V> {}

impl<'a, K, V, S> IntoIterator for &'a NullableMap<K, V, S> {
    type Item = (Option<&'a K>, &'a V);
    type IntoIter = Iter<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, K, V, S> IntoIterator for &'a mut NullableMap<K, V, S> {
    type Item = (Option<&'a K>, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<K, V, S> IntoIterator for NullableMap<K, V, S> {
    type Item = (Option<K>, V);
    type IntoIter = IntoIter<K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            null: self.null_value,
            inner: self.map.into_iter(),
        }
    }
}
