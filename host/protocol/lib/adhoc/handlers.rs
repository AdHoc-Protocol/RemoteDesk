// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Connection event handlers and atomically replaceable handler objects.
//!
//! [`Handlers`] stores `Copy` entries, typically function pointers, and dispatches them under a
//! read lock - an empty list (the common case: an event nobody listens to) with one plain load, no
//! lock. Callbacks must not change that same list during dispatch. [`Handler`] publishes an
//! `Arc`-owned object while keeping replaced objects alive for outstanding readers.

use parking_lot::RwLock;

pub struct Handlers<F: Copy> {
    list: RwLock<Vec<F>>,
    /// The handlers in `list`, written under its write lock: an event of none takes no lock - the
    /// lock is an atomic write to a line every thread shares.
    n: std::sync::atomic::AtomicUsize,
}

impl<F: Copy> Handlers<F> {
    pub const fn new() -> Self {
        Self { list: parking_lot::RwLock::new(Vec::new()), n: std::sync::atomic::AtomicUsize::new(0) }
    }

    pub fn add(&self, f: F) {
        let mut l = self.list.write();
        l.push(f);
        self.n.store(l.len(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Removes the first handler equal to `f`: true — it was there.
    pub fn remove(&self, f: F) -> bool
    where
        F: PartialEq,
    {
        let mut l = self.list.write();
        match l.iter().position(|x| *x == f) {
            Some(i) => {
                l.remove(i);
                self.n.store(l.len(), std::sync::atomic::Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n.load(std::sync::atomic::Ordering::Relaxed) == 0
    }

    /// Calls `call` with each handler, in the order they were added. A handler must not add or
    /// remove handlers of the same event: the list is read-locked meanwhile.
    #[inline]
    pub fn each(&self, mut call: impl FnMut(F)) {
        // a handler added on another thread meanwhile hears the next event: the order of the two is
        // not given anyway
        if self.n.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            return;
        }
        for &f in self.list.read().iter() {
            call(f);
        }
    }
}

impl<F: Copy> Default for Handlers<F> {
    fn default() -> Self {
        Self::new()
    }
}

/// Atomically replaceable handler shared by connection users.
///
/// Reads borrow the current handler without locking or cloning its `Arc`. Replaced handlers remain
/// owned by the cell until it is dropped, keeping outstanding borrows valid. Setting the same
/// object again does not retain another copy.
pub struct Handler<H: ?Sized> {
    /// The handler now: null — none.
    cur: std::sync::atomic::AtomicPtr<std::sync::Arc<H>>,
    /// Every handler set: they go with the cell. Writers take this lock.
    kept: parking_lot::Mutex<Vec<Box<std::sync::Arc<H>>>>,
}

impl<H: ?Sized> Handler<H> {
    pub const fn new() -> Self {
        Self { cur: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()), kept: parking_lot::Mutex::new(Vec::new()) }
    }

    /// Sets `h` (`None` — no handler) and returns the one it replaces: a handler that wraps the one
    /// before keeps it and passes the events on.
    pub fn exchange(&self, h: Option<std::sync::Arc<H>>) -> Option<std::sync::Arc<H>> {
        let mut kept = self.kept.lock();
        let old = self.cur.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: what `cur` points at is kept in `kept`
        let before = (!old.is_null()).then(|| unsafe { &*old }.clone());
        let p = match h {
            Some(h) => match kept.iter_mut().find(|k| std::ptr::addr_eq(std::sync::Arc::as_ptr(k), std::sync::Arc::as_ptr(&h))) {
                Some(k) => &mut **k as *mut std::sync::Arc<H>, // set before: kept already
                None => {
                    let mut b = Box::new(h);
                    let p: *mut std::sync::Arc<H> = &mut *b;
                    kept.push(b);
                    p
                }
            },
            None => std::ptr::null_mut(),
        };
        self.cur.store(p, std::sync::atomic::Ordering::Release);
        before
    }

    /// The handler now, if any.
    #[inline(always)]
    pub fn get(&self) -> Option<&H> {
        let p = self.cur.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: what `cur` points at is kept in `kept` as long as the cell is
        (!p.is_null()).then(|| &**unsafe { &*p })
    }

    /// The handler now, if any, shared: for one that outlives the read (a count of references
    /// more).
    #[inline]
    pub fn get_arc(&self) -> Option<std::sync::Arc<H>> {
        let p = self.cur.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: as `get`
        (!p.is_null()).then(|| unsafe { &*p }.clone())
    }
}

impl<H: ?Sized> Default for Handler<H> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Say: Send + Sync {
        fn say(&self) -> u32;
    }
    struct N(u32);
    impl Say for N {
        fn say(&self) -> u32 {
            self.0
        }
    }

    /// `exchange` gives back the handler it replaces (a wrapper keeps it and passes the events on);
    /// a handler read before a replace stays good.
    #[test]
    fn a_handler_is_exchanged() {
        let h: Handler<dyn Say> = Handler::new();
        assert!(h.get().is_none());
        assert!(h.exchange(Some(std::sync::Arc::new(N(1)))).is_none());
        let first = h.get().unwrap();
        let before = h.exchange(Some(std::sync::Arc::new(N(2)))).unwrap();
        assert_eq!((first.say(), before.say(), h.get().unwrap().say()), (1, 1, 2));
        assert_eq!(h.exchange(None).unwrap().say(), 2);
        assert!(h.get().is_none());
    }

    /// An object set again is kept once: a cell that goes back and forth between two keeps two.
    #[test]
    fn what_is_set_again_is_kept_once() {
        let h: Handler<dyn Say> = Handler::new();
        let (a, b): (std::sync::Arc<dyn Say>, std::sync::Arc<dyn Say>) = (std::sync::Arc::new(N(1)), std::sync::Arc::new(N(2)));
        for _ in 0..100 {
            h.exchange(Some(a.clone()));
            h.exchange(Some(b.clone()));
        }
        assert_eq!(h.kept.lock().len(), 2);
        assert_eq!(h.get_arc().unwrap().say(), 2);
        assert_eq!(std::sync::Arc::strong_count(&a), 2, "the one kept and this");
    }
}
