//! Hash map with cached hashes and borrowed-key lookup.
//!
//! Keys and values use the split table in `lohi.rs`. Each key stores the low 32 hash bits, avoiding
//! rehashing during growth and removal and filtering comparisons during lookup. Borrowed forms such
//! as `&str` for `String` are accepted when their hashing and equality agree. Use
//! [`PrimitiveMap`](super::PrimitiveMap) for cheap `Copy` keys or
//! [`NullableMap`](super::NullableMap) to include a `None` key.

use super::lohi::{RawIntoIter, RawIter, RawIterMut, RawTable};
use super::Token;
use core::borrow::Borrow;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use core::mem;
use core::ops::Index;
use rustc_hash::FxBuildHasher;

/// What a lookup reads: the key with its cached hash; the value lies apart.
#[derive(Clone)]
pub(crate) struct Probe<K> {
    hash: u32,
    key: K,
}

#[inline(always)]
fn cached(e: &Probe<impl Sized>) -> u64 {
    e.hash as u64
}

pub struct Map<K, V, S = FxBuildHasher> {
    // Filters must use the cached 32 bits: rebuilds never have the original full hash.
    raw: RawTable<Probe<K>, V, u32, 32>,
    hasher: S,
}

impl<K, V, S> Map<K, V, S> {
    /// First physical slot; see [`super::token`] for validity and iteration order.
    #[inline(always)]
    pub fn token(&self) -> Option<Token> { self.raw.token() }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> { self.raw.next_token(token) }

    /// Checks slot occupancy, without detecting a stale or foreign token.
    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool { self.raw.contains_token(token) }

    /// Direct slot access without hashing; token is not an ordinal `entry_at` index.
    #[inline(always)]
    pub fn entry_by_token(&self, token: Token) -> Option<(&K, &V)> {
        self.raw.get_by_token(token).map(|(p, v)| (&p.key, v))
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<&K> {
        self.raw.get_by_token(token).map(|(p, _)| &p.key)
    }

    #[inline(always)]
    pub fn value_by_token(&self, token: Token) -> Option<&V> {
        self.raw.get_by_token(token).map(|(_, v)| v)
    }

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

    /// Removes all entries, keeping the allocated memory.
    #[inline]
    pub fn clear(&mut self) {
        self.raw.clear_by(cached); // the cached hash: a few entries in a large table cost themselves, not the capacity
    }

    /// [`Self::clear`] that gives each key to `give` instead of dropping it (the generated receiving:
    /// the memory of the string keys serves the keys received next).
    #[inline]
    pub fn clear_keys_to(&mut self, mut give: impl FnMut(K)) {
        self.raw.clear_giving(cached, |e| give(e.key));
    }

    #[inline]
    pub fn hasher(&self) -> &S {
        &self.hasher
    }

    pub fn reserve(&mut self, additional: usize) {
        self.raw.reserve(additional, cached);
    }

    pub fn shrink_to_fit(&mut self) {
        self.raw.shrink_to_fit(cached);
    }

    /// Keeps only the entries for which `f` returns `true`.
    pub fn retain<F: FnMut(&K, &mut V) -> bool>(&mut self, mut f: F) {
        self.raw.retain(|p, v| f(&p.key, v), cached);
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter(self.raw.iter())
    }

    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut(self.raw.iter_mut())
    }

    /// The `n`-th entry in the order of [`Self::iter`], O(1): the pass by number of a pack being
    /// sent.
    #[inline]
    pub fn entry_at(&self, n: usize) -> (&K, &V) {
        let (p, v) = self.raw.at(n);
        (&p.key, v)
    }

    #[inline]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> + FusedIterator + Clone + '_ {
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

impl<K: Eq + Hash, V> Map<K, V, FxBuildHasher> {
    #[inline]
    pub fn new() -> Self {
        Self::with_hasher(FxBuildHasher)
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, FxBuildHasher)
    }
}

impl<K: Eq + Hash, V> Default for Map<K, V, FxBuildHasher> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash, V, S: BuildHasher> Map<K, V, S> {
    /// Finds a physical slot using any borrowed form of the key.
    #[inline(always)]
    pub fn token_of<Q>(&self, key: &Q) -> Option<Token>
    where K: Borrow<Q>, Q: Hash + Eq + ?Sized {
        let h = self.hash(key);
        self.raw.find(h, |e| e.hash as u64 == h && e.key.borrow() == key)
    }

    /// Does not allocate.
    #[inline]
    pub fn with_hasher(hasher: S) -> Self {
        Self { raw: RawTable::new(), hasher }
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self { raw: RawTable::with_capacity(capacity), hasher }
    }

    /// The table uses the low 32 bits only: the bucket index and the cached hash.
    #[inline(always)]
    fn hash<Q: Hash + ?Sized>(&self, q: &Q) -> u64 {
        self.hasher.hash_one(q) as u32 as u64
    }

    #[inline(always)]
    fn find<Q>(&self, h: u64, q: &Q) -> Option<(&Probe<K>, &V)>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        self.raw.get(h, |e| e.hash as u64 == h && e.key.borrow() == q)
    }

    #[inline]
    pub fn get<Q>(&self, q: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.find(self.hash(q), q).map(|(_, v)| v)
    }

    #[inline]
    pub fn get_key_value<Q>(&self, q: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.find(self.hash(q), q).map(|(p, v)| (&p.key, v))
    }

    #[inline]
    pub fn get_mut<Q>(&mut self, q: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let h = self.hash(q);
        self.raw.get_mut(h, |e| e.hash as u64 == h && e.key.borrow() == q)
    }

    #[inline]
    pub fn contains_key<Q>(&self, q: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.find(self.hash(q), q).is_some()
    }

    /// Returns the previous value of the key; the stored key is kept, `k` is dropped.
    #[inline]
    pub fn insert(&mut self, k: K, value: V) -> Option<V> {
        let h = self.hash(&k);
        if let Some(i) = self.raw.find_for_insert(h, |e| e.hash as u64 == h && e.key == k) {
            return Some(mem::replace(unsafe { self.raw.at_mut(i) }, value));
        }
        self.raw.insert_unique(h, Probe { hash: h as u32, key: k }, value, cached);
        None
    }

    /// Inserts or replaces the value of `k` and gives the token of its entry: the value is then
    /// reached by [`Self::value_by_token_mut`] without the key (a value received in parts). Valid
    /// until the next insertion.
    #[inline]
    pub fn insert_token(&mut self, k: K, value: V) -> Token {
        let h = self.hash(&k);
        if let Some(i) = self.raw.find_for_insert(h, |e| e.hash as u64 == h && e.key == k) {
            *unsafe { self.raw.at_mut(i) } = value;
            return i;
        }
        self.raw.insert_unique_token(h, Probe { hash: h as u32, key: k }, value, cached)
    }

    /// Returns a reference to the value, inserting `f()` if the key is absent.
    #[inline]
    pub fn get_or_insert_with<F: FnOnce() -> V>(&mut self, k: K, f: F) -> &mut V {
        let h = self.hash(&k);
        // `k` moves into the entry only when it is inserted, so the lookup goes by index.
        match self.raw.find(h, |e| e.hash as u64 == h && e.key == k) {
            Some(i) => unsafe { self.raw.at_mut(i) },
            None => self.raw.insert_unique(h, Probe { hash: h as u32, key: k }, f(), cached),
        }
    }

    #[inline]
    pub fn remove<Q>(&mut self, q: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.remove_entry(q).map(|(_, v)| v)
    }

    #[inline]
    pub fn remove_entry<Q>(&mut self, q: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let h = self.hash(q);
        self.raw.remove(h, |e| e.hash as u64 == h && e.key.borrow() == q, cached).map(|(p, v)| (p.key, v))
    }
}

impl<K: Clone, V: Clone, S: Clone> Clone for Map<K, V, S> {
    fn clone(&self) -> Self {
        Self { raw: self.raw.clone(), hasher: self.hasher.clone() }
    }
}

impl<K: Eq + Hash, V: PartialEq, S: BuildHasher> PartialEq for Map<K, V, S> {
    /// The cached hash finds the key in `other` without hashing it again when the hashers agree;
    /// a miss is checked by the full lookup.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.raw.iter().all(|(p, v)| match other.raw.get(cached(p), |e| e.hash == p.hash && e.key == p.key) {
                Some((_, w)) => w == v,
                None => other.get(&p.key) == Some(v),
            })
    }
}

impl<K: Eq + Hash, V: Eq, S: BuildHasher> Eq for Map<K, V, S> {}

/// Order-independent, as the equality is: each entry hashed alone, the hashes summed, then the
/// count.
impl<K: Hash, V: Hash, S> Hash for Map<K, V, S> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        let mut sum = 0u64;
        for e in self.iter() {
            sum = sum.wrapping_add(super::entry_hash(&e));
        }
        state.write_usize(self.len());
        state.write_u64(sum);
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S> fmt::Debug for Map<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Panics if the key is absent.
impl<K, Q, V, S> Index<&Q> for Map<K, V, S>
where
    K: Eq + Hash + Borrow<Q>,
    Q: Hash + Eq + ?Sized,
    S: BuildHasher,
{
    type Output = V;
    #[inline]
    fn index(&self, q: &Q) -> &V {
        self.get(q).expect("key not found")
    }
}

impl<K: Eq + Hash, V, S: BuildHasher> Extend<(K, V)> for Map<K, V, S> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Default> FromIterator<(K, V)> for Map<K, V, S> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut m = Self::with_hasher(S::default());
        m.extend(iter);
        m
    }
}

impl<K: Eq + Hash, V, const N: usize> From<[(K, V); N]> for Map<K, V, FxBuildHasher> {
    fn from(entries: [(K, V); N]) -> Self {
        entries.into_iter().collect()
    }
}

// iterators

/// Order: the lo region, then the hi region.
pub struct Iter<'a, K, V>(RawIter<'a, Probe<K>, V>);

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(p, v)| (&p.key, v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn fold<B, F: FnMut(B, Self::Item) -> B>(self, init: B, mut f: F) -> B {
        self.0.fold(init, |acc, (p, v)| f(acc, (&p.key, v)))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K, V> FusedIterator for Iter<'_, K, V> {}

pub struct IterMut<'a, K, V>(RawIterMut<'a, Probe<K>, V>);

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(p, v)| (&p.key, v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V> ExactSizeIterator for IterMut<'_, K, V> {}
impl<K, V> FusedIterator for IterMut<'_, K, V> {}

pub struct IntoIter<K, V>(RawIntoIter<Probe<K>, V>);

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<(K, V)> {
        self.0.next().map(|(p, v)| (p.key, v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V> ExactSizeIterator for IntoIter<K, V> {}
impl<K, V> FusedIterator for IntoIter<K, V> {}

impl<'a, K, V, S> IntoIterator for &'a Map<K, V, S> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, K, V, S> IntoIterator for &'a mut Map<K, V, S> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<K, V, S> IntoIterator for Map<K, V, S> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter(self.raw.into_iter())
    }
}
