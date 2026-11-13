// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Reentrant spin lock with process-unique thread ownership tags.
//!
//! The owning thread may acquire the lock recursively. Other threads use compare-and-swap and spin
//! until ownership is released. The connection [`group`](crate::lib::adhoc::connection::group)
//! lists use this lock to allow membership changes during callbacks.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// FastReentrantSpinLock

/// Spin lock with recursive acquisition by its owning thread.
///
/// Ownership uses a process-unique thread-local tag. Each acquisition increments the depth; the
/// matching outermost release makes the lock available to other threads.
pub struct FastReentrantSpinLock {
    owner: AtomicU64, // 0 = free, else the owning thread's tag
    locks: AtomicU32, // reentrancy depth; touched by the owner only
}

impl Default for FastReentrantSpinLock {
    fn default() -> Self {
        Self::new()
    }
}

static NEXT_THREAD_TAG: AtomicU64 = AtomicU64::new(1);

fn current_thread_tag() -> u64 {
    thread_local! {
        static TAG: u64 = NEXT_THREAD_TAG.fetch_add(1, Ordering::Relaxed);
    }
    TAG.with(|t| *t)
}

impl FastReentrantSpinLock {
    pub fn new() -> Self {
        Self {
            owner: AtomicU64::new(0),
            locks: AtomicU32::new(0),
        }
    }

    /// Acquires the lock, spinning until it is held by the calling thread. Re-acquisition by the
    /// current owner just bumps the reentrancy count.
    pub fn acquire(&self) {
        let th = current_thread_tag();
        if self.owner.load(Ordering::Acquire) == th {
            self.locks.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.owner.compare_exchange(0, th, Ordering::Acquire, Ordering::Relaxed).is_err() {
            self.contended(th);
        }
        self.locks.store(1, Ordering::Relaxed);
    }

    /// The lock is held: a holder keeps it for a few instructions, so the wait is spun - read
    /// before tried, with a pause that doubles between the tries, so a release is not rushed by
    /// every waiter at once - and then yields the processor: a holder the scheduler took off it
    /// gets it back.
    #[cold]
    fn contended(&self, th: u64) {
        let mut pause = 1u32;
        loop {
            if pause <= 64 {
                for _ in 0..pause {
                    std::hint::spin_loop();
                }
                pause <<= 1;
            } else {
                std::thread::yield_now();
            }
            if self.owner.load(Ordering::Relaxed) == 0 && self.owner.compare_exchange_weak(0, th, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return;
            }
        }
    }

    /// Releases one level of the lock held by the calling thread; the lock is freed when the
    /// outermost release matches the first acquire. A non-owner releases nothing.
    pub fn release(&self) {
        if self.owner.load(Ordering::Acquire) != current_thread_tag() {
            return;
        }
        if 1 < self.locks.fetch_sub(1, Ordering::Relaxed) {
            return; // still held by an outer acquire
        }
        self.owner.store(0, Ordering::Release);
    }

    /// Acquire and return an RAII guard.
    pub fn lock(&self) -> SpinGuard<'_> {
        self.acquire();
        SpinGuard(self, PhantomData)
    }
}

/// RAII guard of [`FastReentrantSpinLock::lock`], released on its acquiring thread.
/// The guard cannot be sent or shared between threads.
pub struct SpinGuard<'a>(&'a FastReentrantSpinLock, PhantomData<*mut ()>);

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_cannot_move_to_another_thread() {
        trait AmbiguousIfSend<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
        // Type inference becomes ambiguous if SpinGuard implements Send.
        let _ = <SpinGuard<'_> as AmbiguousIfSend<_>>::check;
    }

    #[test]
    fn unwinding_releases_every_reentrant_guard() {
        let lock = FastReentrantSpinLock::new();
        let result = std::panic::catch_unwind(|| {
            let _outer = lock.lock();
            let _inner = lock.lock();
            panic!("leave both lock scopes");
        });
        assert!(result.is_err());
        assert_eq!(lock.owner.load(Ordering::Acquire), 0);
        assert_eq!(lock.locks.load(Ordering::Relaxed), 0);
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = lock.lock();
                })
                .join()
                .unwrap();
        });
    }
}
