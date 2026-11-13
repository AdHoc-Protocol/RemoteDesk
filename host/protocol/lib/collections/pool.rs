//! Object reuse through thread-local magazines and a shared depot.
//!
//! Each thread caches two magazines of up to 32 objects. Local take and release avoid atomics; the
//! depot exchanges whole magazines between threads. An empty pool uses the factory and a full cache
//! drops excess objects. No FIFO ordering is provided.
//!
//! Thread slots are reclaimed when threads exit and may be reused with their cached objects. Beyond
//! [`THREADS`] concurrent slots, callers use the factory and drop returned objects. Pooling retains
//! allocations owned by values such as `Vec<u8>`. [`Pool::get`] returns a guard that releases its
//! object on drop.

use core::cell::UnsafeCell;
use core::fmt;
use core::mem::{self, ManuallyDrop};
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Threads with magazines of their own in every pool at once.
pub const THREADS: usize = 256;
/// Objects a magazine holds at most.
const M: usize = 32;

/// This thread's place in the pools: given at its first use of one, given back when the thread
/// ends. `None` past [`THREADS`] threads at once, or while the thread ends.
#[inline]
fn place() -> Option<usize> {
    struct Place(usize);
    static FREE: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    impl Drop for Place {
        fn drop(&mut self) {
            if self.0 < THREADS {
                if let Ok(mut free) = FREE.lock() {
                    free.push(self.0);
                }
            }
        }
    }
    thread_local!(static PLACE: Place = Place(FREE.lock().ok().and_then(|mut f| f.pop()).unwrap_or_else(|| NEXT.fetch_add(1, Ordering::Relaxed))));
    PLACE.try_with(|p| p.0).ok().filter(|&p| p < THREADS)
}

/// A thread's two magazines: the loaded one, and the one before - full or empty: a thread taking
/// and giving back in turn at the edge of a magazine does not go to the depot each time. A cache line
/// of its own: 48 bytes in 64, not 128 - two threads side by side were no slower, the room of the
/// places of a pool is half.
#[repr(align(64))]
struct Local<T> {
    loaded: UnsafeCell<Vec<T>>,
    prev: UnsafeCell<Vec<T>>,
}

pub struct Pool<T, F = fn() -> T> {
    /// The magazines of the threads, by their places.
    locals: Box<[Local<T>]>,
    /// The depot: full magazines, and empty ones - a place holds a boxed magazine or null.
    full: Box<[AtomicPtr<Vec<T>>]>,
    empty: Box<[AtomicPtr<Vec<T>>]>,
    /// Objects a magazine holds.
    m: usize,
    factory: F,
}

// SAFETY: the magazines of a place are touched by the thread that holds it alone; the depot by
// atomic swaps.
unsafe impl<T: Send, F: Send> Send for Pool<T, F> {}
unsafe impl<T: Send, F: Sync> Sync for Pool<T, F> {}

impl<T, F: Fn() -> T> Pool<T, F> {
    /// Creates a pool with room for at most `2^capacity_power_of_two` objects in its shared depot,
    /// plus two magazines per thread.
    ///
    /// # Panics
    ///
    /// Panics unless `capacity_power_of_two` is in `1..=30`.
    pub fn new(capacity_power_of_two: u32, factory: F) -> Self {
        assert!((1..=30).contains(&capacity_power_of_two), "capacity_power_of_two must be 1..=30");
        let capacity = 1usize << capacity_power_of_two;
        let m = capacity.min(M);
        let places = || (0..capacity / m).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
        Self {
            locals: (0..THREADS)
                .map(|_| Local {
                    loaded: UnsafeCell::new(Vec::new()),
                    prev: UnsafeCell::new(Vec::new()),
                })
                .collect(),
            full: places(),
            empty: places(),
            m,
            factory,
        }
    }

    /// The magazines of place `p`.
    #[inline(always)]
    fn local(&self, p: usize) -> &Local<T> {
        // SAFETY: `place` gives less than `THREADS`, the length of `locals`
        unsafe { self.locals.get_unchecked(p) }
    }

    /// An object of this thread's magazines; else of a full magazine of the depot; else a new one.
    #[inline]
    pub fn acquire(&self) -> T {
        if let Some(p) = place() {
            // SAFETY: the magazines of a place are its thread's alone
            if let Some(item) = unsafe { &mut *self.local(p).loaded.get() }.pop() {
                return item;
            }
            if let Some(item) = self.acquire_depot(p) {
                return item;
            }
        }
        (self.factory)()
    }

    /// The loaded magazine is empty: the one before, if it holds any; else a full one of the depot
    /// for it, the empty one to the depot.
    #[cold]
    fn acquire_depot(&self, p: usize) -> Option<T> {
        // SAFETY: as in `acquire`
        let (loaded, prev) = unsafe { (&mut *self.local(p).loaded.get(), &mut *self.local(p).prev.get()) };
        if !prev.is_empty() {
            mem::swap(loaded, prev);
            return loaded.pop();
        }
        let mut full = take(&self.full)?;
        mem::swap(loaded, &mut *full); // `full` holds the empty one now
        if let Err(e) = give(&self.empty, full) {
            drop(e);
        }
        loaded.pop()
    }

    /// Gives `item` back: into this thread's magazines; a full pool drops it.
    #[inline]
    pub fn release(&self, item: T) {
        let Some(p) = place() else { return drop(item) };
        // SAFETY: as in `acquire`
        let loaded = unsafe { &mut *self.local(p).loaded.get() };
        let len = loaded.len();
        // below `m` and at its capacity: the magazine is not allocated yet - the cold path does it,
        // no growth of a `Vec` on this one
        if len < self.m && len < loaded.capacity() {
            // SAFETY: `len` is below the capacity
            unsafe {
                loaded.as_mut_ptr().add(len).write(item);
                loaded.set_len(len + 1);
            }
        } else {
            self.release_depot(p, item);
        }
    }

    /// The loaded magazine is full: the one before, if it has room; else the one before - full - to
    /// the depot and an empty one in its place.
    #[cold]
    fn release_depot(&self, p: usize, item: T) {
        // SAFETY: as in `acquire`
        let (loaded, prev) = unsafe { (&mut *self.local(p).loaded.get(), &mut *self.local(p).prev.get()) };
        if loaded.len() < self.m {
            // not allocated yet
            loaded.reserve_exact(self.m);
            return loaded.push(item);
        }
        if prev.len() < self.m {
            mem::swap(loaded, prev);
            if loaded.capacity() == 0 {
                loaded.reserve_exact(self.m);
            }
            return loaded.push(item);
        }
        let mut mag = take(&self.empty).unwrap_or_else(|| Box::new(Vec::with_capacity(self.m)));
        mem::swap(prev, &mut *mag); // `mag` holds the full one now, `prev` an empty one
        match give(&self.full, mag) {
            Ok(()) => {
                mem::swap(loaded, prev);
                loaded.push(item);
            }
            Err(mut back) => {
                // the depot is full: the magazine back as it was, the object dropped
                mem::swap(prev, &mut *back);
                drop(give(&self.empty, back));
                drop(item);
            }
        }
    }

    /// [`acquire`](Self::acquire) behind a guard that releases the object when dropped.
    #[inline]
    pub fn get(&self) -> Pooled<'_, T, F> {
        Pooled {
            item: ManuallyDrop::new(self.acquire()),
            pool: self,
        }
    }
}

/// A magazine of `places`, its place left null; `None` - none there.
fn take<T>(places: &[AtomicPtr<Vec<T>>]) -> Option<Box<Vec<T>>> {
    for p in places {
        if !p.load(Ordering::Relaxed).is_null() {
            let m = p.swap(ptr::null_mut(), Ordering::Acquire);
            if !m.is_null() {
                // SAFETY: put by `give` from a box, taken out once by the swap
                return Some(unsafe { Box::from_raw(m) });
            }
        }
    }
    None
}

/// `mag` into a null place of `places`; `Err(mag)` - none there.
fn give<T>(places: &[AtomicPtr<Vec<T>>], mag: Box<Vec<T>>) -> Result<(), Box<Vec<T>>> {
    let raw = Box::into_raw(mag);
    for p in places {
        if p.load(Ordering::Relaxed).is_null() && p.compare_exchange(ptr::null_mut(), raw, Ordering::Release, Ordering::Relaxed).is_ok() {
            return Ok(());
        }
    }
    // SAFETY: not put anywhere: still this call's
    Err(unsafe { Box::from_raw(raw) })
}

impl<T, F> Pool<T, F> {
    /// Idle objects the depot holds at most; each thread holds two magazines besides.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.full.len() * self.m
    }

    /// Idle objects in the depot now (its magazines are full), a snapshot; the threads' magazines
    /// are not counted.
    pub fn len(&self) -> usize {
        self.full.iter().filter(|p| !p.load(Ordering::Relaxed).is_null()).count() * self.m
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T, F> Drop for Pool<T, F> {
    fn drop(&mut self) {
        for p in self.full.iter().chain(self.empty.iter()) {
            let m = p.swap(ptr::null_mut(), Ordering::Acquire);
            if !m.is_null() {
                // SAFETY: as in `take`
                drop(unsafe { Box::from_raw(m) });
            }
        }
    }
}

impl<T, F> fmt::Debug for Pool<T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool").field("capacity", &self.capacity()).field("len", &self.len()).finish()
    }
}

/// An object from a [`Pool`]; released back when dropped.
pub struct Pooled<'a, T, F: Fn() -> T> {
    item: ManuallyDrop<T>,
    pool: &'a Pool<T, F>,
}

impl<T, F: Fn() -> T> Pooled<'_, T, F> {
    /// Keeps the object: it does not go back to the pool.
    pub fn into_inner(self) -> T {
        let mut this = ManuallyDrop::new(self);
        // SAFETY: `this` is never dropped, so `item` is taken exactly once.
        unsafe { ManuallyDrop::take(&mut this.item) }
    }
}

impl<T, F: Fn() -> T> Deref for Pooled<'_, T, F> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.item
    }
}

impl<T, F: Fn() -> T> DerefMut for Pooled<'_, T, F> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.item
    }
}

impl<T, F: Fn() -> T> Drop for Pooled<'_, T, F> {
    fn drop(&mut self) {
        // SAFETY: `item` is taken once, here; the guard is gone afterwards.
        self.pool.release(unsafe { ManuallyDrop::take(&mut self.item) });
    }
}

impl<T: fmt::Debug, F: Fn() -> T> fmt::Debug for Pooled<'_, T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.item, f)
    }
}
