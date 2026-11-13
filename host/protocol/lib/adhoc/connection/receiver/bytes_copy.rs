// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Reference-counted copies of received packets for forwarding and broadcast.
//!
//! [`Rx::capture`](super::Rx::capture) requests capture while a packet is being received. A
//! completed [`Capture`] retains its identifier and encoded bytes, avoiding serialization when
//! forwarding. [`Capture::share`] reserves several references in one atomic operation.
//!
//! The last reference returns the buffer to a process-wide [`Pool`], preserving its capacity.
//! Thread-local magazines allow capture and final release to occur on different loop threads.

use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicUsize, Ordering};

use crate::lib::adhoc::connection::transmitter::{BytesSrc, Tx};
use crate::lib::collections::Pool;

/// The bytes of a captured pack, with the count of their references.
struct Bytes {
    refs: AtomicUsize,
    id: u32,
    body: Vec<u8>,
}

/// A pack received and kept as its bytes: one reference. Sent, it writes its pack id and its body
/// as they came; dropped, the reference is given back.
pub struct Capture(NonNull<Bytes>);

// SAFETY: the bytes are not changed once captured; the count is atomic.
unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

/// How many captures the depot of the pool keeps: what a burst of broadcasts in flight needs.
const POOL: usize = 1 << 10;
/// A buffer that grew larger than this goes, not back to the pool: the pool is not to hold the room
/// of the largest pack.
const KEEP_MAX: usize = 64 << 10;

fn fresh() -> Box<Bytes> {
    Box::new(Bytes { refs: AtomicUsize::new(1), id: 0, body: Vec::new() })
}

/// The captures given back, to be taken again.
fn pool() -> &'static Pool<Box<Bytes>> {
    static FREE: std::sync::OnceLock<Pool<Box<Bytes>>> = std::sync::OnceLock::new();
    FREE.get_or_init(|| Pool::new(POOL.trailing_zeros(), fresh))
}

/// `p`, no reference left, kept for the next capture; a full pool frees it.
#[inline]
fn keep(p: NonNull<Bytes>) {
    // SAFETY: no reference is left: this is the owner
    pool().release(unsafe { Box::from_raw(p.as_ptr()) });
}

impl Capture {
    /// The capture of the pack `id`, its body empty: one of the pool, or a new one.
    pub(crate) fn start(id: u32) -> Self {
        // given back with no reference left, or new: this one is the only
        let mut b = pool().acquire();
        b.id = id;
        b.body.clear();
        *b.refs.get_mut() = 1;
        Self(NonNull::from(Box::leak(b)))
    }

    /// A capture of `body` as it is.
    #[cfg(test)]
    pub(crate) fn new(id: u32, body: Vec<u8>) -> Self {
        let mut c = Self::start(id);
        *c.body_mut() = body;
        c
    }

    /// The body being captured: while the receive holds the only reference, before it is shared.
    #[inline]
    pub(crate) fn body_mut(&mut self) -> &mut Vec<u8> {
        debug_assert_eq!(self.refs(), 1);
        // SAFETY: the only reference, and the receive's own: nobody else reads the bytes yet
        unsafe { &mut (*self.0.as_ptr()).body }
    }

    #[inline]
    fn bytes(&self) -> &Bytes {
        // SAFETY: alive while a reference is.
        unsafe { self.0.as_ref() }
    }

    /// The pack id.
    #[inline]
    pub fn id(&self) -> u32 {
        self.bytes().id
    }

    /// The body: the bytes after the pack id (and the headers of the connection it came by).
    #[inline]
    pub fn body(&self) -> &[u8] {
        &self.bytes().body
    }

    /// The references held now: a snapshot.
    pub fn refs(&self) -> usize {
        self.bytes().refs.load(Ordering::Relaxed)
    }

    /// `n` more references, taken in one atomic step: the shares of the subscribers of a broadcast.
    /// Those not taken from the iterator are given back in one step when it drops.
    pub fn share(&self, n: usize) -> Shares {
        if n != 0 {
            self.bytes().refs.fetch_add(n, Ordering::Relaxed);
        }
        Shares { p: self.0, left: n }
    }
}

impl Clone for Capture {
    fn clone(&self) -> Self {
        self.bytes().refs.fetch_add(1, Ordering::Relaxed);
        Self(self.0)
    }
}

/// Gives `n` references of `p` back: the last gives the bytes back to the pool - or frees them, a
/// buffer grown too large or the pool full.
#[inline]
fn release(p: NonNull<Bytes>, n: usize) {
    // SAFETY: `p` is alive while its references are; the last one takes it over, no one reads it
    // after.
    if unsafe { p.as_ref() }.refs.fetch_sub(n, Ordering::Release) == n {
        fence(Ordering::Acquire);
        if KEEP_MAX < unsafe { p.as_ref() }.body.capacity() {
            drop(unsafe { Box::from_raw(p.as_ptr()) });
        } else {
            keep(p);
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        release(self.0, 1);
    }
}

/// References taken in one step ([`Capture::share`]), handed out one by one.
pub struct Shares {
    p: NonNull<Bytes>,
    left: usize,
}

impl Iterator for Shares {
    type Item = Capture;

    #[inline]
    fn next(&mut self) -> Option<Capture> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        Some(Capture(self.p))
    }
}

impl Drop for Shares {
    fn drop(&mut self) {
        if self.left != 0 {
            release(self.p, self.left);
        }
    }
}

/// Sent as a pack of the connection: the body after the id and the headers the transmitter wrote.
impl BytesSrc for Capture {
    fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
        if dst.state() == 1 {
            dst.slot().index0 = 0; // the body from its start
        }
        let body = self.body();
        let at = dst.slot().index0 as usize;
        let n = dst.put_slice(&body[at..], 2);
        if at + n < body.len() {
            dst.slot().index0 = (at + n) as i32;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of a capture the last share gave back are taken by the next capture, their room
    /// with them; a buffer grown past `KEEP_MAX` is not kept. The pool is one for the process: the
    /// tests beside take from it too, so a capture is looked for among a few.
    #[test]
    fn a_capture_given_back_is_taken_again_with_its_room() {
        let mut c = Capture::start(7);
        c.body_mut().extend_from_slice(&[1; 1000]);
        let at = c.0;
        let shares: Vec<Capture> = c.share(3).collect();
        drop(c);
        drop(shares); // the last one out: back to the pool
        let again: Vec<Capture> = (0..POOL).map(|i| Capture::start(i as u32)).collect();
        let back = again.iter().find(|x| x.0 == at).expect("taken again");
        assert!(back.body().is_empty() && 1000 <= unsafe { at.as_ref() }.body.capacity() && back.refs() == 1);
        drop(again);
        let mut big = Capture::start(1);
        big.body_mut().resize(KEEP_MAX + 1, 0);
        let gone = big.0;
        drop(big);
        let again: Vec<Capture> = (0..POOL).map(|i| Capture::start(i as u32)).collect();
        assert!(again.iter().all(|x| x.0 != gone || unsafe { x.0.as_ref() }.body.capacity() <= KEEP_MAX), "not kept");
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Manual benchmark for a 200-byte capture shared by four recipients. Compares pooled
    /// allocations with fresh allocation. Run with
    /// `cargo test --release capture_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn capture_cost() {
        const N: usize = 2_000_000;
        let body = [5u8; 200];
        for threads in [1usize, 4] {
            let pooled = || {
                for i in 0..N {
                    let mut c = Capture::start(i as u32);
                    c.body_mut().extend_from_slice(&body);
                    for x in c.share(4) {
                        std::hint::black_box(&x);
                    }
                }
            };
            let fresh = || {
                for i in 0..N {
                    let mut v = Vec::new();
                    v.extend_from_slice(&body);
                    let b = Box::new(Bytes { refs: AtomicUsize::new(1), id: i as u32, body: v });
                    for _ in 0..4 {
                        b.refs.fetch_add(1, Ordering::Relaxed);
                    }
                    std::hint::black_box(&b);
                    for _ in 0..5 {
                        b.refs.fetch_sub(1, Ordering::Release);
                    }
                    drop(b);
                }
            };
            for (name, f) in [("pool", &pooled as &(dyn Fn() + Sync)), ("fresh", &fresh)] {
                let t = std::time::Instant::now();
                std::thread::scope(|s| {
                    for _ in 0..threads {
                        s.spawn(f);
                    }
                });
                println!("{name} x{threads}: {:.1} ns", t.elapsed().as_nanos() as f64 / N as f64);
            }
        }
    }

    /// ns a capture that goes from thread to thread through the pool of the process: `r` receiving
    /// threads capture and hand each capture to a sending thread, which drops it - its own keep
    /// fills, the rest goes to the pool, the receivers take from it. Run by hand:
    /// `cargo test --release capture_across -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn capture_across_threads() {
        const N: usize = 1_000_000;
        let body = [5u8; 200];
        for r in [1usize, 2, 4] {
            let best = (0..3)
                .map(|_| {
                    let (tx, rx) = std::sync::mpsc::sync_channel::<Capture>(4096);
                    let t = std::time::Instant::now();
                    std::thread::scope(|s| {
                        for _ in 0..r {
                            let tx = tx.clone();
                            s.spawn(move || {
                                for i in 0..N / r {
                                    let mut c = Capture::start(i as u32);
                                    c.body_mut().extend_from_slice(&body);
                                    tx.send(c).unwrap();
                                }
                            });
                        }
                        drop(tx);
                        s.spawn(move || {
                            for c in rx {
                                drop(c);
                            }
                        });
                    });
                    t.elapsed().as_nanos() as f64 / N as f64
                })
                .fold(f64::MAX, f64::min);
            println!("across: {r} capturing, 1 dropping: {best:.1} ns");
        }
    }
}
