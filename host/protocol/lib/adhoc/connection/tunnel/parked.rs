// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! The sessions of tunnels lost with their tunnels and parked ([`EndPoint`](super::end_point::EndPoint)
//! `resumable`), by slot: each waits for a reclaim on a new tunnel till its deadline, then its close
//! event comes. One thread, started with the first parked session, ends the sessions whose wait ran
//! out.

use std::any::Any;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What a parked session waits with: the session, its identity, its deadline and what ends it.
struct Waiting {
    session: Arc<dyn Any + Send + Sync>,
    ident: u32,
    deadline: i64,
    reap: Box<dyn FnOnce() + Send>,
}

#[derive(Default)]
struct Parked {
    by_slot: HashMap<u32, Waiting>,
    by_deadline: BTreeSet<(i64, u32)>,
}

static NEXT: AtomicU32 = AtomicU32::new(0);
static PARKED: OnceLock<(Mutex<Parked>, Condvar)> = OnceLock::new();

fn parked() -> &'static (Mutex<Parked>, Condvar) {
    PARKED.get_or_init(|| {
        let _ = std::thread::Builder::new().name("AdHoc parked tunnels".into()).spawn(reaper);
        (Mutex::new(Parked::default()), Condvar::new())
    })
}

/// Unix time in seconds: the clock of the deadlines.
#[inline]
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A new slot: the number of a session among the tunnel sessions of the process, never 0.
pub fn slot() -> u32 {
    loop {
        let s = NEXT.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if s != 0 {
            return s;
        }
    }
}

/// The session on `slot` under the identity `ident` parks till `deadline` ( [`now`] seconds ): `reap`
/// ends it then, unless a reclaim [takes](take) it before.
pub fn put(slot: u32, ident: u32, deadline: i64, session: Arc<dyn Any + Send + Sync>, reap: Box<dyn FnOnce() + Send>) {
    let (lock, wake) = parked();
    let mut p = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(was) = p.by_slot.insert(slot, Waiting { session, ident, deadline, reap }) {
        p.by_deadline.remove(&(was.deadline, slot));
    }
    p.by_deadline.insert((deadline, slot));
    wake.notify_one();
}

/// The parked session on `slot` under the identity `ident`, taken off the list; `None` - none.
pub fn take(slot: u32, ident: u32) -> Option<Arc<dyn Any + Send + Sync>> {
    let (lock, _) = PARKED.get()?;
    let mut p = lock.lock().unwrap_or_else(|e| e.into_inner());
    if p.by_slot.get(&slot).is_none_or(|w| w.ident != ident) {
        return None;
    }
    let w = p.by_slot.remove(&slot)?;
    p.by_deadline.remove(&(w.deadline, slot));
    Some(w.session)
}

/// How many sessions of tunnels wait now.
pub fn count() -> usize {
    PARKED.get().map_or(0, |(lock, _)| lock.lock().unwrap_or_else(|e| e.into_inner()).by_slot.len())
}

/// Ends the sessions whose wait ran out, outside the lock.
fn reaper() {
    let (lock, wake) = loop {
        if let Some(p) = PARKED.get() {
            break p;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let mut p = lock.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let t = now();
        let mut due = Vec::new();
        while let Some(&(deadline, slot)) = p.by_deadline.first() {
            if t < deadline {
                break;
            }
            p.by_deadline.pop_first();
            if let Some(w) = p.by_slot.remove(&slot) {
                due.push(w.reap);
            }
        }
        if !due.is_empty() {
            drop(p);
            for reap in due {
                reap();
            }
            p = lock.lock().unwrap_or_else(|e| e.into_inner());
            continue;
        }
        p = match p.by_deadline.first() {
            Some(&(deadline, _)) => {
                wake.wait_timeout(p, Duration::from_millis(((deadline - t).max(0) as u64) * 1000 + 50))
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
            None => wake.wait(p).unwrap_or_else(|e| e.into_inner()),
        };
    }
}
