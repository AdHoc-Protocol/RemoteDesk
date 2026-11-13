//! Map of one-byte keys with bitset presence and compact or directly indexed values.
//!
//! Small values use 256 direct slots. Larger values start in a key-ordered vector, where the bitset
//! rank locates a value, and switch to direct slots past the density threshold.
//! [`ByteMap::shrink_to_fit`] can restore compact storage. An `Option<T>` value is stored like any
//! other value; an absent key has no value slot in compact storage.

use super::byte_set::{self, ByteSet};
use super::keys::ByteKey;
use super::Token;
use core::fmt;
use core::iter::FusedIterator;
use core::marker::PhantomData;
use core::mem::{self, MaybeUninit};
use core::ops::Index;

/// The compressed layout holds at most this many entries.
const FLAT_THRESHOLD: usize = 128;
/// Maximum value size for direct storage from the first insertion. At this size, 256 slots occupy
/// at most 4 KiB and avoid rank calculation. Larger values use compact storage until
/// [`FLAT_THRESHOLD`] is exceeded.
const SMALL_VALUE: usize = 16;

enum Store<V> {
    /// `values[keys.rank_below(k)]`, `values.len() == keys.len()`.
    Compressed(Vec<V>),
    /// 256 slots; slot `k` is initialized exactly when `keys` contains `k`.
    Flat(Box<[MaybeUninit<V>]>),
}

pub struct ByteMap<K, V> {
    keys: ByteSet<K>,
    store: Store<V>,
}

impl<K, V> ByteMap<K, V> {
    /// First entry token. Compressed tokens pack value index and byte key; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> {
        self.keys.token()
    }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        if !self.contains_token(token) {
            return None;
        }
        let next_byte = self.keys.next_from((token & 255) + 1)? as usize;
        match &self.store {
            Store::Compressed(values) => {
                let index = (token >> 8) + 1;
                (index < values.len()).then_some((index << 8) | next_byte)
            }
            Store::Flat(_) => Some(next_byte),
        }
    }

    /// Checks encoded slot bounds and key presence; no version/ownership check.
    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        match &self.store {
            Store::Compressed(values) => (token >> 8) < values.len() && self.keys.contains_byte(token as u8),
            Store::Flat(_) => token < 256 && self.keys.contains_byte(token as u8),
        }
    }

    /// Direct value slot access: compressed tokens avoid repeating the key rank calculation.
    #[inline(always)]
    pub fn value_by_token(&self, token: Token) -> Option<&V> {
        if !self.contains_token(token) {
            return None;
        }
        Some(unsafe {
            match &self.store {
                Store::Compressed(values) => values.get_unchecked(token >> 8),
                Store::Flat(values) => values.get_unchecked(token).assume_init_ref(),
            }
        })
    }

    #[inline(always)]
    pub fn value_by_token_mut(&mut self, token: Token) -> Option<&mut V> {
        if !self.contains_token(token) {
            return None;
        }
        Some(unsafe {
            match &mut self.store {
                Store::Compressed(values) => values.get_unchecked_mut(token >> 8),
                Store::Flat(values) => values.get_unchecked_mut(token).assume_init_mut(),
            }
        })
    }

    /// Does not allocate.
    #[inline]
    pub const fn new() -> Self {
        Self {
            keys: ByteSet::new(),
            store: Store::Compressed(Vec::new()),
        }
    }

    /// Room for `capacity` entries (at most 256): the flat layout above 128.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            keys: ByteSet::new(),
            store: if capacity > FLAT_THRESHOLD {
                Store::Flat(Box::new_uninit_slice(256))
            } else {
                Store::Compressed(Vec::with_capacity(capacity))
            },
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// `true` in the flat (256-slot) layout.
    #[inline]
    pub fn is_flat(&self) -> bool {
        matches!(self.store, Store::Flat(_))
    }

    /// The keys, as a set.
    #[inline]
    pub fn key_set(&self) -> ByteSet<K> {
        self.keys
    }

    /// Drops the values of the flat layout; the caller resets `keys`.
    fn drop_flat_values(&mut self) {
        if let Store::Flat(flat) = &mut self.store {
            if mem::needs_drop::<V>() {
                let keys = mem::take(&mut self.keys); // a panicking Drop leaks the rest, never drops twice
                for i in 0..4 {
                    let mut w = keys.word(i);
                    while w != 0 {
                        let b = i << 6 | w.trailing_zeros() as usize;
                        unsafe { flat.get_unchecked_mut(b).assume_init_drop() };
                        w &= w - 1;
                    }
                }
            }
        }
    }

    /// Removes all entries, keeping the layout and its memory.
    pub fn clear(&mut self) {
        match &mut self.store {
            Store::Compressed(v) => v.clear(),
            Store::Flat(_) => self.drop_flat_values(),
        }
        self.keys.clear();
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        let (base, flat) = match &self.store {
            Store::Compressed(v) => (v.as_ptr(), false),
            Store::Flat(f) => (f.as_ptr() as *const V, true),
        };
        Iter {
            keys: self.keys.iter(),
            base,
            flat,
            n: 0,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        let (base, flat) = match &mut self.store {
            Store::Compressed(v) => (v.as_mut_ptr(), false),
            Store::Flat(f) => (f.as_mut_ptr() as *mut V, true),
        };
        IterMut {
            keys: self.keys.iter(),
            base,
            flat,
            n: 0,
            _marker: PhantomData,
        }
    }
}

impl<K, V> Default for ByteMap<K, V> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Drop for ByteMap<K, V> {
    fn drop(&mut self) {
        self.drop_flat_values();
    }
}

impl<K: ByteKey, V> ByteMap<K, V> {
    #[inline(always)]
    pub fn token_of(&self, key: K) -> Option<Token> {
        let byte = key.to_byte();
        if !self.keys.contains_byte(byte) {
            return None;
        }
        Some(match &self.store {
            Store::Compressed(_) => (self.keys.rank_below(byte) << 8) | byte as usize,
            Store::Flat(_) => byte as usize,
        })
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<K> {
        self.contains_token(token).then(|| K::from_byte(token as u8))
    }

    #[inline(always)]
    pub fn entry_by_token(&self, token: Token) -> Option<(K, &V)> {
        self.value_by_token(token).map(|value| (K::from_byte(token as u8), value))
    }

    /// The `n`-th entry of a pass entry after entry (the generated sending): `at` keeps its place
    /// ([`super::token::Seq`]). The compressed layout keeps the values in the order of the keys:
    /// the `n`-th value is the value of the `n`-th key.
    ///
    /// # Safety
    /// `at` is the cursor of this pass over this map: the pass starts at `n` 0 with any `at`, and
    /// the map is not changed between its steps. A cursor of another pass or of another collection
    /// reads a slot that is not set.
    #[inline(always)]
    pub unsafe fn entry_seq(&self, n: usize, at: &mut super::token::Seq) -> (super::token::Key<K>, &V) {
        let k = super::token::seq_bit(n, at, self.keys.words());
        let v = match &self.store {
            Store::Flat(f) => unsafe { f.get_unchecked(k).assume_init_ref() },
            Store::Compressed(values) => &values[n],
        };
        (super::token::Key(K::from_byte(k as u8)), v)
    }

    #[inline]
    pub fn contains_key(&self, k: K) -> bool {
        self.keys.contains(k)
    }

    #[inline]
    pub fn get(&self, k: K) -> Option<&V> {
        let b = k.to_byte();
        if !self.keys.contains_byte(b) {
            return None;
        }
        if let Store::Flat(f) = &self.store {
            return Some(unsafe { f.get_unchecked(b as usize).assume_init_ref() });
        }
        Some(unsafe {
            match &self.store {
                Store::Compressed(v) => v.get_unchecked(self.keys.rank_below(b)),
                Store::Flat(f) => f.get_unchecked(b as usize).assume_init_ref(),
            }
        })
    }

    #[inline]
    pub fn get_mut(&mut self, k: K) -> Option<&mut V> {
        let b = k.to_byte();
        if !self.keys.contains_byte(b) {
            return None;
        }
        Some(unsafe {
            match &mut self.store {
                Store::Compressed(v) => v.get_unchecked_mut(self.keys.rank_below(b)),
                Store::Flat(f) => f.get_unchecked_mut(b as usize).assume_init_mut(),
            }
        })
    }

    /// Inserts or replaces the value of `k` and gives the token of its entry: the value is then reached by
    /// [`Self::value_by_token_mut`] without the key (a value received in parts).
    #[inline]
    pub fn insert_token(&mut self, k: K, value: V) -> Token {
        let b = k.to_byte();
        if let Some(at) = self.push_at(b) {
            unsafe { self.push(at, b, value) };
            return at << 8 | b as usize;
        }
        if self.is_flat() {
            self.insert(k, value);
        } else {
            self.insert_compressed(k, value);
        }
        self.token_of(k).unwrap()
    }

    /// The index of the value of `b` if it goes last in the compressed layout - as a map arrives: a key above all
    /// there, and room. One bitset check, no call.
    #[inline(always)]
    fn push_at(&self, b: u8) -> Option<usize> {
        match &self.store {
            Store::Compressed(v) if SMALL_VALUE < mem::size_of::<V>() && v.len() < v.capacity() && v.len() < FLAT_THRESHOLD && self.keys.none_from(b) => Some(v.len()),
            _ => None,
        }
    }

    /// # Safety
    /// `at` is of [`Self::push_at`] of `b`.
    #[inline(always)]
    unsafe fn push(&mut self, at: usize, b: u8, value: V) {
        if let Store::Compressed(v) = &mut self.store {
            // SAFETY: `at` is the length, below the capacity: the slot past the initialized ones.
            unsafe {
                v.as_mut_ptr().add(at).write(value);
                v.set_len(at + 1);
            }
        }
        self.keys.set1(b);
    }

    /// Returns the previous value of the key.
    #[inline(always)]
    pub fn insert(&mut self, k: K, value: V) -> Option<V> {
        let b = k.to_byte();
        if let Store::Flat(f) = &mut self.store {
            // one bit and one slot
            let slot = unsafe { f.get_unchecked_mut(b as usize) };
            if self.keys.set1(b) {
                slot.write(value);
                return None;
            }
            return Some(mem::replace(unsafe { slot.assume_init_mut() }, value));
        }
        if let Some(at) = self.push_at(b) {
            unsafe { self.push(at, b, value) };
            return None;
        }
        self.insert_compressed(k, value)
    }

    /// [`Self::insert`] into the compressed layout: large values, or the first entry.
    #[inline(never)]
    fn insert_compressed(&mut self, k: K, value: V) -> Option<V> {
        let b = k.to_byte();
        if let Some(v) = self.get_mut(k) {
            return Some(mem::replace(v, value));
        }
        self.insert_new(b, value);
        None
    }

    /// Returns a reference to the value, inserting `f()` if the key is absent.
    pub fn get_or_insert_with<F: FnOnce() -> V>(&mut self, k: K, f: F) -> &mut V {
        if !self.keys.contains(k) {
            self.insert_new(k.to_byte(), f());
        }
        self.get_mut(k).unwrap()
    }

    /// Inserts a key known to be absent.
    fn insert_new(&mut self, b: u8, value: V) {
        match &mut self.store {
            Store::Compressed(v) if v.len() < FLAT_THRESHOLD && SMALL_VALUE < mem::size_of::<V>() => {
                let at = self.keys.rank_below(b);
                if at == v.len() {
                    v.push(value); // keys coming in order - as a map sent from a ByteMap arrives
                } else {
                    v.insert(at, value)
                }
            }
            Store::Compressed(_) => {
                self.make_flat();
                self.insert_new(b, value);
                return;
            }
            Store::Flat(f) => {
                unsafe { f.get_unchecked_mut(b as usize) }.write(value);
            }
        }
        self.keys.set1(b);
    }

    /// Moves the compressed values into 256 slots.
    #[cold]
    fn make_flat(&mut self) {
        let mut flat = Box::new_uninit_slice(256);
        if let Store::Compressed(v) = &mut self.store {
            for (b, value) in self.keys.iter().map(|k: K| k.to_byte()).zip(v.drain(..)) {
                flat[b as usize].write(value);
            }
        }
        self.store = Store::Flat(flat);
    }

    /// Room for `additional` more entries: the flat layout at once where they would go there anyway
    /// (small values, or past 128 entries), no growth of the compressed one on the way.
    pub fn reserve(&mut self, additional: usize) {
        if additional == 0 {
            return; // an empty map received: no 256 slots for it
        }
        if let Store::Compressed(v) = &mut self.store {
            if mem::size_of::<V>() <= SMALL_VALUE || FLAT_THRESHOLD < v.len() + additional {
                self.make_flat();
            } else {
                v.reserve(additional);
            }
        }
    }

    pub fn remove(&mut self, k: K) -> Option<V> {
        let b = k.to_byte();
        if !self.keys.set0(b) {
            return None;
        }
        Some(match &mut self.store {
            Store::Compressed(v) => v.remove(self.keys.rank_below(b)),
            Store::Flat(f) => unsafe { f.get_unchecked(b as usize).assume_init_read() },
        })
    }

    /// Keeps only the entries for which `f` returns `true`.
    pub fn retain<F: FnMut(K, &mut V) -> bool>(&mut self, mut f: F) {
        let mut keys = self.keys.iter();
        match &mut self.store {
            Store::Compressed(v) => v.retain_mut(|value| {
                let k = keys.next().unwrap();
                let keep = f(k, value);
                if !keep {
                    self.keys.set0(k.to_byte());
                }
                keep
            }),
            Store::Flat(flat) => {
                for k in keys {
                    let b = k.to_byte();
                    let slot = unsafe { flat.get_unchecked_mut(b as usize) };
                    if !f(k, unsafe { slot.assume_init_mut() }) {
                        self.keys.set0(b);
                        unsafe { slot.assume_init_drop() };
                    }
                }
            }
        }
    }

    /// Back to the compressed layout when at most 128 entries remain; trims the compressed `Vec`.
    pub fn shrink_to_fit(&mut self) {
        match &mut self.store {
            Store::Compressed(v) => v.shrink_to_fit(),
            Store::Flat(flat) if self.keys.len() <= FLAT_THRESHOLD => {
                let mut v = Vec::with_capacity(self.keys.len());
                for k in self.keys.iter() {
                    v.push(unsafe { flat.get_unchecked(k.to_byte() as usize).assume_init_read() });
                }
                // The values moved out: the old slots are freed without dropping.
                self.store = Store::Compressed(v);
            }
            Store::Flat(_) => {}
        }
    }

    #[inline]
    pub fn keys(&self) -> byte_set::Iter<K> {
        self.keys.iter()
    }

    #[inline]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + FusedIterator + '_ {
        self.iter().map(|(_, v)| v)
    }

    #[inline]
    pub fn values_mut(&mut self) -> impl ExactSizeIterator<Item = &mut V> + FusedIterator + '_ {
        self.iter_mut().map(|(_, v)| v)
    }
}

impl<K: ByteKey, V: Clone> Clone for ByteMap<K, V> {
    fn clone(&self) -> Self {
        let store = match &self.store {
            Store::Compressed(v) => Store::Compressed(v.clone()),
            Store::Flat(f) => {
                let mut c = Box::new_uninit_slice(256);
                // A panicking `V::clone` leaks the clones made so far.
                for k in self.keys.iter() {
                    let b = k.to_byte() as usize;
                    c[b].write(unsafe { f[b].assume_init_ref() }.clone());
                }
                Store::Flat(c)
            }
        };
        Self { keys: self.keys, store }
    }
}

impl<K: ByteKey, V: PartialEq> PartialEq for ByteMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.keys == other.keys && self.values().zip(other.values()).all(|(a, b)| a == b)
    }
}

impl<K: ByteKey, V: Eq> Eq for ByteMap<K, V> {}

impl<K: ByteKey + fmt::Debug, V: fmt::Debug> fmt::Debug for ByteMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Panics if the key is absent.
impl<K: ByteKey, V> Index<K> for ByteMap<K, V> {
    type Output = V;
    #[inline]
    fn index(&self, k: K) -> &V {
        self.get(k).expect("key not found")
    }
}

impl<K: ByteKey, V> Extend<(K, V)> for ByteMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: ByteKey, V> FromIterator<(K, V)> for ByteMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut m = Self::new();
        m.extend(iter);
        m
    }
}

// iterators

/// Ascending keys. The value of the next key is the next compressed one, or the slot of the key.
pub struct Iter<'a, K, V> {
    keys: byte_set::Iter<K>,
    base: *const V,
    flat: bool,
    n: usize,
    _marker: PhantomData<&'a V>,
}

// The raw pointer stands for `&'a [V]`.
unsafe impl<K, V: Sync> Send for Iter<'_, K, V> {}
unsafe impl<K, V: Sync> Sync for Iter<'_, K, V> {}

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Self { keys: self.keys.clone(), ..*self }
    }
}

impl<'a, K: ByteKey, V> Iterator for Iter<'a, K, V> {
    type Item = (K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let k = self.keys.next()?;
        let i = if self.flat {
            k.to_byte() as usize
        } else {
            self.n += 1;
            self.n - 1
        };
        Some((k, unsafe { &*self.base.add(i) }))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.keys.size_hint()
    }
}

impl<K: ByteKey, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K: ByteKey, V> FusedIterator for Iter<'_, K, V> {}

pub struct IterMut<'a, K, V> {
    keys: byte_set::Iter<K>,
    base: *mut V,
    flat: bool,
    n: usize,
    _marker: PhantomData<&'a mut V>,
}

// The raw pointer stands for `&'a mut [V]`.
unsafe impl<K, V: Send> Send for IterMut<'_, K, V> {}
unsafe impl<K, V: Sync> Sync for IterMut<'_, K, V> {}

impl<'a, K: ByteKey, V> Iterator for IterMut<'a, K, V> {
    type Item = (K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let k = self.keys.next()?;
        let i = if self.flat {
            k.to_byte() as usize
        } else {
            self.n += 1;
            self.n - 1
        };
        // SAFETY: keys are distinct, so is every slot yielded.
        Some((k, unsafe { &mut *self.base.add(i) }))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.keys.size_hint()
    }
}

impl<K: ByteKey, V> ExactSizeIterator for IterMut<'_, K, V> {}
impl<K: ByteKey, V> FusedIterator for IterMut<'_, K, V> {}

enum IntoStore<V> {
    Compressed(std::vec::IntoIter<V>),
    Flat(Box<[MaybeUninit<V>]>),
}

pub struct IntoIter<K: ByteKey, V> {
    keys: byte_set::Iter<K>,
    store: IntoStore<V>,
}

impl<K: ByteKey, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<(K, V)> {
        let k = self.keys.next()?;
        let v = match &mut self.store {
            IntoStore::Compressed(it) => it.next()?,
            IntoStore::Flat(f) => unsafe { f.get_unchecked(k.to_byte() as usize).assume_init_read() },
        };
        Some((k, v))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.keys.size_hint()
    }
}

impl<K: ByteKey, V> ExactSizeIterator for IntoIter<K, V> {}
impl<K: ByteKey, V> FusedIterator for IntoIter<K, V> {}

impl<K: ByteKey, V> Drop for IntoIter<K, V> {
    fn drop(&mut self) {
        if let IntoStore::Flat(_) = self.store {
            for e in self.by_ref() {
                drop(e);
            }
        }
    }
}

impl<'a, K: ByteKey, V> IntoIterator for &'a ByteMap<K, V> {
    type Item = (K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, K: ByteKey, V> IntoIterator for &'a mut ByteMap<K, V> {
    type Item = (K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<K: ByteKey, V> IntoIterator for ByteMap<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    fn into_iter(mut self) -> Self::IntoIter {
        let keys = mem::take(&mut self.keys); // `self` drops nothing now
        let store = match mem::replace(&mut self.store, Store::Compressed(Vec::new())) {
            Store::Compressed(v) => IntoStore::Compressed(v.into_iter()),
            Store::Flat(f) => IntoStore::Flat(f),
        };
        IntoIter { keys: keys.iter(), store }
    }
}
