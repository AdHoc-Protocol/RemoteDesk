//! Split key/value hash table shared by the collection implementations.
//!
//! Keys and values occupy separate arrays at matching indices, so lookup reads values only after a
//! key match. Entries in `lo` have chain links; entries in `hi` terminate chains and need no link.
//! Buckets store a head index plus one, with zero denoting an empty bucket.
//!
//! A separate filter byte per bucket rejects missing hash classes before touching entries. Removal
//! may leave conservative filter bits until rebuilding. Twice as many buckets as slots keep the
//! maximum load factor at 0.5. Initial links and the primary arrays share one allocation;
//! additional links use separate storage.
//!
//! Lookups receive a hash and equality closure. Relocation receives a rehash closure, which may use
//! cached hashes. Indices are `u32`, or `u16` for two-byte key collections. `HASH_BITS` specifies
//! the hash width preserved across rebuilds.

use core::iter::FusedIterator;
use core::marker::PhantomData;
use core::mem::{self, MaybeUninit};
use core::ptr::{self, NonNull};
use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use super::token::{dense_contains, dense_first, dense_next, Token};

/// Minimum capacity of the first allocation.
const MIN_CAPACITY: usize = 4;
/// Buckets per slot: `buckets = cap << BUCKET_SHIFT`.
const BUCKET_SHIFT: u32 = 1;
/// Minimum length of `links` on its first allocation.
const MIN_LINKS: usize = 16;
const NONE: usize = usize::MAX;

/// Shared zero bucket of an empty table, read as `u16` or `u32`. Never written to: insertion
/// allocates real buckets first.
static EMPTY_BUCKET: u32 = 0;
static EMPTY_FILTER: u8 = 0;

/// The type of bucket and link indices.
pub(crate) trait RawIndex: Copy + 'static {
    /// Indices are stored as `idx + 1` and there are `cap << BUCKET_SHIFT` buckets.
    const MAX_CAPACITY: usize;
    fn new(i: usize) -> Self;
    fn get(self) -> usize;
    /// A zeroed slice (`alloc_zeroed`).
    fn zeroed(n: usize) -> Box<[Self]>;
    /// Select bits outside the bucket index, within the hash bits the collection preserves.
    #[inline(always)]
    fn filter_bit(h: u64, _bucket_mask: usize, hash_bits: u32) -> u8 {
        1u8 << ((h >> (hash_bits - 3)) & 7)
    }
}

impl RawIndex for u32 {
    const MAX_CAPACITY: usize = 1 << 30;
    #[inline(always)]
    fn new(i: usize) -> Self {
        i as u32
    }
    #[inline(always)]
    fn get(self) -> usize {
        self as usize
    }
    #[inline]
    fn zeroed(n: usize) -> Box<[Self]> {
        vec![0u32; n].into_boxed_slice()
    }
}

impl RawIndex for u16 {
    const MAX_CAPACITY: usize = 1 << 15;
    #[inline(always)]
    fn new(i: usize) -> Self {
        i as u16
    }
    #[inline(always)]
    fn get(self) -> usize {
        self as usize
    }
    #[inline]
    fn zeroed(n: usize) -> Box<[Self]> {
        vec![0u16; n].into_boxed_slice()
    }
}

pub(crate) struct RawTable<K, V = (), I: RawIndex = u32, const HASH_BITS: u32 = { usize::BITS }> {
    /// `bucket_mask + 1` buckets; points to `EMPTY_BUCKET` when `cap == 0`.
    buckets: NonNull<I>,
    /// One hash-class mask per bucket; static zero when capacity is zero.
    filters: NonNull<u8>,
    bucket_mask: usize,
    /// Key storage, including cached hashes when the owning collection uses them. The slot count is
    /// derived from the bucket count rather than stored separately; an empty table uses a static
    /// bucket and has zero capacity.
    keys: NonNull<K>,
    /// The value of `keys[i]`; no memory when `V` is `()`.
    vals: NonNull<V>,
    /// Links of the lo region, `links_len` of them: in the allocation of the table, or past its
    /// share an allocation of their own ([`Self::grow_links`]). Invariant: `links_len >= lo_size`.
    links: NonNull<I>,
    links_len: usize,
    lo_size: usize,
    /// Start of the hi region, `cap` when it is empty: the table is full when
    /// `lo_size == hi_start`. Kept instead of the size of hi: the capacity is not stored (half the
    /// buckets) and stays off the path of an insertion and a removal.
    hi_start: usize,
}

// `NonNull` disables auto Send/Sync; the table owns its data like any ordinary collection.
unsafe impl<K: Send, V: Send, I: RawIndex, const HASH_BITS: u32> Send for RawTable<K, V, I, HASH_BITS> {}
unsafe impl<K: Sync, V: Sync, I: RawIndex, const HASH_BITS: u32> Sync for RawTable<K, V, I, HASH_BITS> {}

#[inline(always)]
fn empty_buckets<I>() -> NonNull<I> {
    NonNull::from(&EMPTY_BUCKET).cast()
}

#[inline(always)]
fn empty_filters() -> NonNull<u8> {
    NonNull::from(&EMPTY_FILTER)
}

/// The allocation of a table of `cap` slots: `cap << BUCKET_SHIFT` buckets at its start, the
/// filters right after them - both zeroed by one fill - then the keys, the values and `links_len`
/// links. Offsets in bytes.
struct Parts {
    layout: Layout,
    filters: usize,
    keys: usize,
    vals: usize,
    links: usize,
    links_len: usize,
}

/// At load ≤ 0.5 about 20% of the entries are in lo: a quarter of the slots holds the links of a
/// table that fills.
fn parts<K, V, I>(cap: usize) -> Parts {
    let n = cap << BUCKET_SHIFT;
    let links_len = (cap / 4).max(MIN_LINKS).min(cap);
    let l = Layout::array::<I>(n).unwrap();
    let (l, filters) = l.extend(Layout::array::<u8>(n).unwrap()).unwrap();
    let (l, keys) = l.extend(Layout::array::<K>(cap).unwrap()).unwrap();
    let (l, vals) = l.extend(Layout::array::<V>(cap).unwrap()).unwrap();
    let (layout, links) = l.extend(Layout::array::<I>(links_len).unwrap()).unwrap();
    Parts { layout, filters, keys, vals, links, links_len }
}

/// Frees links of their own allocation ([`RawTable::grow_links`]).
unsafe fn free_links<I>(p: NonNull<I>, n: usize) {
    drop(Box::from_raw(ptr::slice_from_raw_parts_mut(p.as_ptr(), n)));
}

/// The allocation of a table taken off it ([`RawTable::take_memory`]): the entries in its slots are
/// read out, then it is freed. Of an empty table - nothing to free.
pub(crate) struct Memory<K, V> {
    base: NonNull<u8>,
    layout: Layout,
    keys: NonNull<K>,
    vals: NonNull<V>,
}

impl<K, V> Memory<K, V> {
    /// Frees it without dropping what is in the slots.
    #[inline]
    unsafe fn free(&self) {
        if self.layout.size() != 0 {
            dealloc(self.base.as_ptr(), self.layout);
        }
    }
}

/// `range` of slots known to be initialized.
#[inline(always)]
unsafe fn drop_slots<T>(p: NonNull<T>, range: core::ops::Range<usize>) {
    ptr::drop_in_place(ptr::slice_from_raw_parts_mut(p.as_ptr().add(range.start), range.len()));
}

impl<K, V, I: RawIndex, const HASH_BITS: u32> RawTable<K, V, I, HASH_BITS> {
    /// Does not allocate.
    #[inline]
    pub fn new() -> Self {
        Self {
            buckets: empty_buckets(),
            filters: empty_filters(),
            bucket_mask: 0,
            keys: NonNull::dangling(),
            vals: NonNull::dangling(),
            links: NonNull::dangling(),
            links_len: 0,
            lo_size: 0,
            hi_start: 0,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let mut t = Self::new();
        if capacity > 0 {
            // An empty table moves no entries: the rehash closure is never called.
            t.resize(Self::capacity_for(capacity), |_| unreachable!());
        }
        t
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.lo_size + self.capacity() - self.hi_start
    }

    /// Half the buckets; the single static bucket of an empty table gives 0.
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        (self.bucket_mask + 1) >> BUCKET_SHIFT
    }

    #[inline(always)]
    fn hi_start(&self) -> usize {
        self.hi_start
    }

    /// The `n`-th entry in the order of [`Self::iter`] (lo, then hi), O(1). A pass by number,
    /// unlike an iterator, does not borrow the table between calls: the sending of a pack goes on
    /// with it after any pause.
    #[inline(always)]
    pub fn at(&self, n: usize) -> (&K, &V) {
        assert!(n < self.len());
        let i = if n < self.lo_size { n } else { self.hi_start() + n - self.lo_size };
        unsafe { (self.key(i), self.val(i)) }
    }

    #[inline(always)]
    pub fn token(&self) -> Option<Token> {
        dense_first(self.lo_size, self.hi_start(), self.capacity())
    }

    #[inline(always)]
    pub fn next_token(&self, token: Token) -> Option<Token> {
        dense_next(token, self.lo_size, self.hi_start(), self.capacity())
    }

    #[inline(always)]
    pub fn contains_token(&self, token: Token) -> bool {
        dense_contains(token, self.lo_size, self.hi_start(), self.capacity())
    }

    #[inline(always)]
    pub fn get_by_token(&self, token: Token) -> Option<(&K, &V)> {
        if self.contains_token(token) { Some(unsafe { (self.key(token), self.val(token)) }) } else { None }
    }

    #[inline(always)]
    pub fn get_by_token_mut(&mut self, token: Token) -> Option<&mut V> {
        if self.contains_token(token) { Some(unsafe { self.val_mut(token) }) } else { None }
    }

    #[inline(always)]
    fn has_buckets(&self) -> bool {
        self.buckets != empty_buckets()
    }

    #[inline(always)]
    pub fn capacity_for(n: usize) -> usize {
        assert!(n <= I::MAX_CAPACITY, "hash table capacity overflow");
        n.next_power_of_two().max(MIN_CAPACITY)
    }

    // low-level helpers

    #[inline(always)]
    fn bucket(&self, h: u64) -> *mut I {
        // SAFETY: bucket_mask + 1 buckets are available (an empty table has the single static one).
        unsafe { self.buckets.as_ptr().add(h as usize & self.bucket_mask) }
    }

    #[inline(always)]
    fn filter(&self, h: u64) -> *mut u8 {
        // SAFETY: same length as buckets, including the static empty-table filter.
        unsafe { self.filters.as_ptr().add(h as usize & self.bucket_mask) }
    }

    #[inline(always)]
    fn filter_bit(&self, h: u64) -> u8 {
        I::filter_bit(h, self.bucket_mask, HASH_BITS)
    }

    #[inline(always)]
    unsafe fn key(&self, i: usize) -> &K {
        &*self.keys.as_ptr().add(i)
    }

    #[inline(always)]
    unsafe fn val(&self, i: usize) -> &V {
        &*self.vals.as_ptr().add(i)
    }

    #[inline(always)]
    unsafe fn val_mut(&mut self, i: usize) -> &mut V {
        &mut *self.vals.as_ptr().add(i)
    }

    /// Moves the entry out of slot `i`, leaving it uninitialized.
    #[inline(always)]
    unsafe fn read(&self, i: usize) -> (K, V) {
        (ptr::read(self.keys.as_ptr().add(i)), ptr::read(self.vals.as_ptr().add(i)))
    }

    #[inline(always)]
    unsafe fn write(&mut self, i: usize, k: K, v: V) {
        ptr::write(self.keys.as_ptr().add(i), k);
        ptr::write(self.vals.as_ptr().add(i), v);
    }

    #[inline(always)]
    unsafe fn link(&self, i: usize) -> usize {
        (*self.links.as_ptr().add(i)).get()
    }

    #[inline(always)]
    unsafe fn set_link(&mut self, i: usize, v: usize) {
        *self.links.as_ptr().add(i) = I::new(v);
    }

    /// Gives the table without memory ([`Self::take_memory`]) the allocation of `cap` slots, empty:
    /// zero buckets and filters, the slots and the links uninitialized.
    fn install(&mut self, cap: usize) {
        debug_assert!(!self.has_buckets());
        let p = parts::<K, V, I>(cap);
        let n = cap << BUCKET_SHIFT;
        unsafe {
            let base = alloc(p.layout);
            if base.is_null() {
                handle_alloc_error(p.layout);
            }
            ptr::write_bytes(base, 0, p.filters + n);
            self.buckets = NonNull::new_unchecked(base as *mut I);
            self.filters = NonNull::new_unchecked(base.add(p.filters));
            self.keys = NonNull::new_unchecked(base.add(p.keys) as *mut K);
            self.vals = NonNull::new_unchecked(base.add(p.vals) as *mut V);
            self.links = NonNull::new_unchecked(base.add(p.links) as *mut I);
        }
        self.links_len = p.links_len;
        self.bucket_mask = n - 1;
        self.lo_size = 0;
        self.hi_start = cap;
    }

    /// Takes the memory off the table, left empty without memory: the entries stay in the slots of
    /// what it returns, links of their own allocation are freed.
    fn take_memory(&mut self) -> Memory<K, V> {
        let m = if self.has_buckets() {
            let p = parts::<K, V, I>(self.capacity());
            if self.links_len != p.links_len {
                unsafe { free_links(self.links, self.links_len) };
            }
            Memory { base: self.buckets.cast(), layout: p.layout, keys: self.keys, vals: self.vals }
        } else {
            Memory { base: NonNull::dangling(), layout: Layout::new::<()>(), keys: NonNull::dangling(), vals: NonNull::dangling() }
        };
        self.buckets = empty_buckets();
        self.filters = empty_filters();
        self.bucket_mask = 0;
        self.keys = NonNull::dangling();
        self.vals = NonNull::dangling();
        self.links = NonNull::dangling();
        self.links_len = 0;
        self.lo_size = 0;
        self.hi_start = 0;
        m
    }

    /// Drops all entries without touching buckets or links.
    fn drop_entries(&mut self) {
        let (lo, hi) = (self.lo_size, self.hi_start());
        // Reset the counters before dropping: a panic in a Drop cannot cause a double drop.
        self.lo_size = 0;
        self.hi_start = self.capacity();
        let cap = self.capacity();
        unsafe {
            if mem::needs_drop::<K>() {
                drop_slots(self.keys, 0..lo);
                drop_slots(self.keys, hi..cap);
            }
            if mem::needs_drop::<V>() {
                drop_slots(self.vals, 0..lo);
                drop_slots(self.vals, hi..cap);
            }
        }
    }

    /// Removes all entries, keeping the allocated memory.
    pub fn clear(&mut self) {
        if self.len() == 0 {
            return;
        }
        self.drop_entries();
        // the filters right after the buckets (`Parts`)
        unsafe { ptr::write_bytes(self.buckets.as_ptr() as *mut u8, 0, (self.bucket_mask + 1) * (mem::size_of::<I>() + 1)) };
    }

    /// [`Self::clear`] that costs the entries, not the capacity, when they are few: only their
    /// buckets and filters are zeroed - every bucket in use is the bucket of one of them. A table
    /// that once grew large and now holds a few (a map received pack after pack) is not wiped whole
    /// each time.
    pub fn clear_by(&mut self, rehash: impl Fn(&K) -> u64) {
        let n = self.len();
        if n == 0 {
            return;
        }
        // Zeroing them all costs about as much as zeroing a key per 32 buckets, in the cache and past
        // it alike, and 3 keys more.
        if 3 + ((self.bucket_mask + 1) >> 5) <= n {
            return self.clear();
        }
        let (lo, gap) = (self.lo_size, self.hi_start() - self.lo_size);
        // one short loop, the slot of the j-th entry by a select: a chain stepped by `for` costs a
        // branch an entry
        for j in 0..n {
            unsafe {
                let h = rehash(self.key(if j < lo { j } else { j + gap }));
                *self.bucket(h) = I::new(0);
                *self.filter(h) = 0;
            }
        }
        self.drop_entries();
    }

    /// [`Self::clear_by`] that gives each key to `give` instead of dropping it: what a key holds
    /// serves the keys received next.
    pub fn clear_giving(&mut self, rehash: impl Fn(&K) -> u64, mut give: impl FnMut(K)) {
        let n = self.len();
        if n == 0 {
            return;
        }
        let (lo, hi, cap) = (self.lo_size, self.hi_start(), self.capacity());
        unsafe {
            if 3 + ((self.bucket_mask + 1) >> 5) <= n {
                ptr::write_bytes(self.buckets.as_ptr() as *mut u8, 0, (self.bucket_mask + 1) * (mem::size_of::<I>() + 1));
            } else {
                for i in (0..lo).chain(hi..cap) {
                    let h = rehash(self.key(i));
                    *self.bucket(h) = I::new(0);
                    *self.filter(h) = 0;
                }
            }
            // Reset the counters first: a panic in `give` or in a Drop cannot cause a double drop.
            self.lo_size = 0;
            self.hi_start = cap;
            for i in (0..lo).chain(hi..cap) {
                // SAFETY: slot `i` was in use and the counters above no longer own it: read once.
                give(ptr::read(self.keys.as_ptr().add(i)));
            }
            if mem::needs_drop::<V>() {
                drop_slots(self.vals, 0..lo);
                drop_slots(self.vals, hi..cap);
            }
        }
    }

    // lookup

    #[inline(always)]
    pub fn find(&self, h: u64, mut eq: impl FnMut(&K) -> bool) -> Option<usize> {
        unsafe {
            if *self.filter(h) & self.filter_bit(h) == 0 {
                return None;
            }
            // A positive filter guarantees a nonempty bucket.
            let mut i = (*self.bucket(h)).get() - 1;
            loop {
                if eq(self.key(i)) {
                    return Some(i);
                }
                if i >= self.lo_size {
                    return None;
                }
                i = self.link(i);
            }
        }
    }

    /// Looks up an entry before insertion, testing the bucket filter before comparing keys.
    #[inline(always)]
    pub fn find_for_insert(&self, h: u64, eq: impl FnMut(&K) -> bool) -> Option<usize> {
        self.find(h, eq)
    }

    #[inline(always)]
    pub fn get(&self, h: u64, eq: impl FnMut(&K) -> bool) -> Option<(&K, &V)> {
        match self.find(h, eq) {
            Some(i) => Some(unsafe { (self.key(i), self.val(i)) }),
            None => None,
        }
    }

    #[inline(always)]
    pub fn get_mut(&mut self, h: u64, eq: impl FnMut(&K) -> bool) -> Option<&mut V> {
        match self.find(h, eq) {
            Some(i) => Some(unsafe { self.val_mut(i) }),
            None => None,
        }
    }

    /// The value at an index `find` returned.
    ///
    /// # Safety
    ///
    /// `i` comes from `find` and the table has not been modified since.
    #[inline(always)]
    pub unsafe fn at_mut(&mut self, i: usize) -> &mut V {
        self.val_mut(i)
    }

    // insertion

    /// Inserts an entry known to be absent. Grows the table if needed.
    #[inline]
    pub fn insert_unique(&mut self, h: u64, key: K, value: V, rehash: impl Fn(&K) -> u64) -> &mut V {
        if self.lo_size == self.hi_start {
            self.grow(rehash);
        }
        unsafe {
            let i = self.place(h, key, value);
            self.val_mut(i)
        }
    }

    /// [`Self::insert_unique`] that gives the token of the new entry.
    #[inline]
    pub fn insert_unique_token(&mut self, h: u64, key: K, value: V, rehash: impl Fn(&K) -> u64) -> usize {
        if self.lo_size == self.hi_start {
            self.grow(rehash);
        }
        unsafe { self.place(h, key, value) }
    }

    /// Returns the value of the key `eq` matches, or inserts `make()`. `true` when the entry
    /// existed.
    #[inline]
    pub fn find_or_insert(
        &mut self,
        h: u64,
        eq: impl FnMut(&K) -> bool,
        make: impl FnOnce() -> (K, V),
        rehash: impl Fn(&K) -> u64,
    ) -> (bool, &mut V) {
        if let Some(i) = self.find(h, eq) {
            return (true, unsafe { self.val_mut(i) });
        }
        let (k, v) = make();
        (false, self.insert_unique(h, k, v, rehash))
    }

    /// Places an entry without a lookup and without a capacity check.
    #[inline(always)]
    unsafe fn place(&mut self, h: u64, key: K, value: V) -> usize {
        let bp = self.bucket(h);
        let head = (*bp).get();
        let dst = if head == 0 {
            // Empty bucket: hi region, no link.
            self.hi_start -= 1;
            self.hi_start
        } else {
            // Collision: lo region; the new entry becomes the chain head.
            let dst = self.lo_size;
            if dst == self.links_len {
                self.grow_links();
            }
            self.set_link(dst, head - 1);
            self.lo_size = dst + 1;
            dst
        };
        self.write(dst, key, value);
        *bp = I::new(dst + 1);
        *self.filter(h) |= self.filter_bit(h);
        dst
    }

    #[cold]
    #[inline(never)]
    fn grow_links(&mut self) {
        let n = (self.links_len * 2).max(MIN_LINKS).min(self.capacity());
        let l = Box::into_raw(I::zeroed(n)) as *mut I;
        unsafe {
            ptr::copy_nonoverlapping(self.links.as_ptr(), l, self.links_len);
            if self.links_len != parts::<K, V, I>(self.capacity()).links_len {
                free_links(self.links, self.links_len);
            }
            self.links = NonNull::new_unchecked(l);
        }
        self.links_len = n;
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, rehash: impl Fn(&K) -> u64) {
        let cap = self.capacity();
        self.resize(if cap == 0 { MIN_CAPACITY } else { Self::capacity_for(cap * 2) }, rehash);
    }

    /// Rebuilds the table for `new_cap` (a power of two, ≥ the number of entries).
    #[cold]
    #[inline(never)]
    fn resize(&mut self, new_cap: usize, rehash: impl Fn(&K) -> u64) {
        debug_assert!(new_cap.is_power_of_two() && new_cap >= self.len());
        let (old_lo, old_hi_start, old_cap) = (self.lo_size, self.hi_start, self.capacity());
        let old = self.take_memory();
        self.install(new_cap);

        // Entries are moved out bitwise; after the loop the old memory is freed without dropping
        // their contents.
        // two loops: a chain stepped by `for` costs a branch an entry
        unsafe {
            for i in 0..old_lo {
                let k = ptr::read(old.keys.as_ptr().add(i));
                let v = ptr::read(old.vals.as_ptr().add(i));
                self.place(rehash(&k), k, v);
            }
            for i in old_hi_start..old_cap {
                let k = ptr::read(old.keys.as_ptr().add(i));
                let v = ptr::read(old.vals.as_ptr().add(i));
                self.place(rehash(&k), k, v);
            }
            old.free();
        }
    }

    pub fn reserve(&mut self, additional: usize, rehash: impl Fn(&K) -> u64) {
        let need = self.len() + additional;
        if need > self.capacity() {
            self.resize(Self::capacity_for(need), rehash);
        }
    }

    pub fn shrink_to_fit(&mut self, rehash: impl Fn(&K) -> u64) {
        let count = self.len();
        if count == 0 {
            unsafe { self.take_memory().free() };
            return;
        }
        let cap = Self::capacity_for(count);
        if cap < self.capacity() {
            self.resize(cap, rehash);
        }
    }

    // removal

    /// Moves entry `src` into free slot `dst` and redirects its incoming bucket or chain link.
    #[inline]
    unsafe fn relocate(&mut self, src: usize, dst: usize, rehash: &impl Fn(&K) -> u64) {
        let (k, v) = self.read(src);
        let bp = self.bucket(rehash(&k));
        let head = (*bp).get() - 1;
        if head == src {
            *bp = I::new(dst + 1);
        } else {
            let mut j = head;
            loop {
                let l = &mut *self.links.as_ptr().add(j);
                if l.get() == src {
                    *l = I::new(dst);
                    break;
                }
                j = l.get();
            }
        }
        if src < self.lo_size {
            self.set_link(dst, self.link(src));
        }
        self.write(dst, k, v);
    }

    /// Handles filtered misses and single-entry buckets inline; delegates collision-chain removal
    /// to [`remove_chain`](Self::remove_chain).
    #[inline]
    pub fn remove(&mut self, h: u64, mut eq: impl FnMut(&K) -> bool, rehash: impl Fn(&K) -> u64) -> Option<(K, V)> {
        unsafe {
            let fp = self.filter(h);
            if *fp & self.filter_bit(h) == 0 {
                return None;
            }
            let bp = self.bucket(h);
            let i = (*bp).get() - 1;
            if i < self.lo_size {
                return self.remove_chain(h, eq, rehash);
            }
            // Chain of a single hi entry.
            if !eq(self.key(i)) {
                return None;
            }
            let val = self.read(i);
            *bp = I::new(0);
            *fp = 0;
            // Fill the hole with the lowest entry of the hi region.
            let src = self.hi_start;
            self.hi_start += 1;
            if src != i {
                self.relocate(src, i, &rehash);
            }
            Some(val)
        }
    }

    /// [`remove`](Self::remove) of a chain that begins in lo.
    #[inline(never)]
    fn remove_chain(&mut self, h: u64, mut eq: impl FnMut(&K) -> bool, rehash: impl Fn(&K) -> u64) -> Option<(K, V)> {
        unsafe {
            let bp = self.bucket(h);
            let mut i = (*bp).get() - 1;
            let lo = self.lo_size;

            let (mut prev, mut prev_prev) = (NONE, NONE);
            loop {
                if eq(self.key(i)) {
                    break;
                }
                if i >= lo {
                    return None;
                }
                prev_prev = prev;
                prev = i;
                i = self.link(i);
            }

            let last_lo = lo - 1;
            let val = self.read(i);
            if i < lo {
                // Found in lo: unlink it from the chain and fill the hole with the last lo entry.
                let next = self.link(i);
                if prev == NONE {
                    *bp = I::new(next + 1);
                } else {
                    self.set_link(prev, next);
                }
                if last_lo != i {
                    self.relocate(last_lo, i, &rehash);
                }
            } else {
                // Found at the hi tail of a chain of length ≥ 2: prev (lo) moves into the hi slot
                // and becomes the new tail; the freed lo slot is filled with last_lo.
                let (pk, pv) = self.read(prev);
                self.write(i, pk, pv);
                if prev_prev == NONE {
                    *bp = I::new(i + 1);
                } else {
                    self.set_link(prev_prev, i);
                }
                if last_lo != prev {
                    self.relocate(last_lo, prev, &rehash);
                }
            }
            self.lo_size = last_lo;
            Some(val)
        }
    }

    /// Keeps only the entries for which `f` returns `true`. Rebuilds the table in place of its
    /// capacity: O(capacity), no per-entry lookups.
    pub fn retain(&mut self, mut f: impl FnMut(&K, &mut V) -> bool, rehash: impl Fn(&K) -> u64) {
        if self.len() == 0 {
            return;
        }
        let (old_lo, old_hi_start, cap) = (self.lo_size, self.hi_start, self.capacity());
        let old = self.take_memory();
        self.install(cap);
        // A panic in `f` or in a Drop leaks the entries not yet visited and the old memory, never
        // drops one twice.
        unsafe {
            for i in 0..old_lo {
                let k = ptr::read(old.keys.as_ptr().add(i));
                let mut v = ptr::read(old.vals.as_ptr().add(i));
                if f(&k, &mut v) {
                    self.place(rehash(&k), k, v);
                }
            }
            for i in old_hi_start..cap {
                let k = ptr::read(old.keys.as_ptr().add(i));
                let mut v = ptr::read(old.vals.as_ptr().add(i));
                if f(&k, &mut v) {
                    self.place(rehash(&k), k, v);
                }
            }
            old.free();
        }
    }

    // iteration

    /// Order: `[0, lo_size)`, then `[hi_start, cap)`.
    #[inline]
    pub fn iter(&self) -> RawIter<'_, K, V> {
        let cap = self.capacity();
        RawIter {
            keys: unsafe { &*ptr::slice_from_raw_parts(self.keys.as_ptr() as *const MaybeUninit<K>, cap) },
            vals: unsafe { &*ptr::slice_from_raw_parts(self.vals.as_ptr() as *const MaybeUninit<V>, cap) },
            cur: 0,
            lo_end: self.lo_size,
            hi_start: self.hi_start(),
            remaining: self.len(),
        }
    }

    #[inline]
    pub fn iter_mut(&mut self) -> RawIterMut<'_, K, V> {
        let (lo_end, hi_start, remaining) = (self.lo_size, self.hi_start(), self.len());
        RawIterMut {
            keys: self.keys.as_ptr() as *const K,
            vals: self.vals.as_ptr(),
            cur: 0,
            lo_end,
            hi_start,
            remaining,
            _marker: PhantomData,
        }
    }

    /// Moves the entries out; the table is left empty without memory.
    pub fn into_iter(mut self) -> RawIntoIter<K, V> {
        let (lo_end, hi_start, remaining) = (self.lo_size, self.hi_start(), self.len());
        // the drop of `self` then frees nothing
        let memory = self.take_memory();
        RawIntoIter { memory, cur: 0, lo_end, hi_start, remaining }
    }
}

impl<K, V, I: RawIndex, const HASH_BITS: u32> Drop for RawTable<K, V, I, HASH_BITS> {
    fn drop(&mut self) {
        self.drop_entries();
        unsafe { self.take_memory().free() };
    }
}

impl<K: Clone, V: Clone, I: RawIndex, const HASH_BITS: u32> Clone for RawTable<K, V, I, HASH_BITS> {
    /// Same capacity and the same layout: no rehashing.
    fn clone(&self) -> Self {
        let cap = self.capacity();
        let mut t = Self::new();
        if self.has_buckets() {
            t.install(cap);
            while t.links_len < self.lo_size {
                t.grow_links();
            }
            unsafe {
                // the filters right after the buckets (`Parts`)
                ptr::copy_nonoverlapping(self.buckets.as_ptr() as *const u8, t.buckets.as_ptr() as *mut u8, (self.bucket_mask + 1) * (mem::size_of::<I>() + 1));
                ptr::copy_nonoverlapping(self.links.as_ptr(), t.links.as_ptr(), self.lo_size);
            }
        }
        // The counters grow as entries are cloned: a panic in a clone cannot cause uninitialized
        // slots to be dropped.
        unsafe {
            for i in 0..self.lo_size {
                let (k, v) = (self.key(i).clone(), self.val(i).clone());
                t.write(i, k, v);
                t.lo_size += 1;
            }
            for i in (self.hi_start()..cap).rev() {
                let (k, v) = (self.key(i).clone(), self.val(i).clone());
                t.write(i, k, v);
                t.hi_start = i;
            }
        }
        t
    }
}

// iterators

pub(crate) struct RawIter<'a, K, V> {
    keys: &'a [MaybeUninit<K>],
    vals: &'a [MaybeUninit<V>],
    cur: usize,
    lo_end: usize,
    hi_start: usize,
    remaining: usize,
}

/// `range` of slots known to be initialized, as a slice of `T`.
#[inline(always)]
unsafe fn inited<T>(s: &[MaybeUninit<T>], range: core::ops::Range<usize>) -> &[T] {
    &*(s.get_unchecked(range) as *const [MaybeUninit<T>] as *const [T])
}

impl<'a, K, V> Iterator for RawIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        if self.cur == self.lo_end {
            self.cur = self.hi_start; // the lo → hi bridge fires once
        }
        let e = unsafe {
            (self.keys.get_unchecked(self.cur).assume_init_ref(), self.vals.get_unchecked(self.cur).assume_init_ref())
        };
        self.cur += 1;
        self.remaining -= 1;
        Some(e)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }

    /// Two tight loops without the bridge branch; `for_each`, `sum`, `collect` etc. go through it.
    #[inline]
    fn fold<B, F: FnMut(B, Self::Item) -> B>(self, init: B, mut f: F) -> B {
        let mut acc = init;
        if self.remaining == 0 {
            return acc;
        }
        // State: either cur ∈ [0, lo_end) — still in lo — or already in hi (cur == lo_end or ≥
        // hi_start).
        let (lo, hi_from) = if self.cur < self.lo_end {
            (self.cur..self.lo_end, self.hi_start)
        } else {
            (0..0, self.cur.max(self.hi_start))
        };
        let hi = hi_from..hi_from + (self.remaining - lo.len());
        unsafe {
            for (k, v) in inited(self.keys, lo.clone()).iter().zip(inited(self.vals, lo)) {
                acc = f(acc, (k, v));
            }
            for (k, v) in inited(self.keys, hi.clone()).iter().zip(inited(self.vals, hi)) {
                acc = f(acc, (k, v));
            }
        }
        acc
    }
}

impl<K, V> ExactSizeIterator for RawIter<'_, K, V> {}
impl<K, V> FusedIterator for RawIter<'_, K, V> {}

impl<K, V> Clone for RawIter<'_, K, V> {
    fn clone(&self) -> Self {
        Self { ..*self }
    }
}

pub(crate) struct RawIterMut<'a, K, V> {
    keys: *const K,
    vals: *mut V,
    cur: usize,
    lo_end: usize,
    hi_start: usize,
    remaining: usize,
    _marker: PhantomData<(&'a K, &'a mut V)>,
}

unsafe impl<K: Sync, V: Send> Send for RawIterMut<'_, K, V> {}
unsafe impl<K: Sync, V: Sync> Sync for RawIterMut<'_, K, V> {}

impl<'a, K, V> Iterator for RawIterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        if self.cur == self.lo_end {
            self.cur = self.hi_start;
        }
        // SAFETY: each slot is yielded at most once.
        let e = unsafe { (&*self.keys.add(self.cur), &mut *self.vals.add(self.cur)) };
        self.cur += 1;
        self.remaining -= 1;
        Some(e)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> ExactSizeIterator for RawIterMut<'_, K, V> {}
impl<K, V> FusedIterator for RawIterMut<'_, K, V> {}

pub(crate) struct RawIntoIter<K, V> {
    memory: Memory<K, V>,
    cur: usize,
    lo_end: usize,
    hi_start: usize,
    remaining: usize,
}

impl<K, V> Iterator for RawIntoIter<K, V> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<(K, V)> {
        if self.remaining == 0 {
            return None;
        }
        if self.cur == self.lo_end {
            self.cur = self.hi_start;
        }
        let e = unsafe { (ptr::read(self.memory.keys.as_ptr().add(self.cur)), ptr::read(self.memory.vals.as_ptr().add(self.cur))) };
        self.cur += 1;
        self.remaining -= 1;
        Some(e)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> ExactSizeIterator for RawIntoIter<K, V> {}
impl<K, V> FusedIterator for RawIntoIter<K, V> {}

// `NonNull` disables auto Send/Sync; the iterator owns the entries left like the table did.
unsafe impl<K: Send, V: Send> Send for RawIntoIter<K, V> {}
unsafe impl<K: Sync, V: Sync> Sync for RawIntoIter<K, V> {}

impl<K, V> Drop for RawIntoIter<K, V> {
    fn drop(&mut self) {
        if mem::needs_drop::<K>() || mem::needs_drop::<V>() {
            for e in self.by_ref() {
                drop(e);
            }
        }
        unsafe { self.memory.free() };
    }
}

#[cfg(test)]
#[path = "lohi_filter_tests.rs"]
mod filter_tests;
