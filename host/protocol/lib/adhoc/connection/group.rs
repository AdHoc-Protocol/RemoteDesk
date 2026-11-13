// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Connection groups with constant-time membership changes.
//!
//! A member belongs to at most one group, and a group to at most one group list. Membership changes
//! and traversal hold the group's reentrant lock, allowing callbacks to join or leave on the same
//! thread. Traversal visits newer members first.
//!
//! Removing a member leaves an empty slot so an active traversal does not skip or repeat the
//! remaining members. Empty slots are compacted only outside traversal, once they outnumber live
//! members; this amortizes compaction over removals.

// GroupMember / Group / Groups

use std::cell::UnsafeCell;
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use crate::lib::adhoc::spin_lock::FastReentrantSpinLock;

/// The places of the members of a group, or of the groups of a list. Touched only under the lock of
/// their owner.
struct Places<T> {
    /// The oldest first; `None` — left.
    v: Vec<Option<Arc<T>>>,
    live: usize,
    holes: usize,
    /// Walks in progress (the lock is reentrant: a walk may walk).
    walking: u32,
    /// Those that left during a walk: kept until the walk is over, a walk lends them to its
    /// callback by reference.
    gone: Vec<Arc<T>>,
}

impl<T> Default for Places<T> {
    fn default() -> Self {
        Self {
            v: Vec::new(),
            live: 0,
            holes: 0,
            walking: 0,
            gone: Vec::new(),
        }
    }
}

impl<T> Places<T> {
    /// A new one at the end: its place.
    #[inline]
    fn push(&mut self, t: Arc<T>) -> usize {
        self.v.push(Some(t));
        self.live += 1;
        self.v.len() - 1
    }

    /// The one at `i` leaves: its place is emptied - the last places, when they are empty, are
    /// dropped. What the caller drops, unless a walk is on: then it stays until the walk is over.
    #[inline]
    fn take(&mut self, i: usize) -> Option<Arc<T>> {
        let t = self.v[i].take();
        self.live -= 1;
        self.holes += 1;
        while let Some(None) = self.v.last() {
            self.v.pop(); // places above the one a walk is at: it was there already
            self.holes -= 1;
        }
        if self.walking == 0 {
            return t;
        }
        self.gone.extend(t);
        None
    }

    /// The empty places are many, and no walk is on: they are to be closed up.
    #[inline]
    fn crowded(&self) -> bool {
        self.walking == 0 && self.live < self.holes
    }

    /// Closes the empty places up, the order kept; each one moved hears its new place.
    fn close_up(&mut self, moved: impl Fn(&T, usize)) {
        let mut j = 0;
        for i in 0..self.v.len() {
            if let Some(t) = self.v[i].take() {
                if i != j {
                    moved(&t, j);
                }
                self.v[j] = Some(t);
                j += 1;
            }
        }
        self.v.truncate(j);
        self.holes = 0;
    }
}

/// Ends a walk however it ends (its callback may panic): those that left during it go, the places
/// are closed up.
struct Walk<'a, T> {
    places: &'a UnsafeCell<Places<T>>,
    moved: fn(&T, usize),
}

impl<T> Drop for Walk<'_, T> {
    fn drop(&mut self) {
        // SAFETY: under the owner's lock, which the walk holds; no reference into the places is
        // out.
        let p = unsafe { &mut *self.places.get() };
        p.walking -= 1;
        if p.walking == 0 {
            p.gone.clear();
            if p.live < p.holes {
                p.close_up(self.moved);
            }
        }
    }
}

/// The newest first, each lent to `f`: by reference, nothing counted - one that leaves meanwhile is
/// kept until the walk is over. Under the owner's lock: `f` may join and leave on this thread, a
/// newcomer is not walked.
fn walk<T>(places: &UnsafeCell<Places<T>>, moved: fn(&T, usize), mut f: impl FnMut(&T)) {
    let mut i = {
        // SAFETY: under the owner's lock; the borrow ends here.
        let p = unsafe { &mut *places.get() };
        p.walking += 1;
        p.v.len()
    };
    let _end = Walk { places, moved };
    while 0 < i {
        i -= 1;
        // SAFETY: under the owner's lock; the borrow ends before `f`. The pointer is to the heap of
        // an `Arc` that the places or `gone` hold until the walk is over: `f` may move the row (a
        // join) or empty the place (a leave).
        let p = unsafe { &*places.get() };
        let t: *const T = match p.v.get(i) {
            Some(Some(t)) => Arc::as_ptr(t),
            _ => continue, // left, or above the row now
        };
        f(unsafe { &*t });
    }
}

/// Where a member stands: the group it is in and its place there. Taken from `None` only (or from a
/// group that is gone), so a member is in one group at most, whoever joins it where.
struct MemberSlot<M, S> {
    group: Option<Weak<Group<M, S>>>,
    index: usize,
}

/// A group member: the value plus where it stands. Use through `Arc<GroupMember<M, S>>` — groups
/// hold strong references to members. `S` — what the members of its group share ([`Group::state`]).
pub struct GroupMember<M, S = ()> {
    /// Membership state embedded in each group member.
    pub value: M,
    slot: Mutex<MemberSlot<M, S>>,
}

impl<M, S> GroupMember<M, S> {
    pub fn new(value: M) -> Self {
        Self {
            value,
            slot: Mutex::new(MemberSlot { group: None, index: 0 }),
        }
    }

    /// The group it is a member of; `None` — none.
    pub fn group(&self) -> Option<Arc<Group<M, S>>> {
        self.slot.lock().group.as_ref().and_then(Weak::upgrade)
    }

    /// Leaves its group, if it is in one. An instance that is gone is in no group: call it from
    /// your release path.
    pub fn leave(self: &Arc<Self>) {
        let g = self.slot.lock().group.as_ref().and_then(Weak::upgrade);
        if let Some(g) = g {
            g.remove(self);
        }
    }
}

/// The list a group is in, whatever the list keeps of its own: the group leaves it through this.
trait ListOf<M, S>: Send + Sync {
    fn remove(&self, g: &Arc<Group<M, S>>);
}

/// A group of members — the place for what all the members share. Use through `Arc<Group<M, S>>`.
pub struct Group<M, S = ()> {
    /// What all the members share.
    pub state: S,
    /// The lock of the group: joining, leaving and a walk are under it, and nothing else is.
    pub lock: FastReentrantSpinLock,
    /// Under `lock`.
    members: UnsafeCell<Places<GroupMember<M, S>>>,
    /// The list of groups this one is in, and its place there; `None` — none.
    list: Mutex<Option<(Weak<dyn ListOf<M, S>>, usize)>>,
}

// SAFETY: `members` is touched only under `lock`.
unsafe impl<M: Send + Sync, S: Send + Sync> Send for Group<M, S> {}
unsafe impl<M: Send + Sync, S: Send + Sync> Sync for Group<M, S> {}

impl<M, S: Default> Default for Group<M, S> {
    fn default() -> Self {
        Self::with(S::default())
    }
}

impl<M, S: Default> Group<M, S> {
    pub fn new() -> Self {
        Self::with(S::default())
    }
}

impl<M, S> Group<M, S> {
    /// A group whose members share `state`.
    pub fn with(state: S) -> Self {
        Self {
            state,
            lock: FastReentrantSpinLock::new(),
            members: UnsafeCell::new(Places::default()),
            list: Mutex::new(None),
        }
    }

    /// A member moved to place `i`.
    fn moved(m: &GroupMember<M, S>, i: usize) {
        m.slot.lock().index = i;
    }

    /// How many members now.
    pub fn size(&self) -> usize {
        let _g = self.lock.lock();
        // SAFETY: under `lock`.
        unsafe { (*self.members.get()).live }
    }

    /// Joins this group, and leaves the group the member was in: one group at a time. `false` — it
    /// is in this group already, or another join took it meanwhile.
    pub fn enter(self: &Arc<Self>, member: &Arc<GroupMember<M, S>>) -> bool {
        if member.group().is_some_and(|was| Arc::ptr_eq(&was, self)) {
            return false; // one read: another thread may move it meanwhile
        }
        member.leave();
        self.add(member)
    }

    /// Takes the member from no group only. `false` — it is in a group meanwhile.
    fn add(self: &Arc<Self>, member: &Arc<GroupMember<M, S>>) -> bool {
        let _g = self.lock.lock();
        let mut slot = member.slot.lock();
        if slot.group.as_ref().is_some_and(|w| w.strong_count() != 0) {
            return false;
        }
        // SAFETY: under `lock`; no other borrow is out (a walk lends no reference into the places).
        slot.index = unsafe { (*self.members.get()).push(member.clone()) };
        slot.group = Some(Arc::downgrade(self));
        true
    }

    /// Takes the member out where it stands.
    fn remove(self: &Arc<Self>, member: &Arc<GroupMember<M, S>>) {
        let _g = self.lock.lock();
        let gone = {
            let mut slot = member.slot.lock();
            if !slot.group.as_ref().is_some_and(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(self))) {
                return;
            }
            slot.group = None;
            // SAFETY: under `lock`.
            unsafe { (*self.members.get()).take(slot.index) }
        };
        drop(gone);
        // SAFETY: under `lock`; the slot of the member is free again, `moved` takes the slots of
        // others.
        let p = unsafe { &mut *self.members.get() };
        if p.crowded() {
            p.close_up(Self::moved);
        }
    }

    /// A walk over the members, the newest first, under the group's lock: `f` may join and leave,
    /// this group too. One that leaves is not walked any more, one that joins is not walked.
    pub fn walk(&self, f: impl FnMut(&GroupMember<M, S>)) {
        let _g = self.lock.lock();
        walk(&self.members, Self::moved, f);
    }

    /// Joins the list of groups `l`, and leaves the one it was in. `false` — it is in `l` already,
    /// or another join took it meanwhile.
    pub fn join_list<L: Send + Sync + 'static>(self: &Arc<Self>, l: &Arc<Groups<M, S, L>>) -> bool
    where
        M: Send + Sync + 'static,
        S: Send + Sync + 'static,
    {
        let was = self.list.lock().as_ref().and_then(|(w, _)| w.upgrade());
        if was.is_some_and(|was| std::ptr::addr_eq(Arc::as_ptr(&was), Arc::as_ptr(l))) {
            return false;
        }
        self.leave_list();
        l.add(self)
    }

    /// Leaves its list of groups, if it is in one.
    pub fn leave_list(self: &Arc<Self>) {
        let l = self.list.lock().as_ref().and_then(|(w, _)| w.upgrade());
        if let Some(l) = l {
            l.remove(self);
        }
    }
}

/// A list of groups — the feeds of a telemetry, the rooms of a chat, and what the groups listed
/// share. Use through `Arc<Groups<M, S, L>>`.
pub struct Groups<M, S = (), L = ()> {
    /// What the groups listed share.
    pub state: L,
    /// The lock of the list: joining, leaving and a walk are under it.
    pub lock: FastReentrantSpinLock,
    /// Under `lock`.
    groups: UnsafeCell<Places<Group<M, S>>>,
}

// SAFETY: `groups` is touched only under `lock`.
unsafe impl<M: Send + Sync, S: Send + Sync, L: Send + Sync> Send for Groups<M, S, L> {}
unsafe impl<M: Send + Sync, S: Send + Sync, L: Send + Sync> Sync for Groups<M, S, L> {}

impl<M, S, L: Default> Default for Groups<M, S, L> {
    fn default() -> Self {
        Self::with(L::default())
    }
}

impl<M, S, L: Default> Groups<M, S, L> {
    pub fn new() -> Self {
        Self::with(L::default())
    }
}

impl<M: Send + Sync + 'static, S: Send + Sync + 'static, L: Send + Sync + 'static> ListOf<M, S> for Groups<M, S, L> {
    fn remove(&self, g: &Arc<Group<M, S>>) {
        Groups::remove(self, g)
    }
}

impl<M, S, L> Groups<M, S, L> {
    /// A list whose groups share `state`.
    pub fn with(state: L) -> Self {
        Self {
            state,
            lock: FastReentrantSpinLock::new(),
            groups: UnsafeCell::new(Places::default()),
        }
    }

    /// A group moved to place `i`.
    fn moved(g: &Group<M, S>, i: usize) {
        if let Some(l) = g.list.lock().as_mut() {
            l.1 = i;
        }
    }

    /// How many groups now.
    pub fn size(&self) -> usize {
        let _g = self.lock.lock();
        // SAFETY: under `lock`.
        unsafe { (*self.groups.get()).live }
    }

    /// Takes the group from no list only.
    fn add(self: &Arc<Self>, g: &Arc<Group<M, S>>) -> bool
    where
        M: Send + Sync + 'static,
        S: Send + Sync + 'static,
        L: Send + Sync + 'static,
    {
        let _l = self.lock.lock();
        let mut list = g.list.lock();
        if list.as_ref().is_some_and(|(w, _)| w.strong_count() != 0) {
            return false;
        }
        // SAFETY: under `lock`.
        let at = unsafe { (*self.groups.get()).push(g.clone()) };
        let me: Weak<dyn ListOf<M, S>> = Arc::downgrade(self) as Weak<Self> as Weak<dyn ListOf<M, S>>;
        *list = Some((me, at));
        true
    }

    /// Takes the group out where it stands.
    fn remove(&self, g: &Arc<Group<M, S>>) {
        let _l = self.lock.lock();
        let gone = {
            let mut list = g.list.lock();
            let at = match list.as_ref() {
                Some((w, at)) if std::ptr::addr_eq(w.as_ptr(), self as *const Self) => *at,
                _ => return,
            };
            *list = None;
            // SAFETY: under `lock`.
            unsafe { (*self.groups.get()).take(at) }
        };
        drop(gone);
        // SAFETY: under `lock`.
        let p = unsafe { &mut *self.groups.get() };
        if p.crowded() {
            p.close_up(Self::moved);
        }
    }

    /// A walk over the groups, the newest first, under the list's lock: `f` may join and leave.
    pub fn walk(&self, f: impl FnMut(&Group<M, S>)) {
        let _l = self.lock.lock();
        walk(&self.groups, Self::moved, f);
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn member(v: usize) -> Arc<GroupMember<usize>> {
        Arc::new(GroupMember::new(v))
    }

    #[test]
    fn spinlock_reentrant() {
        let l = FastReentrantSpinLock::new();
        l.acquire();
        l.acquire();
        let _g = l.lock(); // third level, via the guard
        drop(_g);
        l.release();
        l.release();
        // Fully released: another acquire succeeds immediately.
        l.acquire();
        l.release();
    }

    #[test]
    fn spinlock_excludes_other_thread() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let l = Arc::new(FastReentrantSpinLock::new());
        let entered = Arc::new(AtomicBool::new(false));
        l.acquire();
        let l2 = l.clone();
        let e2 = entered.clone();
        let t = std::thread::spawn(move || {
            l2.acquire(); // spins until the main thread releases
            e2.store(true, Ordering::SeqCst);
            l2.release();
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!entered.load(Ordering::SeqCst), "contender entered while held");
        l.release();
        t.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
    }

    #[test]
    fn enter_leave_walk() {
        let g = Arc::new(Group::<usize>::new());
        let a = member(1);
        let b = member(2);
        let c = member(3);
        assert!(g.enter(&a));
        assert!(g.enter(&b));
        assert!(g.enter(&c));
        assert_eq!(g.size(), 3);
        assert!(!g.enter(&a), "already in this group");
        // Newest first.
        let mut seen = Vec::new();
        g.walk(|m| seen.push(m.value));
        assert_eq!(seen, vec![3, 2, 1]);

        b.leave();
        assert_eq!(g.size(), 2);
        assert!(b.group().is_none());
        let mut seen = Vec::new();
        g.walk(|m| seen.push(m.value));
        assert_eq!(seen, vec![3, 1]);
    }

    #[test]
    fn one_group_at_a_time() {
        let g1 = Arc::new(Group::<usize>::new());
        let g2 = Arc::new(Group::<usize>::new());
        let a = member(1);
        assert!(g1.enter(&a));
        assert!(g2.enter(&a), "moving is leaving + joining");
        assert_eq!(g1.size(), 0);
        assert_eq!(g2.size(), 1);
        assert!(a.group().is_some_and(|g| Arc::ptr_eq(&g, &g2)));
    }

    #[test]
    fn groups_list_join_leave_walk() {
        let l = Arc::new(Groups::<usize>::new());
        let g1 = Arc::new(Group::new());
        let g2 = Arc::new(Group::new());
        let g3 = Arc::new(Group::new());
        assert!(g1.join_list(&l));
        assert!(g2.join_list(&l));
        assert!(g3.join_list(&l));
        assert_eq!(l.size(), 3);
        assert!(!g1.join_list(&l), "already in this list");

        let mut count = 0;
        l.walk(|_| count += 1);
        assert_eq!(count, 3);

        g2.leave_list();
        assert_eq!(l.size(), 2);
        // Re-joining a left group works; a group in another list is refused.
        assert!(g2.join_list(&l));
        let l2 = Arc::new(Groups::<usize>::new());
        assert!(g1.join_list(&l2), "moving between lists is leave + join");
        assert_eq!(l.size(), 2);
        assert_eq!(l2.size(), 1);
    }

    /// A walk that makes members leave and join, this group too, on its own thread: no deadlock,
    /// none walked twice, none that stayed skipped, the newcomer not walked; the order of those
    /// that stay is kept.
    #[test]
    fn a_walk_may_join_and_leave() {
        let g = Arc::new(Group::<usize>::new());
        let ms: Vec<_> = (0..10).map(member).collect();
        for m in &ms {
            assert!(g.enter(m));
        }
        let late = member(100);
        let mut seen = Vec::new();
        g.walk(|m| {
            seen.push(m.value);
            match m.value {
                8 => ms[3].leave(),           // one not walked yet: it is not walked
                6 => ms[9].leave(),           // one walked already
                5 => ms[5].leave(),           // itself
                4 => assert!(g.enter(&late)), // a newcomer: not walked
                _ => {}
            }
        });
        assert_eq!(seen, vec![9, 8, 7, 6, 5, 4, 2, 1, 0]);
        let mut now = Vec::new();
        g.walk(|m| now.push(m.value));
        assert_eq!(now, vec![100, 8, 7, 6, 4, 2, 1, 0]);
        assert_eq!(g.size(), 8);
        for m in &ms {
            m.leave();
        }
        late.leave();
        assert_eq!(g.size(), 0);
        assert!(ms[0].group().is_none());
        assert!(g.enter(&ms[0]), "the places closed up: joining goes on");
    }

    /// What the members of a group share and what the groups of a list share: a member reaches its
    /// group's, a walk of the list each group's and the list's.
    #[test]
    fn groups_and_lists_keep_what_they_share() {
        let rooms = Arc::new(Groups::<usize, &'static str, u32>::with(7));
        let red = Arc::new(Group::with("red"));
        let blue = Arc::new(Group::with("blue"));
        assert!(red.join_list(&rooms) && blue.join_list(&rooms));
        let a = Arc::new(GroupMember::<usize, &'static str>::new(1));
        assert!(red.enter(&a));
        assert_eq!(a.group().unwrap().state, "red");
        let mut seen = vec![];
        rooms.walk(|g| seen.push((g.state, g.size(), rooms.state)));
        assert_eq!(seen, vec![("blue", 0, 7), ("red", 1, 7)]);
        red.leave_list();
        assert_eq!(rooms.size(), 1);
    }

    /// A member of a group that is gone joins another.
    #[test]
    fn a_group_that_is_gone_lets_its_members_go() {
        let a = member(1);
        {
            let g = Arc::new(Group::<usize>::new());
            assert!(g.enter(&a));
        }
        assert!(a.group().is_none());
        let g2 = Arc::new(Group::<usize>::new());
        assert!(g2.enter(&a));
        assert_eq!(g2.size(), 1);
    }

    #[test]
    fn concurrent_enter_leave() {
        let g = Arc::new(Group::<usize>::new());
        let members: Vec<_> = (0..64).map(member).collect();
        let mut handles = Vec::new();
        for round in 0..4 {
            let g = g.clone();
            let members = members.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    let m = &members[round * 16..(round + 1) * 16];
                    for mm in m {
                        g.enter(mm);
                    }
                    for mm in m {
                        mm.leave();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(g.size(), 0);
    }
}
