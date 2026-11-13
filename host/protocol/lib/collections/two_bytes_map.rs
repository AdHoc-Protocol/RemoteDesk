//! Map of two-byte keys with hash and flat layouts.
//!
//! The hash layout uses `u16` indices and [`h16`] for key hashing. The flat layout uses a
//! 65,536-bit presence set and directly indexed values. Growth switches to flat storage when it is
//! smaller than the expanded hash table. `shrink_to_fit` can switch back.
//!
//! For [`TwoBytesSet`](super::TwoBytesSet), zero-sized values leave only the 8 KiB bitset in flat
//! storage; the transition occurs above 512 keys.

use super::keys::TwoBytesKey;
use super::lohi::{RawIntoIter, RawIter, RawIterMut, RawTable};
use super::Token;
use core::fmt;
use core::iter::FusedIterator;
use core::marker::PhantomData;
use core::mem::{self, MaybeUninit};
use core::ops::Index;

const WORDS: usize = 1024;
const SLOTS: usize = 1 << 16;
/// The largest hash table: `u16` indices.
const MAX_HASH_CAPACITY: usize = 1 << 15;

#[inline(always)]
fn rehash(k: &u16) -> u64 {
    h16(*k)
}

/// Fibonacci hash using the high bits of a multiplied key.
///
/// Taking the key bits directly would cluster patterns such as multiples of 256. The mixed 32-bit
/// result supplies both bucket bits and the upper hash-class bits used by the filter.
#[inline(always)]
fn h16(k: u16) -> u64 {
    (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32
}

/// The flat layout. Slot `k` is initialized exactly when bit `k` is set.
struct Flat<V> {
    bits: Box<[u64; WORDS]>,
    values: Box<[MaybeUninit<V>]>,
    len: usize,
}

impl<V> Flat<V> {
    fn new() -> Self {
        let bits = vec![0u64; WORDS].into_boxed_slice().try_into().unwrap();
        // For `V = ()` (sets) the slots take no memory.
        Self {
            bits,
            values: Box::new_uninit_slice(SLOTS),
            len: 0,
        }
    }

    #[inline(always)]
    fn contains(&self, k: u16) -> bool {
        self.bits[(k >> 6) as usize] >> (k & 63) & 1 != 0
    }

    #[inline(always)]
    fn get(&self, k: u16) -> Option<&V> {
        if self.contains(k) {
            Some(unsafe { self.values.get_unchecked(k as usize).assume_init_ref() })
        } else {
            None
        }
    }

    #[inline(always)]
    fn get_mut(&mut self, k: u16) -> Option<&mut V> {
        if self.contains(k) {
            Some(unsafe { self.values.get_unchecked_mut(k as usize).assume_init_mut() })
        } else {
            None
        }
    }

    /// Returns the previous value of the key.
    #[inline(always)]
    fn insert(&mut self, k: u16, value: V) -> Option<V> {
        let w = &mut self.bits[(k >> 6) as usize];
        let bit = 1u64 << (k & 63);
        // SAFETY: `values` has `SLOTS` slots, one per `u16`.
        let slot = unsafe { self.values.get_unchecked_mut(k as usize) };
        if *w & bit != 0 {
            return Some(mem::replace(unsafe { slot.assume_init_mut() }, value));
        }
        *w |= bit;
        self.len += 1;
        slot.write(value);
        None
    }

    /// Inserts a key known to be absent.
    #[inline(always)]
    fn insert_new(&mut self, k: u16, v: V) -> &mut V {
        self.bits[(k >> 6) as usize] |= 1 << (k & 63);
        self.len += 1;
        unsafe { self.values.get_unchecked_mut(k as usize) }.write(v)
    }

    #[inline(always)]
    fn remove(&mut self, k: u16) -> Option<V> {
        let w = &mut self.bits[(k >> 6) as usize];
        let bit = 1u64 << (k & 63);
        if *w & bit == 0 {
            return None;
        }
        *w &= !bit;
        self.len -= 1;
        Some(unsafe { self.values.get_unchecked(k as usize).assume_init_read() })
    }

    /// Drops the values; `len` and `bits` are reset first (a panicking Drop leaks, never drops
    /// twice).
    fn clear(&mut self) {
        if self.len == 0 {
            return;
        }
        self.len = 0;
        if !mem::needs_drop::<V>() {
            self.bits.fill(0);
            return;
        }
        for i in 0..WORDS {
            let mut w = mem::take(&mut self.bits[i]);
            while w != 0 {
                let k = i << 6 | w.trailing_zeros() as usize;
                unsafe { self.values.get_unchecked_mut(k).assume_init_drop() };
                w &= w - 1;
            }
        }
    }

    /// Smallest set key ≥ `from`.
    #[inline]
    fn next_from(&self, from: usize) -> Option<u16> {
        if from >= SLOTS {
            return None;
        }
        let mut i = from >> 6;
        let mut w = self.bits[i] & (u64::MAX << (from & 63));
        loop {
            if w != 0 {
                return Some((i << 6 | w.trailing_zeros() as usize) as u16);
            }
            i += 1;
            if i == WORDS {
                return None;
            }
            w = self.bits[i];
        }
    }
}

impl<V: PartialEq> Flat<V> {
    /// The same keys are the same bits; the values of a key sit at one slot in both. Values of no
    /// size (a set) - the keys are all.
    #[inline(never)]
    fn eq_flat(&self, other: &Self) -> bool {
        if self.bits[..] != other.bits[..] {
            return false;
        }
        if mem::size_of::<V>() == 0 {
            return true;
        }
        let mut c = FlatCursor::new(&self.bits, self.len);
        while let Some(k) = c.next() {
            // SAFETY: bit `k` is set in both.
            if unsafe { self.values.get_unchecked(k as usize).assume_init_ref() != other.values.get_unchecked(k as usize).assume_init_ref() } {
                return false;
            }
        }
        true
    }
}

impl<V> Drop for Flat<V> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<V: Clone> Clone for Flat<V> {
    fn clone(&self) -> Self {
        let mut c = Self::new();
        // `len` grows as values are cloned: a panicking `V::clone` drops only the clones made.
        let mut k = 0;
        while let Some(key) = self.next_from(k) {
            c.insert_new(key, unsafe { self.values.get_unchecked(key as usize).assume_init_ref() }.clone());
            k = key as usize + 1;
        }
        c
    }
}

enum Repr<V> {
    Hash(RawTable<u16, V, u16, 32>),
    Flat(Flat<V>),
}

pub struct TwoBytesMap<K, V> {
    repr: Repr<V>,
    _key: PhantomData<fn() -> K>,
}

impl<K, V> TwoBytesMap<K, V> {
    /// First lo/hi slot or flat key token; see [`super::token`].
    #[inline(always)]
    pub fn token(&self) -> Option<Token> {
        match &self.repr {
            Repr::Hash(t) => t.token(),
            Repr::Flat(f) => f.next_from(0).map(|k| k as usize),
        }
    }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        match &self.repr {
            Repr::Hash(t) => t.next_token(token),
            Repr::Flat(f) => {
                if !self.contains_token(token) {
                    return None;
                }
                f.next_from(token + 1).map(|k| k as usize)
            }
        }
    }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        match &self.repr {
            Repr::Hash(t) => t.contains_token(token),
            Repr::Flat(f) => token < SLOTS && f.contains(token as u16),
        }
    }

    #[inline(always)]
    pub fn value_by_token(&self, token: Token) -> Option<&V> {
        match &self.repr {
            Repr::Hash(t) => t.get_by_token(token).map(|(_, v)| v),
            Repr::Flat(f) => {
                if token < SLOTS {
                    f.get(token as u16)
                } else {
                    None
                }
            }
        }
    }

    #[inline(always)]
    pub fn value_by_token_mut(&mut self, token: Token) -> Option<&mut V> {
        match &mut self.repr {
            Repr::Hash(t) => t.get_by_token_mut(token),
            Repr::Flat(f) => {
                if token < SLOTS {
                    f.get_mut(token as u16)
                } else {
                    None
                }
            }
        }
    }

    /// Does not allocate.
    #[inline]
    pub fn new() -> Self {
        Self {
            repr: Repr::Hash(RawTable::new()),
            _key: PhantomData,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let repr = match Self::hash_capacity_for(capacity) {
            Some(_) => Repr::Hash(RawTable::with_capacity(capacity)),
            None => Repr::Flat(Flat::new()),
        };
        Self { repr, _key: PhantomData }
    }

    /// The largest hash capacity not above the flat layout: a full table below it grows and stays
    /// hashed. A slot costs its `u16` key and value, two `u16` buckets, two filter bytes and about
    /// half a `u16` link.
    const MAX_HASH: usize = {
        let mut cap = MAX_HASH_CAPACITY;
        while cap > 4 && cap * (2 + mem::size_of::<V>() + 7) > WORDS * 8 + SLOTS * mem::size_of::<V>() {
            cap >>= 1;
        }
        cap
    };

    /// The hash capacity for `n` entries, or `None` when the flat layout is smaller.
    fn hash_capacity_for(n: usize) -> Option<usize> {
        if n > Self::MAX_HASH {
            return None;
        }
        Some(RawTable::<u16, V, u16, 32>::capacity_for(n.max(1)))
    }

    #[inline]
    pub fn len(&self) -> usize {
        match &self.repr {
            Repr::Hash(t) => t.len(),
            Repr::Flat(f) => f.len,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `true` in the flat (65 536-slot) layout.
    #[inline]
    pub fn is_flat(&self) -> bool {
        matches!(self.repr, Repr::Flat(_))
    }

    /// Entries the map holds before it reorganizes.
    #[inline]
    pub fn capacity(&self) -> usize {
        match &self.repr {
            Repr::Hash(t) => t.capacity(),
            Repr::Flat(_) => SLOTS,
        }
    }

    /// Removes all entries, keeping the layout and its memory.
    pub fn clear(&mut self) {
        match &mut self.repr {
            Repr::Hash(t) => t.clear_by(rehash),
            Repr::Flat(f) => f.clear(),
        }
    }

    /// Hash layout: the lo region, then the hi region. Flat layout: ascending keys.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            inner: match &self.repr {
                Repr::Hash(t) => IterRepr::Hash(t.iter()),
                Repr::Flat(f) => IterRepr::Flat(FlatCursor::new(&f.bits, f.len), f.values.as_ptr() as *const V),
            },
            _key: PhantomData,
        }
    }

    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut {
            inner: match &mut self.repr {
                Repr::Hash(t) => IterMutRepr::Hash(t.iter_mut()),
                Repr::Flat(f) => IterMutRepr::Flat(FlatCursor::new(&f.bits, f.len), f.values.as_mut_ptr() as *mut V),
            },
            _key: PhantomData,
        }
    }
}

impl<K, V> Default for TwoBytesMap<K, V> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K: TwoBytesKey, V> TwoBytesMap<K, V> {
    #[inline(always)]
    pub fn token_of(&self, key: K) -> Option<Token> {
        let key = key.to_u16();
        match &self.repr {
            Repr::Hash(t) => t.find(h16(key), |e| *e == key),
            Repr::Flat(f) => f.contains(key).then_some(key as usize),
        }
    }

    #[inline(always)]
    pub fn key_by_token(&self, token: Token) -> Option<K> {
        match &self.repr {
            Repr::Hash(t) => t.get_by_token(token).map(|(e, _)| K::from_u16(*e)),
            Repr::Flat(f) => (token < SLOTS && f.contains(token as u16)).then(|| K::from_u16(token as u16)),
        }
    }

    #[inline(always)]
    pub fn entry_by_token(&self, token: Token) -> Option<(K, &V)> {
        match &self.repr {
            Repr::Hash(t) => t.get_by_token(token).map(|(e, v)| (K::from_u16(*e), v)),
            Repr::Flat(f) => {
                if token < SLOTS {
                    f.get(token as u16).map(|v| (K::from_u16(token as u16), v))
                } else {
                    None
                }
            }
        }
    }

    /// The `n`-th entry of a pass entry after entry (the generated sending): `at` keeps its place
    /// ([`super::token::Seq`]). Hash: the next token is the next entry; flat: the next set bit.
    ///
    /// # Safety
    /// `at` is the cursor of this pass over this map: the pass starts at `n` 0 with any `at`, and
    /// the map is not changed between its steps. A cursor of another pass or of another collection
    /// reads a slot that is not set.
    #[inline(always)]
    pub unsafe fn entry_seq(&self, n: usize, at: &mut super::token::Seq) -> (super::token::Key<K>, &V) {
        match &self.repr {
            // the `n`-th entry of a hash table is at hand: `at` is not needed
            Repr::Hash(t) => {
                let (e, v) = t.at(n);
                (super::token::Key(K::from_u16(*e)), v)
            }
            Repr::Flat(f) => {
                let k = super::token::seq_bit(n, at, &f.bits[..]);
                (super::token::Key(K::from_u16(k as u16)), unsafe { f.values.get_unchecked(k).assume_init_ref() })
            }
        }
    }

    #[inline]
    pub fn contains_key(&self, k: K) -> bool {
        let k = k.to_u16();
        match &self.repr {
            Repr::Hash(t) => t.find(h16(k), |e| *e == k).is_some(),
            Repr::Flat(f) => f.contains(k),
        }
    }

    #[inline]
    pub fn get(&self, k: K) -> Option<&V> {
        let k = k.to_u16();
        match &self.repr {
            Repr::Hash(t) => t.get(h16(k), |e| *e == k).map(|(_, v)| v),
            Repr::Flat(f) => f.get(k),
        }
    }

    #[inline]
    pub fn get_mut(&mut self, k: K) -> Option<&mut V> {
        let k = k.to_u16();
        match &mut self.repr {
            Repr::Hash(t) => t.get_mut(h16(k), |e| *e == k),
            Repr::Flat(f) => f.get_mut(k),
        }
    }

    /// Inserts or replaces the value of `k` and gives the token of its entry: the value is then reached by
    /// [`Self::value_by_token_mut`] without the key (a value received in parts).
    #[inline(always)]
    pub fn insert_token(&mut self, k: K, value: V) -> Token {
        let k = k.to_u16();
        match &mut self.repr {
            Repr::Hash(t) => {
                let h = h16(k);
                if let Some(i) = t.find_for_insert(h, |e| *e == k) {
                    *unsafe { t.at_mut(i) } = value;
                    return i;
                }
                if t.len() < Self::MAX_HASH {
                    return t.insert_unique_token(h, k, value, rehash);
                }
            }
            Repr::Flat(f) => {
                f.insert(k, value);
                return k as usize;
            }
        }
        self.insert_grown(k, value);
        self.token_of(K::from_u16(k)).unwrap()
    }

    /// Returns the previous value of the key.
    #[inline(always)]
    pub fn insert(&mut self, k: K, value: V) -> Option<V> {
        let k = k.to_u16();
        match &mut self.repr {
            Repr::Hash(t) => {
                let h = h16(k);
                if let Some(i) = t.find_for_insert(h, |e| *e == k) {
                    let e = unsafe { t.at_mut(i) };
                    return Some(mem::replace(e, value));
                }
                // below the largest hash capacity the layout stays: a full table grows
                if t.len() < Self::MAX_HASH {
                    t.insert_unique(h, k, value, rehash);
                    return None;
                }
            }
            Repr::Flat(f) => return f.insert(k, value),
        }
        self.insert_grown(k, value);
        None
    }

    /// [`Self::insert`] into a full table of the largest hash capacity: goes flat.
    #[cold]
    #[inline(never)]
    fn insert_grown(&mut self, k: u16, value: V) {
        self.insert_new(k, value);
    }

    /// Returns a reference to the value, inserting `f()` if the key is absent.
    pub fn get_or_insert_with<F: FnOnce() -> V>(&mut self, k: K, f: F) -> &mut V {
        if !self.contains_key(k) {
            return self.insert_new(k.to_u16(), f());
        }
        self.get_mut(k).unwrap()
    }

    /// Inserts a key known to be absent.
    #[inline]
    fn insert_new(&mut self, k: u16, value: V) -> &mut V {
        if let Repr::Hash(t) = &self.repr {
            if t.len() == t.capacity() && Self::hash_capacity_for(t.len() + 1).is_none() {
                self.make_flat();
            }
        }
        match &mut self.repr {
            Repr::Hash(t) => t.insert_unique(h16(k), k, value, rehash),
            Repr::Flat(f) => f.insert_new(k, value),
        }
    }

    #[cold]
    #[inline(never)]
    fn make_flat(&mut self) {
        let mut flat = Flat::new();
        if let Repr::Hash(t) = mem::replace(&mut self.repr, Repr::Hash(RawTable::new())) {
            for (key, value) in t.into_iter() {
                flat.insert_new(key, value);
            }
        }
        self.repr = Repr::Flat(flat);
    }

    #[inline]
    pub fn remove(&mut self, k: K) -> Option<V> {
        let k = k.to_u16();
        match &mut self.repr {
            Repr::Hash(t) => t.remove(h16(k), |e| *e == k, rehash).map(|(_, v)| v),
            Repr::Flat(f) => f.remove(k),
        }
    }

    /// Keeps only the entries for which `f` returns `true`.
    pub fn retain<F: FnMut(K, &mut V) -> bool>(&mut self, mut f: F) {
        match &mut self.repr {
            Repr::Hash(t) => t.retain(|e, v| f(K::from_u16(*e), v), rehash),
            Repr::Flat(flat) => {
                let mut k = 0;
                while let Some(key) = flat.next_from(k) {
                    k = key as usize + 1;
                    let keep = f(K::from_u16(key), unsafe { flat.values.get_unchecked_mut(key as usize).assume_init_mut() });
                    if !keep {
                        drop(flat.remove(key));
                    }
                }
            }
        }
    }

    /// Makes room for `additional` more entries; goes flat if that is the smaller layout.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        if let Repr::Hash(t) = &mut self.repr {
            // room left: a capacity the table has reached is never larger than the flat layout
            if additional <= t.capacity() - t.len() {
                return;
            }
            self.reserve_more(additional);
        }
    }

    #[cold]
    #[inline(never)]
    fn reserve_more(&mut self, additional: usize) {
        if let Repr::Hash(t) = &mut self.repr {
            match Self::hash_capacity_for(t.len() + additional) {
                Some(_) => t.reserve(additional, rehash),
                None => self.make_flat(),
            }
        }
    }

    /// Back to the hash layout when it is the smaller one; trims the hash table.
    pub fn shrink_to_fit(&mut self) {
        match &mut self.repr {
            Repr::Hash(t) => t.shrink_to_fit(rehash),
            Repr::Flat(flat) => {
                if let Some(cap) = Self::hash_capacity_for(flat.len) {
                    let mut t = RawTable::with_capacity(cap);
                    let mut k = 0;
                    while let Some(key) = flat.next_from(k) {
                        k = key as usize + 1;
                        let value = flat.remove(key).unwrap();
                        t.insert_unique(h16(key), key, value, rehash);
                    }
                    self.repr = Repr::Hash(t);
                }
            }
        }
    }

    #[inline]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = K> + FusedIterator + '_ {
        self.iter().map(|(k, _)| k)
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

impl<K, V: Clone> Clone for TwoBytesMap<K, V> {
    fn clone(&self) -> Self {
        let repr = match &self.repr {
            Repr::Hash(t) => Repr::Hash(t.clone()),
            Repr::Flat(f) => Repr::Flat(f.clone()),
        };
        Self { repr, _key: PhantomData }
    }
}

impl<K: TwoBytesKey, V: PartialEq> PartialEq for TwoBytesMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        match (&self.repr, &other.repr) {
            (Repr::Flat(a), Repr::Flat(b)) => a.eq_flat(b),
            (Repr::Hash(t), _) => t.iter().all(|(e, v)| other.get(K::from_u16(*e)) == Some(v)),
            _ => self.iter().all(|(k, v)| other.get(k) == Some(v)),
        }
    }
}

impl<K: TwoBytesKey, V: Eq> Eq for TwoBytesMap<K, V> {}

impl<K: TwoBytesKey + fmt::Debug, V: fmt::Debug> fmt::Debug for TwoBytesMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Panics if the key is absent.
impl<K: TwoBytesKey, V> Index<K> for TwoBytesMap<K, V> {
    type Output = V;
    #[inline]
    fn index(&self, k: K) -> &V {
        self.get(k).expect("key not found")
    }
}

impl<K: TwoBytesKey, V> Extend<(K, V)> for TwoBytesMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: TwoBytesKey, V> FromIterator<(K, V)> for TwoBytesMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut m = Self::new();
        m.extend(iter);
        m
    }
}

// iterators

/// Walks the set bits of the flat layout: one word load per 64 keys, `tzcnt` per key.
#[derive(Clone)]
struct FlatCursor<'a> {
    bits: &'a [u64; WORDS],
    word: usize,
    cur: u64,
    remaining: usize,
}

impl<'a> FlatCursor<'a> {
    #[inline]
    fn new(bits: &'a [u64; WORDS], len: usize) -> Self {
        Self {
            bits,
            word: 0,
            cur: bits[0],
            remaining: len,
        }
    }

    #[inline]
    fn next(&mut self) -> Option<u16> {
        if self.remaining == 0 {
            return None;
        }
        while self.cur == 0 {
            self.word += 1;
            // SAFETY: `remaining` keys are set at or past `word`, so it stays below `WORDS`.
            self.cur = unsafe { *self.bits.get_unchecked(self.word) };
        }
        let k = (self.word << 6) as u16 | self.cur.trailing_zeros() as u16;
        self.cur &= self.cur - 1;
        self.remaining -= 1;
        Some(k)
    }
}

enum IterRepr<'a, V> {
    Hash(RawIter<'a, u16, V>),
    Flat(FlatCursor<'a>, *const V),
}

pub struct Iter<'a, K, V> {
    inner: IterRepr<'a, V>,
    _key: PhantomData<fn() -> K>,
}

// The raw pointer stands for `&'a [V]`.
unsafe impl<K, V: Sync> Send for Iter<'_, K, V> {}
unsafe impl<K, V: Sync> Sync for Iter<'_, K, V> {}

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        let inner = match &self.inner {
            IterRepr::Hash(it) => IterRepr::Hash(it.clone()),
            IterRepr::Flat(c, p) => IterRepr::Flat(c.clone(), *p),
        };
        Self { inner, _key: PhantomData }
    }
}

impl<'a, K: TwoBytesKey, V> Iterator for Iter<'a, K, V> {
    type Item = (K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            IterRepr::Hash(it) => it.next().map(|(e, v)| (K::from_u16(*e), v)),
            IterRepr::Flat(c, values) => c.next().map(|k| (K::from_u16(k), unsafe { &*values.add(k as usize) })),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match &self.inner {
            IterRepr::Hash(it) => it.len(),
            IterRepr::Flat(c, _) => c.remaining,
        };
        (n, Some(n))
    }

    #[inline]
    fn fold<B, F: FnMut(B, Self::Item) -> B>(self, init: B, mut f: F) -> B {
        match self.inner {
            IterRepr::Hash(it) => it.fold(init, |acc, (e, v)| f(acc, (K::from_u16(*e), v))),
            IterRepr::Flat(mut c, values) => {
                let mut acc = init;
                while let Some(k) = c.next() {
                    acc = f(acc, (K::from_u16(k), unsafe { &*values.add(k as usize) }));
                }
                acc
            }
        }
    }
}

impl<K: TwoBytesKey, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K: TwoBytesKey, V> FusedIterator for Iter<'_, K, V> {}

enum IterMutRepr<'a, V> {
    Hash(RawIterMut<'a, u16, V>),
    Flat(FlatCursor<'a>, *mut V),
}

pub struct IterMut<'a, K, V> {
    inner: IterMutRepr<'a, V>,
    _key: PhantomData<fn() -> K>,
}

// The raw pointer stands for `&'a mut [V]`.
unsafe impl<K, V: Send> Send for IterMut<'_, K, V> {}
unsafe impl<K, V: Sync> Sync for IterMut<'_, K, V> {}

impl<'a, K: TwoBytesKey, V> Iterator for IterMut<'a, K, V> {
    type Item = (K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            IterMutRepr::Hash(it) => it.next().map(|(e, v)| (K::from_u16(*e), v)),
            // SAFETY: keys are distinct, so is every slot yielded.
            IterMutRepr::Flat(c, values) => c.next().map(|k| (K::from_u16(k), unsafe { &mut *values.add(k as usize) })),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match &self.inner {
            IterMutRepr::Hash(it) => it.len(),
            IterMutRepr::Flat(c, _) => c.remaining,
        };
        (n, Some(n))
    }
}

impl<K: TwoBytesKey, V> ExactSizeIterator for IterMut<'_, K, V> {}
impl<K: TwoBytesKey, V> FusedIterator for IterMut<'_, K, V> {}

enum IntoIterRepr<V> {
    Hash(RawIntoIter<u16, V>),
    /// The next key to look at.
    Flat(Flat<V>, usize),
}

pub struct IntoIter<K, V> {
    inner: IntoIterRepr<V>,
    _key: PhantomData<fn() -> K>,
}

impl<K: TwoBytesKey, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<(K, V)> {
        match &mut self.inner {
            IntoIterRepr::Hash(it) => it.next().map(|(e, v)| (K::from_u16(e), v)),
            IntoIterRepr::Flat(f, from) => {
                let k = f.next_from(*from)?;
                *from = k as usize + 1;
                f.remove(k).map(|v| (K::from_u16(k), v))
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match &self.inner {
            IntoIterRepr::Hash(it) => it.len(),
            IntoIterRepr::Flat(f, _) => f.len,
        };
        (n, Some(n))
    }
}

impl<K: TwoBytesKey, V> ExactSizeIterator for IntoIter<K, V> {}
impl<K: TwoBytesKey, V> FusedIterator for IntoIter<K, V> {}

impl<'a, K: TwoBytesKey, V> IntoIterator for &'a TwoBytesMap<K, V> {
    type Item = (K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, K: TwoBytesKey, V> IntoIterator for &'a mut TwoBytesMap<K, V> {
    type Item = (K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<K: TwoBytesKey, V> IntoIterator for TwoBytesMap<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        let inner = match self.repr {
            Repr::Hash(t) => IntoIterRepr::Hash(t.into_iter()),
            Repr::Flat(f) => IntoIterRepr::Flat(f, 0),
        };
        IntoIter { inner, _key: PhantomData }
    }
}
