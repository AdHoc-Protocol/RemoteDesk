// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Packet broadcast over an immutable snapshot of subscribers.
//!
//! [`Broadcaster`] replaces the subscriber array on subscription changes. Each broadcast holds one
//! `arc_swap` snapshot, so callbacks may change subscriptions without changing the current
//! traversal. Captured packet references are reserved in one atomic operation and released by each
//! subscriber after transmission.

use std::sync::Arc;

use crate::lib::adhoc::connection::transmitter::{BytesSrc, Tx};
use crate::lib::adhoc::connection::receiver::bytes_copy::Capture;

/// A connection a broadcast goes to: the generated connection.
pub trait Subscriber: Send + Sync {
    /// Queues the captured pack: `false` — refused (the queue is full, the connection closes).
    fn put_captured(&self, src: Capture) -> bool;
    /// The connection is open: one that refused and is closed is unsubscribed.
    fn is_open(&self) -> bool;
}

/// One pack to many connections: the subscribers, the broadcast.
pub struct Broadcaster<D: Subscriber> {
    /// Never changed in place: a change publishes a new array.
    subscribers: arc_swap::ArcSwap<Vec<Arc<D>>>,
    /// How many there are: what a received pack looks at to know whether to capture itself, one
    /// load.
    len: std::sync::atomic::AtomicUsize,
    /// The changes one at a time: no code of the application runs under it.
    writers: parking_lot::Mutex<()>,
}

impl<D: Subscriber> Default for Broadcaster<D> {
    fn default() -> Self {
        Self { subscribers: arc_swap::ArcSwap::from_pointee(Vec::new()), len: std::sync::atomic::AtomicUsize::new(0), writers: parking_lot::Mutex::new(()) }
    }
}

impl<D: Subscriber> Broadcaster<D> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The subscribers at this moment: a snapshot.
    #[inline]
    pub fn subscribers(&self) -> Arc<Vec<Arc<D>>> {
        self.subscribers.load_full()
    }

    #[inline]
    pub fn count(&self) -> usize {
        self.len.load(std::sync::atomic::Ordering::Acquire)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// A new array of subscribers in place of the one before; the lock of the writers is held.
    #[inline]
    fn publish(&self, now: Vec<Arc<D>>) {
        self.len.store(now.len(), std::sync::atomic::Ordering::Release);
        self.subscribers.store(Arc::new(now));
    }

    /// Drops every subscriber.
    pub fn reset(&self) {
        let _w = self.writers.lock();
        self.publish(Vec::new());
    }

    /// Adds a subscriber. `false` — it is subscribed already.
    pub fn subscribe(&self, dst: &Arc<D>) -> bool {
        let _w = self.writers.lock();
        let s = self.subscribers.load();
        if s.iter().any(|d| Arc::ptr_eq(d, dst)) {
            return false;
        }
        let mut now = Vec::with_capacity(s.len() + 1);
        now.extend(s.iter().cloned());
        now.push(dst.clone());
        drop(s);
        self.publish(now);
        true
    }

    /// Removes a subscriber. `false` — it is not subscribed.
    pub fn unsubscribe(&self, dst: &Arc<D>) -> bool {
        self.remove_where(|d| Arc::ptr_eq(d, dst)) != 0
    }

    /// Removes every subscriber `which` accepts: how many. `which` runs under no lock - it may
    /// change the subscriptions itself -, so a change that came meanwhile has it asked again, about
    /// the subscribers of then.
    pub fn remove_where(&self, mut which: impl FnMut(&Arc<D>) -> bool) -> usize {
        loop {
            let s = self.subscribers.load_full();
            let keep: Vec<Arc<D>> = s.iter().filter(|d| !which(d)).cloned().collect();
            let gone = s.len() - keep.len();
            if gone == 0 {
                return 0;
            }
            let _w = self.writers.lock();
            if Arc::ptr_eq(&self.subscribers.load(), &s) {
                self.publish(keep);
                return gone;
            }
        }
    }

    /// In the region `OnReceiving`: the pack `id` is captured when there is anyone to broadcast it
    /// to.
    #[inline]
    pub fn prepare(&self, src: &mut crate::lib::adhoc::connection::receiver::Rx<'_>, id: u32) {
        if !self.is_empty() {
            src.capture(id);
        }
    }

    /// In the region `OnReceived`: the pack captured is broadcast, the reference of the capture
    /// given back.
    #[inline]
    pub fn broadcast_received(&self, st: &mut crate::lib::adhoc::connection::receiver::RxState) {
        if let Some(c) = st.take_capture() {
            self.broadcast(&c);
        }
    }

    /// `src` to every subscriber of this moment; the caller keeps its own reference. One atomic
    /// step takes the references of all of them; a subscriber that refuses gives its own back. One
    /// that refused and is closed is unsubscribed after the walk; one whose queue is full stays.
    /// One that panics is unsubscribed too: the trouble of one subscriber is not that of the
    /// others, nor of the connection that received the pack.
    pub fn broadcast(&self, src: &Capture) {
        if self.is_empty() {
            return;
        }
        let all = self.subscribers.load(); // no write to shared memory: a broadcast from many threads at once does not contend
        if all.is_empty() {
            return;
        }
        let mut gone: Vec<&Arc<D>> = Vec::new();
        for (d, share) in all.iter().zip(src.share(all.len())) {
            // the share is moved in: dropped as the panic unwinds, it gives its reference back
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.put_captured(share))) {
                Ok(true) => {}
                Ok(false) if d.is_open() => {}
                _ => gone.push(d),
            }
        }
        for d in gone {
            self.unsubscribe(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::adhoc::connection::receiver::bytes_copy::Capture;
    use crate::lib::adhoc::connection::receiver::{Rx, RxHost, RxState};
    use crate::lib::adhoc::connection::transmitter::{TxHost, TxState};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// The body of the pack `id`: `id * 7` bytes.
    fn body(id: u32) -> Vec<u8> {
        (0..id as usize * 7).map(|i| (i * 31 + id as usize) as u8).collect()
    }

    /// Receives packs of bodies of `id * 7` bytes, captures every one, and keeps the captures.
    struct In {
        got: Vec<u8>,
        caps: Vec<Capture>,
    }

    impl RxHost for In {
        fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool {
            src.capture(id);
            self.got = vec![0; id as usize * 7];
            src.slot().index0 = 0;
            true
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            let at = src.slot().index0 as usize;
            let n = src.get_bytes(&mut self.got[at..], 0);
            src.slot().index0 = (at + n) as i32;
            at + n == self.got.len()
        }
        fn on_received(&mut self, st: &mut RxState) {
            let c = st.take_capture().expect("captured");
            assert!(c.body() == &self.got[..]);
            self.caps.push(c);
        }
    }

    /// Sends the captures queued on it.
    #[derive(Default)]
    struct Out {
        q: Mutex<(VecDeque<Capture>, Option<Capture>)>,
        open: bool,
    }

    impl Subscriber for Out {
        fn put_captured(&self, src: Capture) -> bool {
            if !self.open {
                return false;
            }
            self.q.lock().unwrap().0.push_back(src);
            true
        }
        fn is_open(&self) -> bool {
            self.open
        }
    }

    impl TxHost for &Out {
        fn on_serializing(&mut self) -> bool {
            let mut q = self.q.lock().unwrap();
            q.1 = q.0.pop_front();
            q.1.is_some()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            let c = self.q.lock().unwrap().1.clone().unwrap();
            if dst.state() == 0 && !dst.put_val(c.id() as u64, 1, 1) {
                return false;
            }
            dst.put_bytes(&c, 1)
        }
        fn on_serialized(&mut self) {
            self.q.lock().unwrap().1 = None;
        }
    }

    /// Packs received in windows of every size are captured as they came, broadcast to every open
    /// subscriber and sent by each of them in buffers of every size: the bytes out are the bytes
    /// in; the last one out frees the capture.
    #[test]
    fn captured_packs_go_out_as_they_came() {
        let ids: Vec<u32> = (1..60).collect();
        let mut wire = vec![];
        for &id in &ids {
            wire.push(id as u8);
            wire.extend(body(id));
        }
        for window in [1usize, 2, 7, 100, 5000] {
            let mut st = RxState::new(1);
            let mut host = In { got: vec![], caps: vec![] };
            for w in wire.chunks(window) {
                assert_eq!(st.write_all(w, &mut host), w.len() as isize);
            }
            assert_eq!(host.caps.len(), ids.len());
            let subs: Vec<Arc<Out>> = (0..4).map(|i| Arc::new(Out { open: i != 2, ..Default::default() })).collect();
            let b: Broadcaster<Out> = Broadcaster::new();
            for s in &subs {
                assert!(b.subscribe(s));
            }
            assert!(!b.subscribe(&subs[0]), "once");
            for c in host.caps.drain(..) {
                b.broadcast(&c);
                assert_eq!(c.refs(), 1 + 3, "the caller's and those of the three open subscribers");
            }
            assert_eq!(b.count(), 3, "the closed one that refused is gone");
            for (k, s) in subs.iter().enumerate().filter(|(i, _)| *i != 2) {
                let mut st = TxState::new();
                let mut out = vec![];
                let mut buf = vec![0u8; [1usize, 3, 64, 4096][k]];
                let mut h = &**s;
                loop {
                    let n = st.read(&mut buf, &mut h);
                    if n < 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n as usize]);
                }
                assert!(out == wire, "window {window} buffer {}", buf.len());
            }
        }
    }

    /// A share not handed out is given back when the shares drop: one step for all of them.
    #[test]
    fn shares_not_taken_go_back() {
        let c = Capture::new(5, vec![1, 2, 3]);
        let mut s = c.share(10);
        let a = s.next().unwrap();
        let b = s.next().unwrap();
        assert_eq!(c.refs(), 11);
        drop(s);
        assert_eq!(c.refs(), 3);
        drop(a);
        drop(b);
        assert_eq!(c.refs(), 1);
        let d = c.clone();
        assert_eq!(d.refs(), 2);
    }

    /// Panics as it takes a pack.
    struct Broken;
    impl Subscriber for Broken {
        fn put_captured(&self, _src: Capture) -> bool {
            panic!("a broken subscriber");
        }
        fn is_open(&self) -> bool {
            true
        }
    }

    enum Either {
        Ok(Out),
        Broken(Broken),
    }
    impl Subscriber for Either {
        fn put_captured(&self, src: Capture) -> bool {
            match self {
                Either::Ok(o) => o.put_captured(src),
                Either::Broken(b) => b.put_captured(src),
            }
        }
        fn is_open(&self) -> bool {
            true
        }
    }

    /// A subscriber that panics is cut off; those after it still get the pack, its reference is
    /// given back.
    #[test]
    fn a_panicking_subscriber_is_cut_off_and_the_others_get_the_pack() {
        let b: Broadcaster<Either> = Broadcaster::new();
        let subs = [
            Arc::new(Either::Ok(Out { open: true, ..Default::default() })),
            Arc::new(Either::Broken(Broken)),
            Arc::new(Either::Ok(Out { open: true, ..Default::default() })),
        ];
        for s in &subs {
            b.subscribe(s);
        }
        let c = Capture::new(3, body(3));
        b.broadcast(&c);
        assert_eq!(c.refs(), 1 + 2, "the caller's and those of the two that took it");
        assert_eq!(b.count(), 2, "the broken one is gone");
        b.broadcast(&c);
        assert_eq!(c.refs(), 1 + 4);
        for s in [&subs[0], &subs[2]] {
            let Either::Ok(o) = &**s else { unreachable!() };
            assert_eq!(o.q.lock().unwrap().0.len(), 2);
        }
    }

    /// The question of `remove_where` runs under no lock: it may change the subscriptions itself -
    /// and is asked again about the subscribers of then.
    #[test]
    fn remove_where_may_change_the_subscriptions_itself() {
        let b: Broadcaster<Out> = Broadcaster::new();
        let (x, y, z) = (Arc::new(Out::default()), Arc::new(Out::default()), Arc::new(Out::default()));
        b.subscribe(&x);
        b.subscribe(&y);
        let mut asked = 0;
        let gone = b.remove_where(|d| {
            asked += 1;
            if asked == 1 {
                b.subscribe(&z); // a change meanwhile
            }
            Arc::ptr_eq(d, &x)
        });
        assert_eq!(gone, 1);
        assert_eq!(asked, 2 + 3, "asked again about the three of then");
        let now = b.subscribers();
        assert!(now.len() == 2 && Arc::ptr_eq(&now[0], &y) && Arc::ptr_eq(&now[1], &z) && b.count() == 2);
    }
}
