// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Replace the protocol session driven by an existing transport.
//!
//! The incoming session supplies its own receiver and source. The old receiver stops at the handoff
//! packet boundary and transfers already-read trailing bytes to the replacement. The source changes
//! only after its current packet is complete.
//!
//! [`SwapSink`] and [`SwapSource`] check a handoff flag on each call. [`Reseat::stop`] requests the
//! receiver stop needed for a safe boundary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::lib::adhoc::{BytesDst, BytesSrc};

/// The receiver and the source of a session that moves onto a transport connection, until they take
/// over.
#[derive(Default)]
pub struct Reseat {
    stop: Arc<AtomicBool>,
    sink: parking_lot::Mutex<Option<Box<dyn BytesDst>>>,
    source: parking_lot::Mutex<Option<Box<dyn BytesSrc>>>,
    sink_due: AtomicBool,
    source_due: AtomicBool,
}

impl Reseat {
    /// The flag the receiver the pump drives now reads behind each pack
    /// (`crate::lib::adhoc::connection::receiver::Receiver::stop`).
    pub fn stop(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// Nothing moves: the slot of the connection serves a new session.
    pub(crate) fn reset(&self) {
        self.stop.store(false, Ordering::Release);
        self.sink_due.store(false, Ordering::Release);
        self.source_due.store(false, Ordering::Release);
        let gone = (self.sink.lock().take(), self.source.lock().take());
        drop(gone);
    }

    /// The session moves here: from the next pack on, `sink` receives; `source` sends once no pack
    /// is half out. Called by the handler of the pack that moves the session, on the receiving
    /// thread.
    pub fn move_here(&self, sink: Box<dyn BytesDst>, source: Box<dyn BytesSrc>) {
        *self.sink.lock() = Some(sink);
        *self.source.lock() = Some(source);
        self.sink_due.store(true, Ordering::Release);
        self.source_due.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
    }
}

/// The receiver the pump drives, which a session that moves onto the transport replaces.
pub struct SwapSink {
    cur: Box<dyn BytesDst>,
    reseat: Arc<Reseat>,
}

impl SwapSink {
    pub fn new(cur: Box<dyn BytesDst>, reseat: Arc<Reseat>) -> Self {
        Self { cur, reseat }
    }

    /// The session moved: its receiver takes over with the bytes the one before did not parse.
    #[cold]
    fn swap(&mut self) -> bool {
        self.reseat.sink_due.store(false, Ordering::Release);
        let Some(next) = self.reseat.sink.lock().take() else { return true };
        let tail = self.cur.take_tail();
        self.cur = next;
        tail.is_empty() || self.cur.on_bytes(&tail)
    }

    /// The session moved here between two parses (a reclaim on the connect of the socket, not in
    /// the handler of a pack): no parse stopped behind a pack, so the stop is still up - the
    /// receiver of the session takes over before the first byte, else the one before would parse
    /// what the peer said to the session that moved here.
    #[inline]
    fn moved_between(&mut self) -> bool {
        if self.reseat.sink_due.load(Ordering::Acquire) && self.reseat.stop.swap(false, Ordering::AcqRel) {
            return self.swap();
        }
        true
    }
}

impl BytesDst for SwapSink {
    /// All the bytes, or false: what the sink before the move did not parse goes to the one after
    /// it.
    fn on_bytes(&mut self, mut bytes: &[u8]) -> bool {
        loop {
            let Some(n) = self.on_part(bytes) else { return false };
            bytes = &bytes[n..];
            if bytes.is_empty() {
                return true;
            }
            if n == 0 {
                return false; // refused: a caller of all-or-nothing cannot keep the rest
            }
        }
    }
    #[inline]
    fn on_bytes_mut(&mut self, bytes: &mut [u8]) -> bool {
        self.on_bytes(bytes)
    }
    fn on_block(&mut self, block: &super::buf::RxBuf, at: usize, end: usize) -> Option<usize> {
        if !self.moved_between() {
            return None;
        }
        let n = self.cur.on_block(block, at, end)?;
        if self.reseat.sink_due.load(Ordering::Acquire) && !self.swap() {
            return None;
        }
        Some(n) // the rest stays with the pump: it offers it to the new sink
    }
    /// As [`on_block`](Self::on_block): what is refused stays with the transport.
    fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
        if !self.moved_between() {
            return None;
        }
        let n = self.cur.on_slice(bytes)?;
        if self.reseat.sink_due.load(Ordering::Acquire) && !self.swap() {
            return None;
        }
        Some(n)
    }
    fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
        if !self.moved_between() {
            return None;
        }
        let n = self.cur.on_part(bytes)?;
        if self.reseat.sink_due.load(Ordering::Acquire) && !self.swap() {
            return None;
        }
        Some(n)
    }
    /// As [`on_part`](Self::on_part): the bytes of a record in the clear.
    fn on_clear(&mut self, bytes: &[u8]) -> Option<usize> {
        if !self.moved_between() {
            return None;
        }
        let n = self.cur.on_clear(bytes)?;
        if self.reseat.sink_due.load(Ordering::Acquire) && !self.swap() {
            return None;
        }
        Some(n)
    }
    #[inline]
    fn graceful_close(&self) -> bool {
        self.cur.graceful_close()
    }
    #[inline]
    fn broken(&self) -> bool {
        self.cur.broken()
    }
    #[inline]
    fn wants_more(&self) -> bool {
        self.cur.wants_more()
    }
    fn retry(&mut self) {
        self.cur.retry();
        if self.reseat.sink_due.load(Ordering::Acquire) {
            self.swap();
        }
    }
    #[inline]
    fn take_tail(&mut self) -> Vec<u8> {
        self.cur.take_tail()
    }
    #[inline]
    fn suspend(&mut self) {
        self.cur.suspend()
    }
    #[inline]
    fn lost(&mut self, position: i64) {
        self.cur.lost(position)
    }
    #[inline]
    fn abandon(&mut self) {
        self.cur.abandon()
    }
    #[inline]
    fn continue_(&mut self) {
        self.cur.continue_()
    }
    #[inline]
    fn received(&self) -> i64 {
        self.cur.received()
    }
    #[inline]
    fn idle(&self) -> bool {
        self.cur.idle()
    }
}

/// The source the pump drives, which a session that moves onto the transport replaces at a pack
/// boundary.
pub struct SwapSource {
    cur: Box<dyn BytesSrc>,
    reseat: Arc<Reseat>,
}

impl SwapSource {
    pub fn new(cur: Box<dyn BytesSrc>, reseat: Arc<Reseat>) -> Self {
        Self { cur, reseat }
    }
}

impl BytesSrc for SwapSource {
    fn read(&mut self, dst: &mut [u8]) -> isize {
        if self.reseat.source_due.load(Ordering::Acquire) && self.cur.at_boundary() {
            self.reseat.source_due.store(false, Ordering::Release);
            if let Some(next) = self.reseat.source.lock().take() {
                self.cur = next;
            }
        }
        self.cur.read(dst)
    }
    #[inline]
    fn clear(&self) -> bool {
        self.cur.clear()
    }
    #[inline]
    fn at_boundary(&self) -> bool {
        self.cur.at_boundary()
    }
    #[inline]
    fn journal(&self) -> Option<&crate::lib::adhoc::connection::transmitter::resumable::Journal> {
        self.cur.journal()
    }
    #[inline]
    fn holds_journal(&mut self) -> bool {
        self.cur.holds_journal()
    }
    #[inline]
    fn with_journal(&mut self, f: &mut dyn FnMut(&mut crate::lib::adhoc::connection::transmitter::resumable::Resumable)) -> bool {
        self.cur.with_journal(f)
    }
    #[inline]
    fn online(&self) {
        self.cur.online()
    }
    #[inline]
    fn turn(&mut self, position: i64) -> std::io::Result<i64> {
        self.cur.turn(position)
    }
    #[inline]
    fn abandon(&mut self) {
        self.cur.abandon()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::adhoc::connection::receiver::Receiver;
    use crate::lib::adhoc::connection::receiver::{Rx, RxHost, RxState};
    use std::sync::Mutex;

    /// Packs of a fixed size: `[id][3 bytes]`. The pack with id 9 moves the session.
    struct Host {
        name: u8,
        got: Arc<Mutex<Vec<(u8, u8, [u8; 3])>>>,
        on_move: Option<Box<dyn Fn() + Send>>,
        id: u8,
        body: [u8; 3],
        at: usize,
    }

    impl RxHost for Host {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            self.id = id as u8;
            self.at = 0;
            true
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            while self.at < 3 {
                if src.byte_max <= src.byte_ {
                    return false;
                }
                self.body[self.at] = src.buffer[src.byte_];
                src.byte_ += 1;
                self.at += 1;
            }
            true
        }
        fn on_received(&mut self, _st: &mut RxState) {
            self.got.lock().unwrap().push((self.name, self.id, self.body));
            if self.id == 9 {
                if let Some(f) = &self.on_move {
                    f();
                }
            }
        }
    }

    struct Src(u8, bool);
    impl BytesSrc for Src {
        fn read(&mut self, dst: &mut [u8]) -> isize {
            dst[0] = self.0;
            1
        }
        fn at_boundary(&self) -> bool {
            self.1
        }
    }

    #[test]
    fn the_session_takes_over_behind_its_pack() {
        for step in [1usize, 2, 5, 100] {
            let reseat = Arc::new(Reseat::default());
            let got = Arc::new(Mutex::new(vec![]));
            let r = reseat.clone();
            let g = got.clone();
            let mover = move || {
                let old = crate::lib::adhoc::connection::receiver::Receiver::new(
                    Host {
                        name: b'B',
                        got: g.clone(),
                        on_move: None,
                        id: 0,
                        body: [0; 3],
                        at: 0,
                    },
                    1,
                )
                .stop(r.stop());
                r.move_here(Box::new(old), Box::new(Src(b'b', true)));
            };
            let new = crate::lib::adhoc::connection::receiver::Receiver::new(
                Host {
                    name: b'A',
                    got: got.clone(),
                    on_move: Some(Box::new(mover)),
                    id: 0,
                    body: [0; 3],
                    at: 0,
                },
                1,
            )
            .stop(reseat.stop());
            let mut sink = SwapSink::new(Box::new(new), reseat.clone());
            let wire: Vec<u8> = [[1u8, 1, 1, 1], [9, 9, 9, 9], [2, 2, 2, 2], [3, 3, 3, 3]].concat();
            for w in wire.chunks(step) {
                assert!(sink.on_bytes(w));
            }
            let got = got.lock().unwrap().clone();
            assert_eq!(got, vec![(b'A', 1, [1; 3]), (b'A', 9, [9; 3]), (b'B', 2, [2; 3]), (b'B', 3, [3; 3])], "step {step}");
        }
    }

    #[test]
    fn the_source_takes_over_at_a_boundary() {
        let reseat = Arc::new(Reseat::default());
        let mut source = SwapSource::new(Box::new(Src(b'a', false)), reseat.clone());
        reseat.move_here(
            Box::new(crate::lib::adhoc::connection::receiver::Receiver::new(
                Host {
                    name: 0,
                    got: Default::default(),
                    on_move: None,
                    id: 0,
                    body: [0; 3],
                    at: 0,
                },
                1,
            )),
            Box::new(Src(b'b', true)),
        );
        let mut out = vec![];
        let mut one = |source: &mut SwapSource, out: &mut Vec<u8>| {
            let mut b = [0u8; 1];
            if 0 < source.read(&mut b) {
                out.push(b[0]);
            }
        };
        one(&mut source, &mut out); // a pack half out: not yet
        source.cur = Box::new(Src(b'a', true));
        one(&mut source, &mut out);
        one(&mut source, &mut out);
        assert_eq!(out, b"abb");
    }
}
