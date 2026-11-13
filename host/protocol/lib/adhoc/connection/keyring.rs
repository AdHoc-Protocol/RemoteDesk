// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Session encryption keys retained across transport reconnections.
//!
//! Each handshake contributes a [`Key`]. New packets use the newest key; older keys remain
//! available to decrypt packets still in flight. Readers access an immutable key list without
//! taking the update lock. [`KeyGuard`] keeps a selected snapshot alive while it is read.
//! Replaced snapshots are reclaimed after their last reader releases them.

use arc_swap::{ArcSwap, Guard};
use parking_lot::Mutex;
use std::ops::Deref;
use std::sync::Arc;

/// Packet encryption key: a 32-byte ChaCha20 key, 12-byte base nonce, and an identifier carried
/// unencrypted in the packet header.
#[derive(Clone)]
pub struct Key {
    pub id: u32,
    pub key: [u8; 32],
    pub iv: [u8; 12],
}

impl Key {
    pub fn new(id: u32, key: [u8; 32], iv: [u8; 12]) -> Self {
        Self { id, key, iv }
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key").field("id", &self.id).finish_non_exhaustive()
    }
}

/// A selected key and the immutable snapshot that owns it.
///
/// Dereferences to [`Key`]. The guard remains valid across updates, clearing, and dropping the
/// keyring. Keeping a guard also keeps the other keys in its snapshot alive.
pub struct KeyGuard {
    snapshot: Guard<Arc<Vec<Key>>>,
    index: usize,
}

impl Deref for KeyGuard {
    type Target = Key;

    #[inline]
    fn deref(&self) -> &Key {
        &self.snapshot[self.index]
    }
}

impl AsRef<Key> for KeyGuard {
    #[inline]
    fn as_ref(&self) -> &Key {
        self
    }
}

impl std::fmt::Debug for KeyGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}

/// The keys of one session, published as immutable snapshots.
pub struct Keyring {
    keys: ArcSwap<Vec<Key>>,
    writers: Mutex<()>,
}

impl Default for Keyring {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyring {
    pub fn new() -> Self {
        Self {
            keys: ArcSwap::from_pointee(Vec::new()),
            writers: Mutex::new(()),
        }
    }

    /// Borrows the newest key through a snapshot guard, or returns `None` for an empty ring.
    /// Keep the guard while using references to its key.
    #[inline]
    pub fn current(&self) -> Option<KeyGuard> {
        let snapshot = self.keys.load();
        let index = snapshot.len().checked_sub(1)?;
        Some(KeyGuard { snapshot, index })
    }

    /// The `n`-th key from the newest ( 0 - the newest ), or `None` past the oldest.
    #[inline]
    pub fn newest(&self, n: usize) -> Option<KeyGuard> {
        let snapshot = self.keys.load();
        let index = snapshot.len().checked_sub(1 + n)?;
        Some(KeyGuard { snapshot, index })
    }

    /// Returns the number of keys in the currently published list.
    #[inline]
    pub fn count(&self) -> usize {
        self.keys.load().len()
    }

    /// Finds a key by identifier, keeping its snapshot alive through the returned guard.
    #[inline]
    pub fn find(&self, id: u32) -> Option<KeyGuard> {
        let snapshot = self.keys.load();
        let index = snapshot.iter().rposition(|key| key.id == id)?;
        Some(KeyGuard { snapshot, index })
    }

    /// Adds a key as the newest entry, removing an older entry with the same identifier. If the
    /// newest identifier already matches, leaves the list unchanged.
    pub fn add(&self, key: Key) {
        let _writer = self.writers.lock();
        let old = self.keys.load();
        if old.last().is_some_and(|k| k.id == key.id) {
            return;
        }
        let mut list = Vec::with_capacity(old.len() + 1);
        list.extend(old.iter().filter(|k| k.id != key.id).cloned());
        list.push(key);
        self.keys.store(Arc::new(list));
    }

    /// The keys of `older` - the connections a session had before it moved here (a reclaim, a
    /// reconnect) - go ahead of these: a pack still in flight under one of them - a guaranteed one
    /// the journal replays - is read, new packs are encrypted with the newest of these.
    pub fn adopt(&self, older: &Keyring) {
        if std::ptr::eq(self, older) {
            return;
        }
        let theirs = older.keys.load();
        if theirs.is_empty() {
            return;
        }
        let _writer = self.writers.lock();
        let mine = self.keys.load();
        let mut list = Vec::with_capacity(theirs.len() + mine.len());
        list.extend(theirs.iter().filter(|k| !mine.iter().any(|m| m.id == k.id)).cloned());
        list.extend(mine.iter().cloned());
        self.keys.store(Arc::new(list));
    }

    /// Publishes an empty key list. Older snapshots remain alive only while readers hold them.
    /// Released key material is deallocated without an explicit overwrite.
    pub fn clear(&self) {
        let _writer = self.writers.lock();
        if self.keys.load().is_empty() {
            return;
        }
        self.keys.store(Arc::new(Vec::new()));
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: u32) -> Key {
        Key::new(id, [id as u8; 32], [id as u8; 12])
    }

    #[test]
    fn empty_ring() {
        let r = Keyring::new();
        assert_eq!(r.count(), 0);
        assert!(r.current().is_none());
        assert!(r.find(1).is_none());
    }

    #[test]
    fn add_current_find() {
        let r = Keyring::new();
        r.add(key(1));
        r.add(key(2));
        r.add(key(3));
        assert_eq!(r.count(), 3);
        assert_eq!(r.current().unwrap().id, 3);
        assert_eq!(r.find(1).unwrap().id, 1);
        assert_eq!(r.find(2).unwrap().key, [2u8; 32]);
        assert!(r.find(4).is_none());
    }

    #[test]
    fn same_id_replaced_in_place() {
        let r = Keyring::new();
        r.add(key(1));
        r.add(key(2));
        r.add(Key::new(1, [99u8; 32], [99u8; 12]));
        assert_eq!(r.count(), 2, "replaced, not appended");
        // Re-adding an older key identifier makes that key the newest entry.
        assert_eq!(r.current().unwrap().id, 1, "the re-added key is the newest");
        assert_eq!(r.find(1).unwrap().key, [99u8; 32]);
    }

    #[test]
    fn repeat_of_newest_is_noop() {
        let r = Keyring::new();
        r.add(key(1));
        r.add(key(1));
        assert_eq!(r.count(), 1);
    }

    #[test]
    fn clear() {
        let r = Keyring::new();
        r.add(key(1));
        r.clear();
        assert_eq!(r.count(), 0);
        assert!(r.current().is_none());
    }

    #[test]
    fn readers_see_snapshots() {
        let r = Arc::new(Keyring::new());
        r.add(key(1));
        let cur = r.current().unwrap();
        r.add(key(2)); // the snapshot taken before stays valid
        assert_eq!(cur.id, 1);
        assert_eq!(r.current().unwrap().id, 2);
    }

    #[test]
    fn unused_snapshots_are_reclaimed_after_updates() {
        let ring = Keyring::new();
        for id in 0..100 {
            let old = Arc::downgrade(&ring.keys.load_full());
            ring.add(key(id));
            assert!(old.upgrade().is_none(), "update {id} retained an unused snapshot");
        }
        assert_eq!(ring.count(), 100);
        let old = Arc::downgrade(&ring.keys.load_full());
        ring.clear();
        assert!(old.upgrade().is_none(), "clear retained keys without readers");
    }

    #[test]
    fn a_key_guard_keeps_its_snapshot_until_dropped() {
        let ring = Keyring::new();
        ring.add(key(1));
        let held = ring.find(1).unwrap();
        let old = Arc::downgrade(&ring.keys.load_full());
        ring.add(key(2));
        ring.clear();
        drop(ring);
        assert_eq!(held.key, [1; 32]);
        assert!(old.upgrade().is_some());
        drop(held);
        assert!(old.upgrade().is_none(), "last reader did not release its snapshot");
    }

    #[test]
    fn adoption_preserves_current_keys_and_newest_order() {
        let older = Keyring::new();
        older.add(key(1));
        older.add(key(2));
        let ring = Keyring::new();
        ring.add(Key::new(2, [99; 32], [99; 12]));
        ring.add(key(3));
        ring.adopt(&older);
        assert_eq!(ring.count(), 3);
        assert_eq!(ring.current().unwrap().id, 3);
        assert_eq!(ring.find(1).unwrap().key, [1; 32]);
        assert_eq!(ring.find(2).unwrap().key, [99; 32]);
        assert_eq!(older.count(), 2);
        ring.adopt(&ring);
        assert_eq!(ring.count(), 3);

        let empty = Keyring::new();
        empty.adopt(&older);
        assert_eq!(empty.count(), 2);
        assert_eq!(empty.current().unwrap().id, 2);
    }

    #[test]
    fn concurrent_readers_keep_valid_keys_during_updates() {
        let ring = Arc::new(Keyring::new());
        ring.add(key(0));
        let start = Arc::new(std::sync::Barrier::new(5));
        std::thread::scope(|scope| {
            for writer in 0..2 {
                let (ring, start) = (ring.clone(), start.clone());
                scope.spawn(move || {
                    start.wait();
                    for id in 1..=100 {
                        ring.add(key(writer * 100 + id));
                    }
                });
            }
            for _ in 0..2 {
                let (ring, start) = (ring.clone(), start.clone());
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..2000 {
                        let held = ring.current().unwrap();
                        let id = held.id;
                        let found = ring.find(id).unwrap();
                        std::thread::yield_now();
                        assert_eq!(held.key, [id as u8; 32]);
                        assert_eq!(found.iv, [id as u8; 12]);
                    }
                });
            }
            start.wait();
        });
        assert_eq!(ring.count(), 201);
        for id in 0..=200 {
            assert_eq!(ring.find(id).unwrap().id, id);
        }
    }
}
