//! Bounded queue with sequence-numbered slots and nonblocking try operations.
//!
//! The queue follows Dmitry Vyukov's bounded MPMC design. Each slot owns a default-initialized
//! value and a sequence number that transfers access between a producer and a consumer.
//! Construction allocates the slots; subsequent queue operations reuse them.
//!
//! Choose one access mode per queue:
//!
//! * By value: [`RingBuffer::try_put`] and [`RingBuffer::try_get`] support multiple producers and
//!   consumers. A full queue returns the submitted value in `Err`.
//! * In place: producers claim, fill, and publish slots; one consumer takes, reads, and frees each
//!   slot. The unsafe accessors require exclusive ownership of the corresponding slot.
//!
//! A claimed but unpublished slot appears unavailable to consumers; a taken but unreleased slot
//! remains unavailable to producers. Clearing waits for claimed writes to finish. Publication
//! reports when the consumer needs waking, and full-queue hysteresis waits for enough slots to
//! become free before producers retry.

use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

/// Separates frequently written counters by 128 bytes to reduce false sharing across cores. The
/// second field, if any, is a word the threads that write the first read with it: in the line of the
/// first, not in 128 bytes of its own.
#[repr(align(128))]
struct CachePadded<T, U = ()>(T, U);

/// A value and its turn side by side, in one cache line.
struct Slot<T> {
    /// Twice a position: slot `i` of lap `n` is free for the put of position `p = i + n * capacity`
    /// when it equals `2p`, holds its value for the take of `p` when it equals `2p + 2`, and was
    /// given up by its producer ([`RingBuffer::cancel`]) - the consumers pass it by - when it equals
    /// `2p + 3`. The bit in the turn, not a field of its own: no 8 bytes of padding past a value of
    /// a size of 8 bytes, and the consumer learns it with the turn it loads anyway.
    sequence: AtomicU64,
    /// The value of the slot, for good: written by the producer between its claim and its publish,
    /// read by the consumer between its take and its free.
    value: UnsafeCell<T>,
}

/// No value of a slot taken: [`RingBuffer::taken`] has nothing.
const NONE: u64 = u64::MAX;

pub struct RingBuffer<T> {
    slots: Box<[Slot<T>]>,
    mask: u64,
    /// What has to be free before a producer that found the queue full claims again: a quarter.
    part: u64,
    /// The position of the next take.
    head: CachePadded<AtomicU64>,
    /// The position of the next put; and whether a producer found the queue full (a heuristic:
    /// relaxed), read by every claim before the tail.
    tail: CachePadded<AtomicU64, AtomicBool>,
    /// In place: the consumer found the queue empty and stopped - the producer of the next value is
    /// to wake it.
    idle: CachePadded<AtomicBool>,
    /// In place: the position of the slot the consumer holds, [`NONE`]: none. The consumer's alone -
    /// in the line of `head` it cost a single thread 1.5 ns a value more.
    taken: CachePadded<AtomicU64>,
}

// Values move between threads through the queue; a slot is accessed by one thread at a time, as its
// sequence number decides.
unsafe impl<T: Send> Send for RingBuffer<T> {}
unsafe impl<T: Send> Sync for RingBuffer<T> {}

impl<T: Default> RingBuffer<T> {
    /// A queue of 2<sup>`capacity_power_of_two`</sup> values, two at least: with one slot a value
    /// queued and a slot free are the same turn.
    ///
    /// # Panics
    ///
    /// If `capacity_power_of_two > 30`.
    pub fn new(capacity_power_of_two: u32) -> Self {
        assert!(capacity_power_of_two <= 30, "capacity_power_of_two must be 0..=30");
        let capacity = 1usize << capacity_power_of_two.max(1);
        let slots = (0..capacity as u64)
            .map(|i| Slot {
                sequence: AtomicU64::new(i << 1),
                value: UnsafeCell::new(T::default()),
            })
            .collect();
        Self {
            slots,
            mask: capacity as u64 - 1,
            part: (capacity as u64 >> 2).max(1),
            head: CachePadded(AtomicU64::new(0), ()),
            tail: CachePadded(AtomicU64::new(0), AtomicBool::new(false)),
            idle: CachePadded(AtomicBool::new(true), ()),
            taken: CachePadded(AtomicU64::new(NONE), ()),
        }
    }
}

impl<T> RingBuffer<T> {
    /// How many values the queue holds at most.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// How many values are in the queue: a snapshot, stale once other threads move on.
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Acquire); // before the tail: puts in between make it larger, never negative
        let tail = self.tail.0.load(Ordering::Acquire);
        if head < tail {
            (tail - head).min(self.slots.len() as u64) as usize
        } else {
            0
        }
    }

    /// No value in the queue: a snapshot.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tail.0.load(Ordering::Acquire) <= self.head.0.load(Ordering::Acquire)
    }

    /// No room in the queue: a snapshot.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() == self.slots.len()
    }

    /// The position of the next take: compared with what [`put_position`](Self::put_position)
    /// returned, it tells a producer that the consumer is to be woken.
    #[inline]
    pub fn head(&self) -> u64 {
        self.head.0.load(Ordering::Acquire)
    }

    #[inline(always)]
    fn slot(&self, pos: u64) -> &Slot<T> {
        // SAFETY: masked into range.
        unsafe { self.slots.get_unchecked((pos & self.mask) as usize) }
    }

    // by value

    /// Takes the value at the head (`mem::take`: the slot keeps a default). Never waits.
    ///
    /// `None`: the queue is empty — or the value at its head is claimed by a producer and not yet
    /// written, and that producer wakes the consumer once it is (see `put_position`).
    #[inline]
    pub fn try_get(&self) -> Option<T>
    where
        T: Default,
    {
        loop {
            let (pos, live) = self.claim_head(false, Ordering::Acquire)?;
            // SAFETY: the position is claimed: the slot is this thread's alone until it is released
            let value = unsafe { self.take_value(pos, live) };
            self.release(pos);
            if value.is_some() {
                return value;
            }
            // given up by its producer: the next
        }
    }

    /// Puts `value` at the tail. Never waits. `Err(value)`: the queue is full.
    #[inline]
    pub fn try_put(&self, value: T) -> Result<(), T> {
        self.put_position(value).map(|_| ())
    }

    /// Puts `value` at the tail, as [`try_put`](Self::try_put), and tells the position it took:
    /// when [`head`](Self::head) equals it after the put, the consumer found the queue empty or
    /// passed the value by while it was being written, and is to be woken. Never waits.
    pub fn put_position(&self, value: T) -> Result<u64, T> {
        match self.claim_tail() {
            Some(pos) => {
                let slot = self.slot(pos);
                // SAFETY: the slot is of this producer alone between the claim and the publish
                unsafe { *slot.value.get() = value };
                slot.sequence.store((pos + 1) << 1, Ordering::Release); // the value is seen with the sequence
                Ok(pos)
            }
            None => Err(value),
        }
    }

    /// Takes and drops every value there is.
    pub fn clear(&self)
    where
        T: Default,
    {
        self.clear_with(drop);
    }

    /// Takes every value there is and hands each to `on_clear`, oldest first.
    ///
    /// A slot is emptied and freed before its value is handed on: `on_clear` may panic or take its
    /// time without holding the producers up. A value it puts into this queue is taken again.
    pub fn clear_with<F: FnMut(T)>(&self, mut on_clear: F)
    where
        T: Default,
    {
        while let Some((pos, live)) = self.claim_head(true, Ordering::Acquire) {
            // SAFETY: the position is claimed: the slot is this thread's alone until it is released
            let value = unsafe { self.take_value(pos, live) };
            self.release(pos);
            if let Some(v) = value {
                on_clear(v);
            }
        }
    }

    /// The value of the claimed slot `pos` taken out; `None`: the slot was given up (not `live`).
    ///
    /// # Safety
    ///
    /// `pos` is claimed by this thread and not released yet.
    #[inline(always)]
    unsafe fn take_value(&self, pos: u64, live: bool) -> Option<T>
    where
        T: Default,
    {
        live.then(|| core::mem::take(&mut *self.slot(pos).value.get()))
    }

    // in place, one consumer

    /// A producer, any thread: `fill` writes the value into the next slot in place, and the slot is
    /// queued. `fill` returns `false`: the slot is given up, the consumer passes it by.
    ///
    /// `None`: the queue is full, or was and less than a quarter of it is free yet — `fill` is not
    /// called. `Some(true)`: the consumer sleeps and is to be woken.
    #[inline]
    pub fn put(&self, fill: impl FnOnce(&mut T) -> bool) -> Option<bool> {
        self.put_at(|v, _| fill(v))
    }

    /// As [`put`](Self::put): `fill` sees the position of the slot too - what [`taken_at`](Self::taken_at) gives the
    /// consumer of it.
    #[inline]
    pub fn put_at(&self, fill: impl FnOnce(&mut T, u64) -> bool) -> Option<bool> {
        let pos = self.claim()?;
        // a panic out of `fill` gives the slot up: one left claimed would stop the consumer at it
        // for good
        let guard = CancelOnUnwind(self, pos);
        // SAFETY: the slot is of this producer alone between the claim and the publish
        let live = fill(unsafe { self.own(pos) }, pos);
        core::mem::forget(guard);
        Some(self.done(pos, live))
    }

    /// A producer, any thread: the next slot is its own — its value is [`own`](Self::own), to be
    /// filled and [`publish`](Self::publish)ed or given up ([`cancel`](Self::cancel)). `None`: the
    /// queue is full, or was and less than a quarter of it is free yet. [`put`](Self::put) does it
    /// all in one call.
    #[inline]
    pub fn claim(&self) -> Option<u64> {
        if self.tail.1.load(Ordering::Relaxed) {
            let ahead = self.tail.0.load(Ordering::Acquire) + self.part - 1;
            if self.slot(ahead).sequence.load(Ordering::Acquire) != ahead << 1 {
                return None;
            }
            self.tail.1.store(false, Ordering::Relaxed);
        }
        let pos = self.claim_tail();
        if pos.is_none() {
            self.tail.1.store(true, Ordering::Relaxed);
        }
        pos
    }

    /// The value of the slot claimed at `pos`, to be filled in place.
    ///
    /// # Safety
    ///
    /// Only the producer that claimed `pos`, before its [`publish`](Self::publish) or
    /// [`cancel`](Self::cancel).
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn own(&self, pos: u64) -> &mut T {
        &mut *self.slot(pos).value.get()
    }

    /// The slot claimed at `pos` is filled: it is queued. `true`: the consumer sleeps and is to be
    /// woken.
    #[inline]
    pub fn publish(&self, pos: u64) -> bool {
        self.done(pos, true)
    }

    /// The producer of the slot claimed at `pos` gives it up: the consumer passes it by. `true`:
    /// the consumer sleeps and is to be woken — what is queued behind the slot waited for it.
    #[inline]
    pub fn cancel(&self, pos: u64) -> bool {
        self.done(pos, false)
    }

    #[inline]
    fn done(&self, pos: u64, live: bool) -> bool {
        let slot = self.slot(pos);
        // The consumer that sees the turn sees the value. The turn is published before the word is
        // read - the other half of the consumer's word-and-recheck in `take`: one of the two sees
        // the other, no wake is lost. All four are sequentially consistent, in one single order -
        // the swap before the consumer's recheck: the recheck sees the turn; after it: the word,
        // set before the recheck, is seen by the load below - and on x86 the swap is one `xchg`, not
        // a store and an `mfence`. The word is written only when the consumer sleeps - not a cache
        // line taken from the consumer at every value.
        slot.sequence.swap((pos + 1) << 1 | !live as u64, Ordering::SeqCst);
        self.idle.0.load(Ordering::SeqCst) && self.idle.0.swap(false, Ordering::AcqRel)
    }

    /// The consumer: takes the value queued next; it stays taken — [`taken`](Self::taken) — until
    /// [`free`](Self::free). `false`: nothing is queued, or the slot at the head is claimed and not
    /// published yet: the producer of the next value wakes the consumer.
    pub fn take(&self) -> bool {
        if self.next(Ordering::Acquire) {
            return true;
        }
        // The word and the recheck of the turns are sequentially consistent, as the swap and the
        // load in `done`: in their single order either the producer's swap comes before the
        // recheck, which sees the turn, or the word comes before the producer's load, which sees
        // it - no wake is lost, with no fence: on x86 one `xchg` and plain loads. Already set, the
        // word is not written again: the store read - of this consumer or of `clear_in_place`, a
        // sequentially consistent one read by a sequentially consistent load - is earlier in the
        // order still; a consumer polling an empty queue costs no barrier and does not take the
        // cache line of the word from the producers; a producer that cleared it meanwhile wakes
        // the consumer.
        if !self.idle.0.load(Ordering::SeqCst) {
            self.idle.0.store(true, Ordering::SeqCst);
        }
        if self.next(Ordering::SeqCst) {
            self.idle.0.store(false, Ordering::Relaxed); // only spares a producer a needless wake
            return true;
        }
        false
    }

    /// `order`: of the loads of the turns.
    #[inline(always)]
    fn next(&self, order: Ordering) -> bool {
        loop {
            let Some((pos, live)) = self.claim_head(false, order) else {
                return false;
            };
            if live {
                self.taken.0.store(pos, Ordering::Relaxed);
                return true;
            }
            self.release(pos); // given up by its producer
        }
    }

    /// The value the consumer has taken.
    ///
    /// # Safety
    ///
    /// Only the consumer, between a [`take`](Self::take) that returned `true` and
    /// [`free`](Self::free).
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn taken(&self) -> &mut T {
        &mut *self.slot(self.taken.0.load(Ordering::Relaxed)).value.get()
    }

    /// The position of the slot the consumer has taken - the one [`put_at`](Self::put_at) gave its producer.
    #[inline]
    pub fn taken_at(&self) -> u64 {
        self.taken.0.load(Ordering::Relaxed)
    }

    /// The consumer holds a value.
    #[inline]
    pub fn has_taken(&self) -> bool {
        self.taken.0.load(Ordering::Relaxed) != NONE
    }

    /// The consumer is done with what it took: the slot is free for the producers.
    #[inline]
    pub fn free(&self) {
        let pos = self.taken.0.load(Ordering::Relaxed); // the consumer's alone: no read-modify-write
        if pos != NONE {
            self.taken.0.store(NONE, Ordering::Relaxed);
            self.release(pos);
        }
    }

    /// Takes everything queued, oldest first, `on_clear` sees each in place — from any thread; the
    /// value the consumer holds is left to it. A slot claimed and not published yet is waited for:
    /// nothing is left behind when a queue is emptied for good. The producer of the next value
    /// wakes the consumer.
    pub fn clear_in_place(&self, mut on_clear: impl FnMut(&mut T)) {
        while let Some((pos, live)) = self.claim_head(true, Ordering::Acquire) {
            // the slot is given back whatever `on_clear` does - a panic out of it too: one left
            // taken would hold the queue full for good once the ring comes round to it
            let _release = Release(self, pos);
            if live {
                // SAFETY: claimed: this thread's alone until released
                on_clear(unsafe { &mut *self.slot(pos).value.get() });
            }
        }
        // emptied, and not by the consumer: the producer of the next value wakes it
        self.idle.0.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    // the turns

    /// Moves the tail on by one: the position claimed, or `None` when the queue is full. A lost
    /// race backs off before it tries again (crossbeam's `ArrayQueue` does so): producers retrying
    /// at once keep the cache line of `tail` moving between cores, and with 2 producers a value
    /// cost 4 times as much.
    #[inline]
    fn claim_tail(&self) -> Option<u64> {
        let backoff = crossbeam_utils::Backoff::new();
        let mut pos = self.tail.0.load(Ordering::Relaxed);
        loop {
            let dif = self.slot(pos).sequence.load(Ordering::Acquire).wrapping_sub(pos << 1) as i64;
            if dif == 0 {
                match self.tail.0.compare_exchange_weak(pos, pos + 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return Some(pos),
                    Err(p) => {
                        pos = p; // another producer took this position: try the next
                        backoff.spin();
                    }
                }
            } else if dif < 0 {
                return None; // the slot holds the value of the lap before, or its consumer has not released it yet
            } else {
                backoff.spin();
                pos = self.tail.0.load(Ordering::Relaxed); // another producer took this position
            }
        }
    }

    /// Moves the head on by one: the position taken and whether its slot holds a value - not given
    /// up, or `None` when there is nothing to take — empty, or (without `wait`) not yet written.
    /// `order`: of the loads of the turns.
    #[inline(always)]
    fn claim_head(&self, wait: bool, order: Ordering) -> Option<(u64, bool)> {
        let mut spins = 0u32;
        let mut pos = self.head.0.load(Ordering::Relaxed);
        loop {
            let dif = self.slot(pos).sequence.load(order).wrapping_sub((pos + 1) << 1) as i64;
            if dif as u64 <= 1 {
                // published: a value, or given up
                match self.head.0.compare_exchange_weak(pos, pos + 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return Some((pos, dif == 0)),
                    Err(p) => pos = p, // another consumer took this position: try the next
                }
            } else if dif < 0 {
                if !wait || self.tail.0.load(Ordering::Acquire) <= pos {
                    return None; // empty
                }
                // Claimed by a producer that is writing it: a few instructions away - unless the
                // scheduler took that producer off its processor, which the yield gives back.
                if spins < 64 {
                    core::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
                spins += 1;
                pos = self.head.0.load(Ordering::Relaxed);
            } else {
                pos = self.head.0.load(Ordering::Relaxed); // another consumer took this position
            }
        }
    }

    /// Frees the slot of the taken position `pos` for the put of the next lap.
    #[inline(always)]
    fn release(&self, pos: u64) {
        self.slot(pos).sequence.store((pos + self.slots.len() as u64) << 1, Ordering::Release);
        // the reads of the value come first
    }
}

/// Gives the slot `pos` up when dropped on the way out of a `fill` that panicked.
struct CancelOnUnwind<'a, T>(&'a RingBuffer<T>, u64);

impl<T> Drop for CancelOnUnwind<'_, T> {
    fn drop(&mut self) {
        self.0.done(self.1, false);
    }
}

/// Gives the slot `pos` back when dropped: on the way out of a call that may panic.
struct Release<'a, T>(&'a RingBuffer<T>, u64);

impl<T> Drop for Release<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.0.release(self.1);
    }
}

impl<T> fmt::Debug for RingBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingBuffer")
            .field("capacity", &self.capacity())
            .field("len", &self.len())
            .field("head", &self.head.0.load(Ordering::Relaxed))
            .field("tail", &self.tail.0.load(Ordering::Relaxed))
            .finish()
    }
}

/// The state of the queue for a log line.
impl<T> fmt::Display for RingBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RingBuffer{{capacity={}, head={}, tail={}}}",
            self.slots.len(),
            self.head.0.load(Ordering::Acquire),
            self.tail.0.load(Ordering::Acquire)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn in_place_order_full_and_give_up() {
        let q: RingBuffer<u32> = RingBuffer::new(2); // 4 slots
        assert_eq!(
            q.put(|v| {
                *v = 1;
                true
            }),
            Some(true)
        ); // the consumer slept
        assert_eq!(
            q.put(|v| {
                *v = 2;
                false
            }),
            Some(false)
        ); // given up
        assert_eq!(
            q.put(|v| {
                *v = 3;
                true
            }),
            Some(false)
        );
        assert_eq!(
            q.put(|v| {
                *v = 4;
                true
            }),
            Some(false)
        );
        assert_eq!(q.put(|_| true), None); // full
        let mut got = vec![];
        while q.take() {
            got.push(unsafe { *q.taken() });
            q.free();
        }
        assert_eq!(got, vec![1, 3, 4]);
        assert!(q
            .put(|v| {
                *v = 5;
                true
            })
            .is_some()); // a quarter free again
    }

    /// The steps of a put one by one: the order of the claims is the order of the queue, whatever
    /// the order of the publishes.
    #[test]
    fn claim_own_publish_cancel() {
        let q: RingBuffer<u32> = RingBuffer::new(2);
        let (a, b, c) = (q.claim().unwrap(), q.claim().unwrap(), q.claim().unwrap());
        unsafe {
            *q.own(c) = 30;
            *q.own(a) = 10;
        }
        assert!(q.publish(c), "the consumer slept from the start");
        assert!(!q.cancel(b));
        assert!(!q.take(), "the head is claimed, not published yet");
        assert!(q.publish(a), "the consumer slept");
        let mut got = vec![];
        while q.take() {
            got.push(unsafe { *q.taken() });
            q.free();
        }
        assert_eq!(got, vec![10, 30]);
        assert_eq!(q.to_string(), "RingBuffer{capacity=4, head=3, tail=3}");
    }

    #[test]
    fn many_producers_one_consumer_in_place() {
        let q: Arc<RingBuffer<u64>> = Arc::new(RingBuffer::new(6));
        let producers: Vec<_> = (0..4u64)
            .map(|p| {
                let q = q.clone();
                std::thread::spawn(move || {
                    for i in 0..10_000u64 {
                        while q
                            .put(|v| {
                                *v = p << 32 | i;
                                true
                            })
                            .is_none()
                        {
                            std::hint::spin_loop();
                        }
                    }
                })
            })
            .collect();
        let mut last = [None::<u64>; 4];
        let mut n = 0;
        while n < 40_000 {
            if q.take() {
                let v = unsafe { *q.taken() };
                q.free();
                let p = (v >> 32) as usize;
                let i = v & 0xFFFF_FFFF;
                assert!(last[p].map_or(true, |l| l < i), "the order of a producer is kept");
                last[p] = Some(i);
                n += 1;
            }
        }
        for p in producers {
            p.join().unwrap();
        }
    }

    /// A producer that pauses now and then and a consumer that sleeps whenever it finds the queue
    /// empty: every value put is taken, no wake is lost (the consumer sleeps until it is woken, a
    /// lost wake hangs the test).
    #[test]
    fn no_wake_is_lost() {
        use std::sync::atomic::AtomicBool;
        let q: Arc<RingBuffer<u64>> = Arc::new(RingBuffer::new(3));
        let woken = Arc::new(AtomicBool::new(false));
        let put = Arc::new(AtomicU64::new(0));
        let consumer = {
            let (q, woken, put) = (q.clone(), woken.clone(), put.clone());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while n < 20_000 {
                    if q.take() {
                        assert_eq!(unsafe { *q.taken() }, n, "in order");
                        q.free();
                        n += 1;
                    } else {
                        // asleep until a producer that saw `idle` wakes it: 3 s with no wake once
                        // the put of value `n` is done (its wake told) - it was lost. Before that
                        // the producer is late, not the wake: under the load of the whole run
                        // Windows left the producer off a processor for 3 s (value 2135 not put
                        // yet, the queue empty, `idle` set).
                        let mut waited = 0;
                        while !woken.swap(false, Ordering::AcqRel) {
                            std::thread::park_timeout(std::time::Duration::from_millis(100));
                            waited += 1;
                            if put.load(Ordering::SeqCst) <= n {
                                waited = 0;
                            } else if waited >= 30 {
                                let seq = q.slot(n).sequence.load(Ordering::SeqCst);
                                panic!(
                                    "a wake was lost: value {n} awaited, {} put, {q}, idle {}, sequence at the head {seq} ( published: {} )",
                                    put.load(Ordering::SeqCst),
                                    q.idle.0.load(Ordering::SeqCst),
                                    (n + 1) << 1
                                );
                            }
                        }
                    }
                }
            })
        };
        for i in 0..20_000u64 {
            loop {
                if let Some(wake) = q.put(|v| {
                    *v = i;
                    true
                }) {
                    if wake {
                        woken.store(true, Ordering::Release);
                        consumer.thread().unpark();
                    }
                    put.store(i + 1, Ordering::SeqCst); // the put and its wake done
                    break;
                }
                if consumer.is_finished() {
                    break; // the consumer failed: its panic is told by the join below
                }
                std::thread::yield_now();
            }
            if i % 97 == 0 {
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        }
        consumer.join().unwrap();
    }

    /// A panic out of `fill` gives the slot up, a panic out of `on_clear` leaves no slot taken: the
    /// queue goes on.
    #[test]
    fn panics_leave_no_slot_held() {
        let q: RingBuffer<u32> = RingBuffer::new(2); // 4 slots
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| q.put(|_| panic!("a fill of the application"))));
        assert!(r.is_err());
        q.put(|v| {
            *v = 1;
            true
        });
        assert!(q.take() && unsafe { *q.taken() } == 1, "the slot given up is passed by");
        q.free();
        q.put(|v| {
            *v = 2;
            true
        });
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| q.clear_in_place(|_| panic!("a handler of the application"))));
        assert!(r.is_err());
        for i in 0..4 {
            assert!(
                q.put(|v| {
                    *v = i;
                    true
                })
                .is_some(),
                "value {i}: the ring came round"
            );
        }
    }
}
