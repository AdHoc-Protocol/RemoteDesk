// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! The sessions of one kind of peer by its identity - the devices of a service, its workers: who holds the place of an identity
//! now, and the token of its session ([`External::identify`], [`External::reclaim`]). A peer that logs in again finds here the
//! session it had: parked - its socket was lost - it moves onto the new socket ([`Sessions::resumed`]); still on a socket that went
//! silent - a dead half a tunnel keeps open - it is made to park first ([`Sessions::dead_half`]). After a restart of the process
//! [`Sessions::restored`] puts back what the process before held. Who wants to know when an identity comes and goes watches it
//! ([`Sessions::watch`]): the presence is told ([`Sessions::set_presence`]).
//!
//! The identities are a range `[first, first + count)`: one place an identity, no allocation after construction. A holder is what
//! the application keeps for the peer - its generated connection, or more kinds of them in one place ([`Sessions::of`]): `ext`
//! gives the transport of a holder of any kind, `mine` tells the holders whose sessions these are. What tells a holder is silent is
//! the application's: `silent` gives its silence in ms, `0` - parked, `-1` - it spoke lately, or it is never taken over. The
//! application decides the rest: whether a second login is let in, what a peer that goes on is told. The transport is called
//! outside the lock.

use std::sync::Arc;

use crate::lib::adhoc::connection::external::External;

/// The transport of a holder; `None` - none now.
pub type Ext<T> = fn(&Arc<T>) -> Option<Arc<dyn External>>;

/// The holders of the identities, shared by the [`Sessions`] of the kinds of one peer.
type Places<T> = Arc<parking_lot::Mutex<Vec<Option<Arc<T>>>>>;

pub struct Sessions<T: ?Sized + Send + Sync> {
    first: u32,
    count: usize,
    places: Places<T>,
    tokens: parking_lot::Mutex<Vec<u64>>,
    ext: Ext<T>,
    mine: fn(&Arc<T>) -> bool,
    presence: std::sync::OnceLock<Presence>,
    /// Who watches each identity, by place: made at the first watch, a list replaced on a change and never changed, so a tell
    /// reads it without the lock.
    watchers: std::sync::OnceLock<Box<[arc_swap::ArcSwapOption<Vec<u32>>]>>,
    /// Whom each identity watches, by place: made at the first watch; the lock of the changes, one each `Sessions`.
    watching: parking_lot::Mutex<Vec<Vec<u32>>>,
}

/// Told that an identity watched came or left: `(watcher, id, here)`.
type Presence = Box<dyn Fn(u32, u32, bool) + Send + Sync>;

/// One transport and another are the same.
fn same(a: &Arc<dyn External>, b: &dyn External) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), b as *const dyn External)
}

/// Takes `v` out of the list of `at`: a new list in its place, none when it was the last; the lock of the changes is held.
fn without(at: &arc_swap::ArcSwapOption<Vec<u32>>, v: u32) {
    let was = at.load();
    let Some(ws) = was.as_deref() else { return };
    let Some(k) = ws.iter().position(|&x| x == v) else { return };
    let now = (ws.len() != 1).then(|| {
        let mut n = Vec::with_capacity(ws.len() - 1);
        n.extend_from_slice(&ws[..k]);
        n.extend_from_slice(&ws[k + 1..]);
        Arc::new(n)
    });
    drop(was);
    at.store(now);
}

impl<T: ?Sized + Send + Sync> Sessions<T> {
    /// Identities `[first, first + count)`; `ext` - the transport of a holder; `mine` - the holders whose sessions these are.
    pub fn new(first: u32, count: usize, ext: Ext<T>, mine: fn(&Arc<T>) -> bool) -> Self {
        Self { first, count, places: Arc::new(parking_lot::Mutex::new(vec![None; count])), tokens: parking_lot::Mutex::new(vec![0; count]), ext, mine, presence: std::sync::OnceLock::new(), watchers: std::sync::OnceLock::new(), watching: parking_lot::Mutex::new(Vec::new()) }
    }

    /// The places of `share`, tokens of its own, the sessions of the holders `mine` tells: two kinds of session of one peer, one of
    /// them on at a time - a program and the starter of it.
    pub fn of(share: &Sessions<T>, mine: fn(&Arc<T>) -> bool) -> Self {
        let count = share.count;
        Self { first: share.first, count, places: share.places.clone(), tokens: parking_lot::Mutex::new(vec![0; count]), ext: share.ext, mine, presence: std::sync::OnceLock::new(), watchers: std::sync::OnceLock::new(), watching: parking_lot::Mutex::new(Vec::new()) }
    }

    fn at(&self, id: u32) -> Option<usize> {
        let i = id.wrapping_sub(self.first) as usize;
        (id >= self.first && i < self.count).then_some(i)
    }

    /// Whether `id` is in the range.
    pub fn has(&self, id: u32) -> bool {
        self.at(id).is_some()
    }

    /// Who holds the place of `id`; `None` - nobody, or `id` is out of the range.
    pub fn held(&self, id: u32) -> Option<Arc<T>> {
        self.at(id).and_then(|i| self.places.lock()[i].clone())
    }

    /// Takes the place of `id` for `h`; `false` - it is held, or `id` is out of the range.
    pub fn claim(&self, id: u32, h: &Arc<T>) -> bool {
        let Some(i) = self.at(id) else { return false };
        {
            let mut p = self.places.lock();
            if p[i].is_some() {
                return false;
            }
            p[i] = Some(h.clone());
        }
        self.tell(i, id, true);
        true
    }

    /// The session of `h` is over: the place of `id` goes back and its token with it, if `h` holds it - a holder that was taken over
    /// gives back nothing. A session parked by a lost socket is not over: call it from its close event. What `id` watched it watches
    /// no more.
    pub fn left(&self, id: u32, h: &Arc<T>) -> bool {
        let Some(i) = self.at(id) else { return false };
        {
            let mut p = self.places.lock();
            if !p[i].as_ref().is_some_and(|x| Arc::ptr_eq(x, h)) {
                return false;
            }
            p[i] = None;
        }
        self.tokens.lock()[i] = 0;
        self.tell(i, id, false);
        if let Some(all) = self.watchers.get() {
            let mut watching = self.watching.lock();
            for of in std::mem::take(&mut watching[i]) {
                without(&all[(of - self.first) as usize], id);
            }
        }
        true
    }

    /// Told that an identity watched came - its place is taken - or left: `(watcher, id, here)`, on the thread that took or gave
    /// back the place, no lock held; what the watcher is to know the application sends it. A tell may pass a newer one of the same
    /// identity: [`held`](Self::held) says how it is now. A session that waits parked has not left. Once: a second one is ignored.
    pub fn set_presence(&self, f: impl Fn(u32, u32, bool) + Send + Sync + 'static) {
        let _ = self.presence.set(Box::new(f));
    }

    /// `watcher` is told when `id` comes and goes ([`set_presence`](Self::set_presence)), till it [stops](Self::unwatch) or
    /// [leaves](Self::left); whether `id` is here now - [`held`](Self::held). `false` - it watches it already, or either is out of
    /// the range.
    pub fn watch(&self, watcher: u32, id: u32) -> bool {
        let (Some(w), Some(i)) = (self.at(watcher), self.at(id)) else { return false };
        let mut watching = self.watching.lock();
        if watching.is_empty() {
            *watching = vec![Vec::new(); self.count];
        }
        if watching[w].contains(&id) {
            return false;
        }
        watching[w].push(id);
        let at = &self.watchers.get_or_init(|| (0..self.count).map(|_| arc_swap::ArcSwapOption::empty()).collect())[i];
        let was = at.load();
        let ws = was.as_deref().map_or(&[][..], |v| v.as_slice());
        let mut now = Vec::with_capacity(ws.len() + 1);
        now.extend_from_slice(ws);
        now.push(watcher);
        drop(was);
        at.store(Some(Arc::new(now)));
        true
    }

    /// `watcher` is told no more about `id`; `false` - it did not watch it.
    pub fn unwatch(&self, watcher: u32, id: u32) -> bool {
        let (Some(w), Some(i), Some(all)) = (self.at(watcher), self.at(id), self.watchers.get()) else { return false };
        let mut watching = self.watching.lock();
        let Some(k) = watching[w].iter().position(|&x| x == id) else { return false };
        watching[w].swap_remove(k);
        without(&all[i], watcher);
        true
    }

    /// The watchers of `id` at place `i` are told.
    fn tell(&self, i: usize, id: u32, here: bool) {
        let (Some(p), Some(all)) = (self.presence.get(), self.watchers.get()) else { return };
        if let Some(ws) = all[i].load_full() {
            for &w in ws.iter() {
                p(w, id, here);
            }
        }
    }

    /// The place of `id` goes to `h` from its holder, if that one is silent: its socket is aborted - the one that was parked gives
    /// the place up at once and ends when its wait runs out. Its silence in ms, `0` - it was parked; `-1` - the place stays: the
    /// holder spoke lately, or it is never taken over.
    pub fn take_over(&self, id: u32, h: &Arc<T>, silent: impl Fn(&Arc<T>) -> i64) -> i64 {
        let Some(i) = self.at(id) else { return -1 };
        let e = {
            let mut p = self.places.lock();
            let Some(old) = p[i].clone() else {
                p[i] = Some(h.clone());
                drop(p);
                self.tell(i, id, true);
                return 0;
            };
            let Some(e) = (self.ext)(&old) else { return -1 };
            let ms = silent(&old);
            if ms < 0 {
                return -1;
            }
            p[i] = Some(h.clone());
            (e, ms)
        };
        if e.0.parked() < 0 {
            e.0.abort();
        }
        e.1
    }

    /// The session on `e` is of `id` from here on: [`External::identify`]. Its token.
    pub fn identify(&self, id: u32, e: &dyn External) -> u64 {
        let t = e.identify(id);
        if let Some(i) = self.at(id) {
            self.tokens.lock()[i] = t;
        }
        t
    }

    /// The token of the session of `id`; `0` - none yet.
    pub fn token(&self, id: u32) -> u64 {
        self.at(id).map_or(0, |i| self.tokens.lock()[i])
    }

    /// The session of `id` waits parked: it moves onto `e` in the state it was in, what either side was sending going on from the
    /// byte the other has. Its holder, or `None` - there is none to go on with: the login goes the regular way.
    pub fn resumed(&self, id: u32, e: &dyn External) -> Option<Arc<T>> {
        let i = self.at(id)?;
        let token = self.tokens.lock()[i];
        let h = self.places.lock()[i].clone()?;
        if token == 0 || !(self.mine)(&h) || (self.ext)(&h)?.parked() < 0 {
            return None;
        }
        let t = e.reclaim(token);
        self.tokens.lock()[i] = t;
        (self.ext)(&h).is_some_and(|x| same(&x, e)).then_some(h)
    }

    /// The session of `id` is still on a socket that went silent - a dead half: it is made to park, as after any loss. The login is
    /// cut, and the peer logs in again, its session parked by then. `false` - it is not: none, parked already, or it spoke lately.
    pub fn dead_half(&self, id: u32, silent: impl Fn(&Arc<T>) -> i64) -> bool {
        let Some(i) = self.at(id) else { return false };
        if self.tokens.lock()[i] == 0 {
            return false;
        }
        let Some(h) = self.places.lock()[i].clone() else { return false };
        if !(self.mine)(&h) {
            return false;
        }
        let Some(e) = (self.ext)(&h) else { return false };
        if 0 <= e.parked() || silent(&h) < 0 {
            return false;
        }
        e.lost();
        true
    }

    /// The session of `id` the process before held waits parked on `e` (a restore): `h` takes its place again and its token - the
    /// peer dials again and goes on with it. `false` - the place is held.
    pub fn restored(&self, id: u32, h: &Arc<T>, e: &dyn External) -> bool {
        if !self.claim(id, h) {
            return false;
        }
        let t = e.session();
        if let Some(i) = self.at(id) {
            self.tokens.lock()[i] = t;
        }
        true
    }
}

/// The holders are the generated connections of the hosts: the usual route of a tunnel sends it to the holder of its address
/// ([`crate::lib::adhoc::connection::tunnel::relay::Relay::route`]).
impl<T: Send + Sync + 'static> crate::lib::adhoc::connection::tunnel::Hosts for Sessions<T> {
    /// The holder of `address`; `None` - nobody holds it, or it has no transport now.
    fn at(&self, address: u32) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        let h = self.held(address)?;
        (self.ext)(&h)?;
        Some(h)
    }
}
