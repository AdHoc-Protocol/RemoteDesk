//! Hash set with cached hashes, borrowed-key lookup, and a separate `None` flag.
//!
//! Each keyed entry caches the low 32 hash bits to avoid rehashing during relocation. Ordinary
//! methods work with keys directly: `insert(k)`, `contains(&q)`, `remove(&q)`, and `iter()` over
//! `&K`. Use `insert_null`, `contains_null`, and `remove_null` for the optional key, or the `*_opt`
//! methods for `Option<K>` inputs.
//!
//! [`NullableSet::len`] includes the optional key, while `iter()` visits only keyed entries:
//! `len() == iter().len() + usize::from(contains_null())`.

use super::lohi::{RawIntoIter, RawIter, RawTable};
use super::token::{or_null, Token, NULL_TOKEN};
use core::borrow::Borrow;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use rustc_hash::FxBuildHasher;

#[derive(Clone)]
struct Entry<K> {
    hash: u32,
    key: K,
}

#[inline(always)]
fn cached<K>(e: &Entry<K>) -> u64 {
    e.hash as u64
}

pub struct NullableSet<K, S = FxBuildHasher> {
    // Filters must use the cached 32 bits: rebuilds never have the original full hash.
    raw: RawTable<Entry<K>, (), u32, 32>,
    has_null: bool,
    hasher: S,
}

impl<K, S> NullableSet<K, S> {
    /// Keyed slots first, then `NULL_TOKEN`; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> { or_null(self.raw.token(), self.has_null) }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        if token == NULL_TOKEN || !self.raw.contains_token(token) { return None; }
        or_null(self.raw.next_token(token), self.has_null)
    }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        if token == NULL_TOKEN { self.has_null } else { self.raw.contains_token(token) }
    }

    /// `Some(None)` denotes the null key; outer `None` means an unoccupied slot.
    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<Option<&K>> {
        if token == NULL_TOKEN { self.has_null.then_some(None) }
        else { self.raw.get_by_token(token).map(|(e, _)| Some(&e.key)) }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.raw.len() + self.has_null as usize
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.raw.capacity()
    }

    /// Removes all keys, keeping the allocated memory.
    #[inline]
    pub fn clear(&mut self) {
        self.has_null = false;
        self.raw.clear();
    }

    /// [`Self::clear`] that gives each key to `give` instead of dropping it (the generated receiving:
    /// the memory of the string keys serves the keys received next).
    #[inline]
    pub fn clear_keys_to(&mut self, mut give: impl FnMut(K)) {
        self.has_null = false;
        self.raw.clear_giving(cached, |e| give(e.key));
    }

    #[inline]
    pub fn hasher(&self) -> &S {
        &self.hasher
    }

    #[inline(always)]
    pub fn contains_null(&self) -> bool {
        self.has_null
    }

    /// `true` if the null key was not present.
    #[inline]
    pub fn insert_null(&mut self) -> bool {
        !core::mem::replace(&mut self.has_null, true)
    }

    /// `true` if the null key was present.
    #[inline]
    pub fn remove_null(&mut self) -> bool {
        core::mem::take(&mut self.has_null)
    }

    pub fn reserve(&mut self, additional: usize) {
        self.raw.reserve(additional, cached);
    }

    pub fn shrink_to_fit(&mut self) {
        self.raw.shrink_to_fit(cached);
    }

    /// Keeps only the keys for which `f` returns `true`; the null key stays.
    pub fn retain<F: FnMut(&K) -> bool>(&mut self, mut f: F) {
        self.raw.retain(|e, _| f(&e.key), cached);
    }

    /// The keys, without the null one.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K> {
        Iter { inner: self.raw.iter() }
    }

    /// The `n`-th key of [`Self::iter`], O(1): the pass by number of a pack being sent.
    #[inline]
    pub fn key_at(&self, n: usize) -> &K {
        &self.raw.at(n).0.key
    }

    /// All the keys as `Option`: the null one first.
    #[inline]
    pub fn iter_opt(&self) -> IterOpt<'_, K> {
        IterOpt { null: self.has_null, inner: self.raw.iter() }
    }
}

impl<K: Eq + Hash> NullableSet<K, FxBuildHasher> {
    #[inline]
    pub fn new() -> Self {
        Self::with_hasher(FxBuildHasher)
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, FxBuildHasher)
    }
}

impl<K: Eq + Hash> Default for NullableSet<K, FxBuildHasher> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash, S: BuildHasher> NullableSet<K, S> {
    #[inline(always)]
    pub fn token_of<Q>(&self, key: &Q) -> Option<Token>
    where K: Borrow<Q>, Q: Hash + Eq + ?Sized {
        let h = self.hash_of(key);
        self.raw.find(h, |e| e.hash as u64 == h && e.key.borrow() == key)
    }

    #[inline(always)]
    pub fn token_of_opt<Q>(&self, key: Option<&Q>) -> Option<Token>
    where K: Borrow<Q>, Q: Hash + Eq + ?Sized {
        match key { Some(k) => self.token_of(k), None => self.has_null.then_some(NULL_TOKEN) }
    }

    /// Does not allocate.
    #[inline]
    pub fn with_hasher(hasher: S) -> Self {
        Self { raw: RawTable::new(), has_null: false, hasher }
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self { raw: RawTable::with_capacity(capacity), has_null: false, hasher }
    }

    /// The table uses the low 32 bits only: the bucket index and the cached hash.
    #[inline(always)]
    fn hash_of<Q: Hash + ?Sized>(&self, q: &Q) -> u64 {
        self.hasher.hash_one(q) as u32 as u64
    }

    #[inline]
    pub fn contains<Q>(&self, q: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let h = self.hash_of(q);
        self.raw.find(h, |e| e.hash as u64 == h && e.key.borrow() == q).is_some()
    }

    /// `None` — the null key.
    #[inline]
    pub fn contains_opt<Q>(&self, key: Option<&Q>) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match key {
            Some(q) => self.contains(q),
            None => self.has_null,
        }
    }

    /// The stored key equal to `q`.
    #[inline]
    pub fn get<Q>(&self, q: &Q) -> Option<&K>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let h = self.hash_of(q);
        self.raw.get(h, |e| e.hash as u64 == h && e.key.borrow() == q).map(|(e, _)| &e.key)
    }

    /// Key without `Option`, the null key separately: the serializer and the parser neither wrap a key nor check it for null.
    #[inline(always)]
    pub fn insert_key(&mut self, k: K) -> bool { self.insert(k) }

    /// `true` if the key was not present; a present key is kept and `k` is dropped.
    #[inline]
    pub fn insert(&mut self, k: K) -> bool {
        let h = self.hash_of(&k);
        if self.raw.find_for_insert(h, |e| e.hash as u64 == h && e.key == k).is_some() {
            return false;
        }
        self.raw.insert_unique(h, Entry { hash: h as u32, key: k }, (), cached);
        true
    }

    /// `None` — the null key.
    #[inline]
    pub fn insert_opt(&mut self, key: Option<K>) -> bool {
        match key {
            Some(k) => self.insert(k),
            None => self.insert_null(),
        }
    }

    /// `true` if the key was present.
    #[inline]
    pub fn remove<Q>(&mut self, q: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.take(q).is_some()
    }

    /// `None` — the null key.
    #[inline]
    pub fn remove_opt<Q>(&mut self, key: Option<&Q>) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match key {
            Some(q) => self.remove(q),
            None => self.remove_null(),
        }
    }

    /// Removes and returns the stored key equal to `q`.
    #[inline]
    pub fn take<Q>(&mut self, q: &Q) -> Option<K>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let h = self.hash_of(q);
        self.raw.remove(h, |e| e.hash as u64 == h && e.key.borrow() == q, cached).map(|(e, _)| e.key)
    }

    // set algebra

    /// Removes the keys absent from `other`.
    pub fn intersect_with<S2: BuildHasher>(&mut self, other: &NullableSet<K, S2>) {
        if self.has_null && !other.has_null {
            self.has_null = false;
        }
        if other.raw.len() == 0 {
            self.raw.clear();
            return;
        }
        self.raw.retain(|e, _| other.contains(&e.key), cached);
    }

    /// Removes the keys present in `other`.
    pub fn difference_with<S2: BuildHasher>(&mut self, other: &NullableSet<K, S2>) {
        if other.has_null {
            self.has_null = false;
        }
        if self.raw.len() <= other.raw.len() {
            self.raw.retain(|e, _| !other.contains(&e.key), cached);
        } else {
            // The hashers may differ: `other`'s cached hashes mean nothing here.
            for (e, _) in other.raw.iter() {
                self.remove(&e.key);
            }
        }
    }

    /// Toggles every key of `other`: removes it if present, inserts it otherwise.
    pub fn symmetric_difference_with<I: IntoIterator<Item = K>>(&mut self, other: I) {
        for k in other {
            if !self.remove(&k) {
                self.insert(k);
            }
        }
    }

    pub fn is_subset<S2: BuildHasher>(&self, other: &NullableSet<K, S2>) -> bool {
        (!self.has_null || other.has_null)
            && self.raw.len() <= other.raw.len()
            && self.raw.iter().all(|(e, _)| other.contains(&e.key))
    }

    pub fn is_superset<S2: BuildHasher>(&self, other: &NullableSet<K, S2>) -> bool {
        other.is_subset(self)
    }

    /// Walks the smaller set.
    pub fn is_disjoint<S2: BuildHasher>(&self, other: &NullableSet<K, S2>) -> bool {
        if self.has_null && other.has_null {
            return false;
        }
        if self.raw.len() <= other.raw.len() {
            self.raw.iter().all(|(e, _)| !other.contains(&e.key))
        } else {
            other.raw.iter().all(|(e, _)| !self.contains(&e.key))
        }
    }
}

impl<K: Clone, S: Clone> Clone for NullableSet<K, S> {
    fn clone(&self) -> Self {
        Self { raw: self.raw.clone(), has_null: self.has_null, hasher: self.hasher.clone() }
    }
}

impl<K: Eq + Hash, S: BuildHasher> PartialEq for NullableSet<K, S> {
    /// The cached hash finds the key in `other` without hashing it again when the hashers agree;
    /// a miss is checked by the full lookup.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.has_null == other.has_null
            && self.raw.len() == other.raw.len()
            && self.raw.iter().all(|(e, _)| match other.raw.find(cached(e), |o| o.hash == e.hash && o.key == e.key) {
                Some(_) => true,
                None => other.contains(&e.key),
            })
    }
}

impl<K: Eq + Hash, S: BuildHasher> Eq for NullableSet<K, S> {}

/// Order-independent, as the equality is: each entry hashed alone, the hashes summed, then the
/// count.
impl<K: Hash, S> Hash for NullableSet<K, S> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        let mut sum = 0u64;
        for k in self.iter() {
            sum = sum.wrapping_add(super::entry_hash(&k));
        }
        state.write_u8(self.has_null as u8);
        state.write_usize(self.len());
        state.write_u64(sum);
    }
}

impl<K: fmt::Debug, S> fmt::Debug for NullableSet<K, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter_opt()).finish()
    }
}

impl<K: Eq + Hash, S: BuildHasher> Extend<K> for NullableSet<K, S> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for k in iter {
            self.insert(k);
        }
    }
}

impl<K: Eq + Hash, S: BuildHasher + Default> FromIterator<K> for NullableSet<K, S> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        let mut s = Self::with_hasher(S::default());
        s.extend(iter);
        s
    }
}

// iterators

/// The keys without the null one; order: the lo region, then the hi region.
pub struct Iter<'a, K> {
    inner: RawIter<'a, Entry<K>, ()>,
}

impl<K> Clone for Iter<'_, K> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<'a, K> Iterator for Iter<'a, K> {
    type Item = &'a K;

    #[inline]
    fn next(&mut self) -> Option<&'a K> {
        self.inner.next().map(|(e, _)| &e.key)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }

    #[inline]
    fn fold<B, F: FnMut(B, &'a K) -> B>(self, init: B, mut f: F) -> B {
        self.inner.fold(init, |acc, (e, _)| f(acc, &e.key))
    }
}

impl<K> ExactSizeIterator for Iter<'_, K> {}
impl<K> FusedIterator for Iter<'_, K> {}

/// All the keys as `Option`: `None` first, then the lo region, then the hi region.
pub struct IterOpt<'a, K> {
    null: bool,
    inner: RawIter<'a, Entry<K>, ()>,
}

impl<K> Clone for IterOpt<'_, K> {
    fn clone(&self) -> Self {
        Self { null: self.null, inner: self.inner.clone() }
    }
}

impl<'a, K> Iterator for IterOpt<'a, K> {
    type Item = Option<&'a K>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.null {
            self.null = false;
            return Some(None);
        }
        self.inner.next().map(|(e, _)| Some(&e.key))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.inner.len() + self.null as usize;
        (n, Some(n))
    }

    #[inline]
    fn fold<B, F: FnMut(B, Self::Item) -> B>(self, init: B, mut f: F) -> B {
        let mut acc = init;
        if self.null {
            acc = f(acc, None);
        }
        self.inner.fold(acc, |acc, (e, _)| f(acc, Some(&e.key)))
    }
}

impl<K> ExactSizeIterator for IterOpt<'_, K> {}
impl<K> FusedIterator for IterOpt<'_, K> {}

/// The keys without the null one.
pub struct IntoIter<K> {
    inner: RawIntoIter<Entry<K>, ()>,
}

impl<K> Iterator for IntoIter<K> {
    type Item = K;

    #[inline]
    fn next(&mut self) -> Option<K> {
        self.inner.next().map(|(e, _)| e.key)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K> ExactSizeIterator for IntoIter<K> {}
impl<K> FusedIterator for IntoIter<K> {}

impl<'a, K, S> IntoIterator for &'a NullableSet<K, S> {
    type Item = &'a K;
    type IntoIter = Iter<'a, K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K, S> IntoIterator for NullableSet<K, S> {
    type Item = K;
    type IntoIter = IntoIter<K>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter { inner: self.raw.into_iter() }
    }
}
