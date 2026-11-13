// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Guaranteed packs across a lost socket: the journal, the packs the two sides agree by, the
//! position.

use super::*;
use crate::lib::adhoc::connection::control::{Control, Heard, CONFIRM, CONTINUE, LOST};
use crate::lib::adhoc::connection::receiver::{BytesDst, Position, Rx};
use crate::lib::adhoc::connection::receiver::{RxHost, RxState};
use crate::lib::adhoc::connection::transmitter::{BytesSrc, Tx};
use crate::lib::adhoc::BytesSrc as _; // the methods of the source of a transport: the pack-level BytesSrc has the name
use std::collections::VecDeque;
use std::sync::Mutex;

/// The id of the first of the packs the two sides agree by (Confirm_ … Lost_): the generator gives
/// them the ids after the packs of the description.
const SVC: u32 = 9;

/// The bytes of a position as it travels.
const W: usize = 4;

/// The control of a side: positions of `W` bytes, a step of 4 KB.
fn control() -> Control {
    Control::new(W, 1 << 12, -1, 0)
}

/// The position a pack of the agreement carries (Continue_, Lost_): what the generated
/// `service_bytes` does.
fn service_bytes(service: u32, value: &mut u64, src: &mut Rx<'_>) -> bool {
    if service != CONTINUE + 1 && service != LOST + 1 {
        return true;
    }
    loop {
        match src.state() {
            0 => {
                if !src.try_get8(W as i32, 1) {
                    return false;
                }
                src.set_state(1);
            }
            _ => {
                *value = src.u8;
                return true;
            }
        }
    }
}

/// The string of a pack whose producer has nothing now (a stream source that would block): it
/// waits, held.
const HOLD: &str = "\u{0}hold";

/// A pack: id 7 — guaranteed, id 8 — not; a number and a string.
#[derive(Clone, Debug, PartialEq, Default)]
struct P {
    id: u8,
    n: u32,
    s: String,
}

impl BytesSrc for P {
    fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
        loop {
            match dst.state() {
                0 => {
                    if !dst.put_val(self.id as u64, 1, 1) {
                        return false;
                    }
                    dst.set_state(1);
                }
                1 => {
                    if !dst.put_uint_c(self.n, 2) {
                        return false;
                    }
                    dst.set_state(2);
                }
                2 if self.s == HOLD => {
                    dst.hold(); // its producer has nothing now: the pack waits for it
                    return false;
                }
                2 => return dst.put_string(&self.s, u32::MAX, 2),
                _ => return true,
            }
        }
    }
}

impl BytesDst for P {
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        loop {
            match src.state() {
                0 => {
                    if !src.has_4bytes(1) {
                        return false;
                    }
                    self.n = src.get_uint();
                    src.set_state(2);
                }
                1 => {
                    self.n = src.get_uint_();
                    src.set_state(2);
                }
                2 => {
                    if !src.try_get_string(100_000, 3) {
                        return false;
                    }
                    src.set_state(3);
                }
                _ => {
                    self.s = src.get_string().unwrap_or_default();
                    return true;
                }
            }
        }
    }
}

/// The sending side: its queue, the pack being sent, the journal, the control.
struct Sender {
    q: Mutex<(VecDeque<P>, Option<P>)>,
    journal: Journal,
    control: Control,
    ext: Option<Arc<Ext>>,
    /// The socket is gone, the session waits for the next ([`Parked`]).
    parked: std::sync::atomic::AtomicBool,
}

impl Session for Sender {
    fn journal(&self) -> Option<&Journal> {
        Some(&self.journal)
    }
    fn control(&self) -> &Control {
        &self.control
    }
    fn ext_connection(&self) -> Option<Arc<dyn crate::lib::adhoc::connection::external::External>> {
        if self.parked.load(std::sync::atomic::Ordering::Relaxed) {
            return Some(Arc::new(Parked));
        }
        self.ext.clone().map(|e| e as Arc<dyn crate::lib::adhoc::connection::external::External>)
    }
    fn service(&self, kind: u32, value: u64, out: &mut [u8]) -> usize {
        let n = if kind == CONTINUE || kind == LOST { W } else { 0 };
        out[0] = (SVC + kind) as u8;
        out[1..1 + n].copy_from_slice(&value.to_le_bytes()[..n]);
        1 + n
    }
    fn service_room(&self) -> usize {
        1 + W
    }
}

/// The transport of the sender: what it was told.
#[derive(Default)]
struct Ext {
    failures: Mutex<Vec<String>>,
    aborted: std::sync::atomic::AtomicBool,
}

impl crate::lib::adhoc::connection::external::External for Ext {
    fn is_open(&self) -> bool {
        true
    }
    fn wake_transmit(&self) {}
    fn receive_timeout(&self) -> i32 {
        0
    }
    fn set_receive_timeout(&self, _ms: i32) {}
    fn transmit_timeout(&self) -> i32 {
        0
    }
    fn set_transmit_timeout(&self, _ms: i32) {}
    fn abort(&self) {
        self.aborted.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn failure(&self, e: &(dyn std::error::Error + 'static)) {
        self.failures.lock().unwrap().push(e.to_string());
    }
}

impl TxHost for &Sender {
    fn on_serializing(&mut self) -> bool {
        let mut q = self.q.lock().unwrap();
        let p = q.0.pop_front();
        q.1 = p;
        q.1.is_some()
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let q = self.q.lock().unwrap();
        q.1.as_ref().unwrap().get_bytes(dst)
    }
    fn on_serialized(&mut self) {
        self.q.lock().unwrap().1 = None;
    }
    fn guaranteed(&self) -> bool {
        self.q.lock().unwrap().1.as_ref().map_or(false, |p| p.id == 7)
    }
}

/// The receiving side: the packs it got, its position, its control (what it tells the sender).
struct Receiver {
    p: P,
    id: u32,
    service: u32,
    value: u64,
    got: Vec<P>,
    position: Position,
    control: Control,
    lost: Option<i64>,
}

impl RxHost for Receiver {
    fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool {
        if (SVC..SVC + 5).contains(&id) {
            self.service = id - SVC + 1;
            return true;
        }
        self.id = id;
        self.p = P { id: id as u8, ..P::default() };
        if id == 7 {
            self.position.start(src.pack_at(), id);
        }
        id == 7 || id == 8
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        if self.service != 0 {
            service_bytes(self.service, &mut self.value, src)
        } else {
            self.p.put_bytes(src)
        }
    }
    fn on_received(&mut self, st: &mut RxState) {
        if self.service != 0 {
            let kind = self.service - 1;
            self.service = 0;
            let named = self.position.named();
            match self.control.heard(kind, self.value, || named, false) {
                Heard::Lost(at) => {
                    self.position.position(at); // the stream of the sender goes on from there
                    self.lost = Some(at);
                }
                Heard::Ok(at) => self.position.counted_from(at),
                Heard::Nothing => {}
            }
            return;
        }
        self.position.whole(st.consumed());
        self.got.push(std::mem::take(&mut self.p));
    }
    fn on_window(&mut self, st: &mut RxState) {
        let b = self.position.due(st.consumed(), self.control.shift());
        if 0 < b {
            self.control.confirm(b);
        }
    }
}

/// What the receiver says, as the sender hears it.
fn tell(receiver: &Receiver, sender: &Sender) {
    while let Some((kind, value)) = receiver.control.next(false) {
        sender.control.heard(kind, value, || -1, false);
    }
}

fn packs(n: u32) -> Vec<P> {
    (0..n)
        .map(|i| P {
            id: if i % 3 == 2 { 8 } else { 7 },
            n: i,
            s: "x".repeat((i as usize * 97) % 3000),
        })
        .collect()
}

#[test]
fn guaranteed_across_a_lost_socket() {
    for cut_at in [1usize, 100, 5000, 40_000, 77_777] {
        let dir = std::env::temp_dir().join(format!("adhoc-session-test-{}-{cut_at}", std::process::id()));
        let sender = Arc::new(Sender {
            q: Mutex::new((packs(120).into(), None)),
            journal: Journal::new(Resumable::with_dir(100 << 20, dir)),
            control: control(),
            ext: None,
            parked: Default::default(),
        });
        let mut receiver = Receiver {
            p: P::default(),
            id: 0,
            service: 0,
            value: 0,
            got: vec![],
            position: Position::default(),
            control: control(),
            lost: None,
        };

        // the first connection: the receiver has no position, the sender tells it where its journal
        // starts
        receiver.control.continue_from(receiver.position.received(), false);
        tell(&receiver, &sender);
        let mut source = ResumableSource::new(sender.clone());
        let mut wire = Vec::new();
        loop {
            let mut out = Vec::with_capacity(1000);
            source.try_drain(&mut out);
            if out.is_empty() {
                break;
            }
            wire.extend_from_slice(&out);
        }
        // the socket is lost part-way
        let mut rx = RxState::new(1);
        let cut = cut_at.min(wire.len());
        for w in wire[..cut].chunks(333) {
            assert!(0 <= rx.write_all(w, &mut receiver));
            tell(&receiver, &sender); // the confirmations free the journal
        }
        assert!(cut < 9 || receiver.lost.is_some(), "cut at {cut_at}: the receiver was told where the stream starts");
        receiver.position.drop_pack();
        sender.control.silent();
        receiver.control.silent();
        drop(source); // the slot of the socket is reaped: its source lets the journal go

        // the next connection: the receiver names its position, the sender replays from it
        receiver.control.continue_from(receiver.position.received(), false);
        tell(&receiver, &sender);
        let mut source = ResumableSource::new(sender.clone());
        let mut rx = RxState::new(1);
        loop {
            let mut out = Vec::with_capacity(700);
            source.try_drain(&mut out);
            if out.is_empty() {
                break;
            }
            assert!(0 <= rx.write_all(&out, &mut receiver));
            tell(&receiver, &sender);
        }
        let guaranteed: Vec<u32> = receiver.got.iter().filter(|p| p.id == 7).map(|p| p.n).collect();
        let expected: Vec<u32> = packs(120).iter().filter(|p| p.id == 7).map(|p| p.n).collect();
        assert_eq!(guaranteed, expected, "cut at {cut_at}: every guaranteed pack once, in order");
        for p in &receiver.got {
            assert_eq!(p.s.len(), (p.n as usize * 97) % 3000, "cut at {cut_at}");
        }
        // what is confirmed is given back: no more than a step is left unconfirmed
        let mut out = Vec::with_capacity(100);
        source.try_drain(&mut out);
        let j = unsafe { sender.journal.get() };
        assert!(j.next() - j.acked() < 1 << 12, "cut at {cut_at}: the confirmations gave the journal back");
    }
}

/// The socket is lost while a guaranteed pack is half out of the source: the next socket of the
/// session finishes it into the journal, the replay brings it whole - every guaranteed pack once,
/// in order. A pack of no guarantee it cut is lost.
#[test]
fn a_pack_the_lost_socket_cut_goes_on_with_the_next() {
    for stop in [1usize, 2, 3, 40, 700, 2999, 6000] {
        let dir = std::env::temp_dir().join(format!("adhoc-session-cut-{}-{stop}", std::process::id()));
        let sender = Arc::new(Sender {
            q: Mutex::new((packs(30).into(), None)),
            journal: Journal::new(Resumable::with_dir(100 << 20, dir)),
            control: control(),
            ext: None,
            parked: Default::default(),
        });
        let mut receiver = Receiver {
            p: P::default(),
            id: 0,
            service: 0,
            value: 0,
            got: vec![],
            position: Position::default(),
            control: control(),
            lost: None,
        };
        receiver.control.continue_from(receiver.position.received(), false);
        tell(&receiver, &sender);
        let mut source = ResumableSource::new(sender.clone());
        let mut rx = RxState::new(1);
        let mut out = Vec::new();
        while out.len() < stop {
            let mut b = [0u8; 16];
            let n = source.read(&mut b);
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&b[..n as usize]);
        }
        assert!(0 <= rx.write_all(&out, &mut receiver));
        drop(source); // the socket is gone, the pack in the middle
        receiver.position.drop_pack();
        sender.control.silent();
        receiver.control.silent();

        receiver.control.continue_from(receiver.position.received(), false);
        tell(&receiver, &sender);
        let mut source = ResumableSource::new(sender.clone());
        let mut rx = RxState::new(1);
        loop {
            let mut out = Vec::with_capacity(700);
            source.try_drain(&mut out);
            if out.is_empty() {
                break;
            }
            assert!(0 <= rx.write_all(&out, &mut receiver), "stop {stop}: {:?}", rx.error);
            tell(&receiver, &sender);
        }
        let guaranteed: Vec<u32> = receiver.got.iter().filter(|p| p.id == 7).map(|p| p.n).collect();
        let expected: Vec<u32> = packs(30).iter().filter(|p| p.id == 7).map(|p| p.n).collect();
        assert_eq!(guaranteed, expected, "stop {stop}: every guaranteed pack once, in order");
        for p in &receiver.got {
            assert_eq!(p.s.len(), (p.n as usize * 97) % 3000, "stop {stop}");
        }
    }
}

/// The session ends with a guaranteed pack half out - the closing event before the end of the
/// socket, or after it: the pack is of the session over, nothing of it waits for the next session,
/// which starts on a pack boundary.
#[test]
fn a_pack_cut_by_the_end_of_the_session_is_not_kept() {
    for event_first in [true, false] {
        let dir = std::env::temp_dir().join(format!("adhoc-session-over-{}-{event_first}", std::process::id()));
        let sender = Arc::new(Sender {
            q: Mutex::new((packs(30).into(), None)),
            journal: Journal::new(Resumable::with_dir(100 << 20, dir)),
            control: control(),
            ext: None,
            parked: Default::default(),
        });
        let receiver = Receiver {
            p: P::default(),
            id: 0,
            service: 0,
            value: 0,
            got: vec![],
            position: Position::default(),
            control: control(),
            lost: None,
        };
        receiver.control.continue_from(receiver.position.received(), false);
        tell(&receiver, &sender);
        let mut source = ResumableSource::new(sender.clone());
        let mut b = [0u8; 16];
        while !source.tx.is_open() || !source.tx.guaranteed {
            assert!(0 < source.read(&mut b), "a guaranteed pack goes out");
        }
        if event_first {
            sender.journal.end(); // the generated closing event
            drop(source);
        } else {
            drop(source);
            sender.journal.end();
        }
        assert!(unsafe { sender.journal.get() }.kept().is_none(), "event first {event_first}: nothing waits for the next session");
    }
}

/// The journal cannot write: the session's transport hears why and is aborted - the connection does
/// not go silent; the journal is emptied, so the peer is told its position is gone on the next
/// socket.
#[test]
fn a_failed_journal_aborts_the_socket() {
    let file = std::env::temp_dir().join(format!("adhoc-session-test-{}-not-a-dir", std::process::id()));
    std::fs::write(&file, b"x").unwrap(); // its segments go under a file: they cannot be made
    let ext = Arc::new(Ext::default());
    let sender = Arc::new(Sender {
        q: Mutex::new((packs(30).into(), None)),
        journal: Journal::new(Resumable::with_dir(100 << 20, file.join("j"))),
        control: control(),
        ext: Some(ext.clone()),
        parked: Default::default(),
    });
    let receiver = Receiver {
        p: P::default(),
        id: 0,
        service: 0,
        value: 0,
        got: vec![],
        position: Position::default(),
        control: control(),
        lost: None,
    };
    receiver.control.continue_from(receiver.position.received(), false);
    tell(&receiver, &sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut out = Vec::with_capacity(1 << 20);
    source.try_drain(&mut out);
    let _ = std::fs::remove_file(&file);
    assert!(ext.aborted.load(std::sync::atomic::Ordering::Relaxed), "the socket is aborted");
    assert_eq!(ext.failures.lock().unwrap().len(), 1, "why, once");
    assert!(out.len() < packs(30).iter().map(|p| p.s.len()).sum::<usize>(), "the stream stops at the failure");
}

/// The capacity of the journal holds a pack in the middle too: what is not confirmed never grows
/// past it - a file below what is confirmed waits to be whole, it is not held for the peer; the
/// confirmations of the peer let the rest through, every guaranteed pack once, in order.
#[test]
fn the_cap_holds_a_pack_in_the_middle() {
    const CAP: i64 = 16 << 10;
    let dir = std::env::temp_dir().join(format!("adhoc-session-test-{}-cap", std::process::id()));
    let sender = Arc::new(Sender {
        q: Mutex::new((packs(60).into(), None)),
        journal: Journal::new(Resumable::with_dir(CAP, dir)),
        control: control(),
        ext: None,
        parked: Default::default(),
    });
    let mut receiver = Receiver {
        p: P::default(),
        id: 0,
        service: 0,
        value: 0,
        got: vec![],
        position: Position::default(),
        control: control(),
        lost: None,
    };
    receiver.control.continue_from(receiver.position.received(), false);
    tell(&receiver, &sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut rx = RxState::new(1);
    let mut b = vec![0u8; 64 << 10];
    let mut idle = 0;
    while idle < 3 {
        let n = source.read(&mut b);
        {
            let j = unsafe { sender.journal.get() };
            assert!(j.next() - j.acked() <= CAP, "what is not confirmed, {}, is past the cap", j.next() - j.acked());
        }
        if n <= 0 {
            idle += 1;
            continue;
        }
        idle = 0;
        assert!(0 <= rx.write_all(&b[..n as usize], &mut receiver), "{:?}", rx.error);
        tell(&receiver, &sender);
    }
    let guaranteed: Vec<u32> = receiver.got.iter().filter(|p| p.id == 7).map(|p| p.n).collect();
    let expected: Vec<u32> = packs(60).iter().filter(|p| p.id == 7).map(|p| p.n).collect();
    assert_eq!(guaranteed, expected);
}

/// The transport of a parked session: closed, the session waits for the next socket.
struct Parked;

impl crate::lib::adhoc::connection::external::External for Parked {
    fn is_open(&self) -> bool {
        false
    }
    fn wake_transmit(&self) {}
    fn receive_timeout(&self) -> i32 {
        i32::MIN
    }
    fn set_receive_timeout(&self, _ms: i32) {}
    fn transmit_timeout(&self) -> i32 {
        0
    }
    fn set_transmit_timeout(&self, _ms: i32) {}
    fn abort(&self) {}
    fn is_parked(&self) -> bool {
        true
    }
}

/// A sender with `queued` and a journal of `cap`, and a receiver that names no position yet.
fn parked_pair(cap: i64, name: &str, queued: Vec<P>) -> (Arc<Sender>, Receiver) {
    let dir = std::env::temp_dir().join(format!("adhoc-session-offline-{}-{name}", std::process::id()));
    let sender = Arc::new(Sender {
        q: Mutex::new((queued.into(), None)),
        journal: Journal::new(Resumable::with_dir(cap, dir)),
        control: control(),
        ext: None,
        parked: Default::default(),
    });
    let receiver = Receiver {
        p: P::default(),
        id: 0,
        service: 0,
        value: 0,
        got: vec![],
        position: Position::default(),
        control: control(),
        lost: None,
    };
    (sender, receiver)
}

/// The next socket of the session: the receiver names its position, everything the source gives is
/// received.
fn reconnect(sender: &Arc<Sender>, receiver: &mut Receiver) {
    sender.parked.store(false, std::sync::atomic::Ordering::Relaxed);
    receiver.position.drop_pack();
    sender.control.silent();
    receiver.control.silent();
    receiver.control.continue_from(receiver.position.received(), false);
    tell(receiver, sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut rx = RxState::new(1);
    let mut idle = 0;
    while idle < 3 {
        let mut out = Vec::with_capacity(700);
        source.try_drain(&mut out);
        if out.is_empty() {
            idle += 1;
        } else {
            idle = 0;
            assert!(0 <= rx.write_all(&out, receiver), "{:?}", rx.error);
        }
        tell(receiver, sender);
    }
}

/// The socket is lost with packs queued and a guaranteed one half out; more are queued while the
/// session is parked. Who queues them journals them: the queue is empty while parked, a pack of no
/// guarantee is not kept; the next socket replays the journal - every guaranteed pack once, in
/// order, the one cut too.
#[test]
fn a_parked_session_journals_what_is_queued() {
    for stop in [0usize, 3, 700, 6000] {
        let (sender, mut receiver) = parked_pair(100 << 20, &format!("q{stop}"), packs(30));
        receiver.control.continue_from(receiver.position.received(), false);
        tell(&receiver, &sender);
        let mut source = ResumableSource::new(sender.clone());
        let mut rx = RxState::new(1);
        let mut out = Vec::new();
        while out.len() < stop {
            let mut b = [0u8; 16];
            let n = source.read(&mut b);
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&b[..n as usize]);
        }
        assert!(0 <= rx.write_all(&out, &mut receiver));
        sender.parked.store(true, std::sync::atomic::Ordering::Relaxed); // the socket is gone: the session parks
        drop(source); // its end journals what the socket left queued
        assert!(sender.q.lock().unwrap().0.is_empty(), "stop {stop}: the queue is journalled at the end of the socket");
        let backlog = unsafe { sender.journal.get() }.backlog();
        for p in packs(60).into_iter().skip(30) {
            sender.q.lock().unwrap().0.push_back(p);
            journal_queue(&*sender); // what the hook of the parked transport does
        }
        assert!(sender.q.lock().unwrap().0.is_empty(), "stop {stop}: what is queued while parked is journalled");
        assert!(backlog < unsafe { sender.journal.get() }.backlog(), "stop {stop}: the journal took them");

        reconnect(&sender, &mut receiver);
        let guaranteed: Vec<u32> = receiver.got.iter().filter(|p| p.id == 7).map(|p| p.n).collect();
        let expected: Vec<u32> = packs(60).iter().filter(|p| p.id == 7).map(|p| p.n).collect();
        assert_eq!(guaranteed, expected, "stop {stop}: every guaranteed pack once, in order");
        assert!(receiver.got.iter().all(|p| p.id == 7 || p.n < 30), "stop {stop}: none of no guarantee queued while parked");
        for p in &receiver.got {
            assert_eq!(p.s.len(), (p.n as usize * 97) % 3000, "stop {stop}");
        }
    }
}

/// Parked, the journal takes no more than its cap: the rest waits in the queue, the pack the cap
/// cut is finished by the next socket - every guaranteed pack once, in order.
#[test]
fn a_parked_session_journals_up_to_the_cap() {
    const CAP: i64 = 24 << 10;
    let (sender, mut receiver) = parked_pair(CAP, "cap", vec![]);
    receiver.control.continue_from(receiver.position.received(), false);
    tell(&receiver, &sender);
    let source = ResumableSource::new(sender.clone());
    sender.parked.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(source); // a socket came and went: the session is parked
    let queued: Vec<P> = packs(60).into_iter().filter(|p| p.id == 7).collect();
    for p in queued.iter().cloned() {
        sender.q.lock().unwrap().0.push_back(p);
        journal_queue(&*sender);
        let j = unsafe { sender.journal.get() };
        assert!(j.next() - j.base() <= CAP, "the backlog {} is past the cap", j.next() - j.base());
    }
    assert!(!sender.q.lock().unwrap().0.is_empty(), "past the cap the packs wait in the queue");
    reconnect(&sender, &mut receiver);
    let got: Vec<u32> = receiver.got.iter().map(|p| p.n).collect();
    assert_eq!(got, queued.iter().map(|p| p.n).collect::<Vec<u32>>(), "every one once, in order");
}

/// A reclaim moved the session: the source of the socket before ends after the new one took the
/// queue - it says nothing of the journal, and what is queued stays the new source's to send.
#[test]
fn a_source_a_newer_replaced_leaves_the_queue_to_it() {
    let (sender, receiver) = parked_pair(100 << 20, "reclaim", vec![]);
    receiver.control.continue_from(receiver.position.received(), false);
    tell(&receiver, &sender);
    let old = ResumableSource::new(sender.clone());
    let mut new = ResumableSource::new(sender.clone());
    assert!(!new.holds_journal(), "the source before holds the journal");
    drop(old);
    assert!(new.holds_journal(), "the source before let it go");
    sender.q.lock().unwrap().0.extend(packs(3));
    journal_queue(&*sender);
    assert_eq!(sender.q.lock().unwrap().0.len(), 3, "the queue is the new source's");
}

/// Sends every pack of the queue on a first socket; the receiver gets none of them.
fn sent_all(sender: &Arc<Sender>, receiver: &Receiver) -> ResumableSource<Sender> {
    receiver.control.continue_from(receiver.position.received(), false);
    tell(receiver, sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut out = Vec::new();
    source.try_drain(&mut out);
    source
}

/// A Continue_ the peer said on a socket that went before the transmit applied it is of that socket
/// only: the next socket sends nothing guaranteed before the Continue_ of its own peer - else it
/// would replay from where the peer stood before, what the peer has already.
#[test]
fn a_continue_the_lost_socket_did_not_apply_is_forgotten() {
    let (sender, receiver) = parked_pair(100 << 20, "stale", packs(30));
    let source = sent_all(&sender, &receiver);
    let base = unsafe { sender.journal.get() }.base();
    sender.control.heard(CONTINUE, base as u64 & 0xFFFF_FFFF, || -1, false); // said, not applied yet
    sender.parked.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(source);
    sender.parked.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut source = ResumableSource::new(sender.clone());
    let mut out = Vec::new();
    source.try_drain(&mut out);
    assert!(out.is_empty(), "{} bytes went out before the peer of this socket named its position", out.len());
}

/// The session parks: its journal goes to the disk and its buffer grows no more in memory.
#[test]
fn a_parked_session_parks_its_journal() {
    let (sender, receiver) = parked_pair(100 << 20, "park", packs(30));
    let mut source = sent_all(&sender, &receiver);
    sender.control.online(); // the session goes on on the socket (the generated `resume`): at the next fill
    source.try_drain(&mut Vec::new());
    assert!(!unsafe { sender.journal.get() }.is_parked());
    sender.parked.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(source);
    assert!(unsafe { sender.journal.get() }.is_parked(), "the journal of a parked session is parked");
}

/// While the source holds (no position named yet) a confirmation of the peer is taken: what the
/// peer has is given back
///  - else the confirmation is lost and the backlog never shrinks.
#[test]
fn a_confirmation_while_holding_is_taken() {
    let (sender, receiver) = parked_pair(100 << 20, "report", packs(30));
    let source = sent_all(&sender, &receiver);
    drop(source);
    let mut source = ResumableSource::new(sender.clone()); // holds: the peer of this socket named no position
    let (base, next) = {
        let j = unsafe { sender.journal.get() };
        (j.base(), j.next())
    };
    for _ in 0..(next >> 12) - (base >> 12) {
        sender.control.heard(CONFIRM, 0, || -1, false); // a step more: from where the journal answered
    }
    let mut out = Vec::new();
    source.try_drain(&mut out);
    assert_eq!(unsafe { sender.journal.get() }.acked(), next >> 12 << 12, "the confirmations are taken");
}

/// The peer names again the position the replay in progress started from (a Continue_ said twice):
/// the replay goes on - started again mid-way it would cut a pack, and the receiver would parse
/// garbage. Every guaranteed pack once, in order.
#[test]
fn a_continue_for_the_replay_in_progress_does_not_restart_it() {
    let (sender, mut receiver) = parked_pair(100 << 20, "twice", packs(30));
    receiver.control.continue_from(receiver.position.received(), false);
    tell(&receiver, &sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut wire = Vec::new();
    source.try_drain(&mut wire);
    let mut rx = RxState::new(1);
    assert!(0 <= rx.write_all(&wire[..wire.len() / 3], &mut receiver)); // the first socket brings a third
    tell(&receiver, &sender);
    drop(source);
    receiver.position.drop_pack();
    sender.control.silent();
    receiver.control.silent();
    let named = receiver.position.received();
    receiver.control.continue_from(named, false);
    tell(&receiver, &sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut rx = RxState::new(1);
    let mut b = [0u8; 300];
    let mut said = false;
    loop {
        let n = source.read(&mut b);
        if n <= 0 {
            break;
        }
        assert!(0 <= rx.write_all(&b[..n as usize], &mut receiver), "{:?}", rx.error);
        if !said {
            said = true;
            sender.control.heard(CONTINUE, named as u64 & 0xFFFF_FFFF, || -1, false);
            // the same position again, mid-replay
        }
    }
    assert!(said);
    let got: Vec<u32> = receiver.got.iter().filter(|p| p.id == 7).map(|p| p.n).collect();
    let expected: Vec<u32> = packs(30).iter().filter(|p| p.id == 7).map(|p| p.n).collect();
    assert_eq!(got, expected, "every guaranteed pack once, in order");
}

/// At the cap of the journal the source is not empty: it answers 0 - a graceful close waits for the
/// packs queued, a confirmation of the peer wakes the transmit. -1 would let the FIN go with
/// guaranteed packs unsent.
#[test]
fn at_the_cap_the_source_is_not_drained() {
    let (sender, receiver) = parked_pair(16 << 10, "capread", packs(60));
    receiver.control.continue_from(receiver.position.received(), false);
    tell(&receiver, &sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut b = vec![0u8; 64 << 10];
    let mut last = 1;
    for _ in 0..100 {
        last = source.read(&mut b);
        if last <= 0 {
            break;
        }
    }
    assert!(!sender.q.lock().unwrap().0.is_empty(), "the cap holds packs in the queue");
    assert_eq!(last, 0, "the cap: the source waits, it is not empty");
}

/// A pack whose producer has nothing now is in the middle: the source answers 0 - not empty, a
/// graceful close waits for the rest of the pack; the producer wakes the transmit.
#[test]
fn a_held_pack_is_not_drained() {
    let (sender, receiver) = parked_pair(100 << 20, "held", vec![P { id: 7, n: 1, s: HOLD.to_string() }]);
    receiver.control.continue_from(receiver.position.received(), false);
    tell(&receiver, &sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut b = vec![0u8; 4096];
    let first = source.read(&mut b);
    assert!(0 < first, "the pack of the agreement and the head of the pack");
    assert_eq!(source.read(&mut b), 0, "the pack waits for its producer");
}

/// What the receiver of a session keeps across its sockets: the position, the control, what it got,
/// what failed.
#[derive(Default)]
struct Kept {
    position: crate::lib::adhoc::utils::OwnCell<Position>,
    control: Option<Control>,
    got: Mutex<Vec<P>>,
    failures: Mutex<Vec<String>>,
}

/// The host of the receiver of a socket: the pack being parsed, the session it is of.
struct Host {
    kept: Arc<Kept>,
    p: P,
    id: u32,
    service: u32,
    value: u64,
}

impl Host {
    fn of(kept: &Arc<Kept>) -> Self {
        Self {
            kept: kept.clone(),
            p: P::default(),
            id: 0,
            service: 0,
            value: 0,
        }
    }
    fn control(&self) -> &Control {
        self.kept.control.as_ref().unwrap()
    }
}

impl RxHost for Host {
    fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool {
        if (SVC..SVC + 5).contains(&id) {
            self.service = id - SVC + 1;
            return true;
        }
        self.id = id;
        self.p = P { id: id as u8, ..P::default() };
        if id == 7 {
            unsafe { self.kept.position.lend(|__p| __p.start(src.pack_at(), id)) };
        }
        id == 7 || id == 8
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        if self.service != 0 {
            service_bytes(self.service, &mut self.value, src)
        } else {
            self.p.put_bytes(src)
        }
    }
    fn on_received(&mut self, st: &mut RxState) {
        if self.service != 0 {
            match self.on_service() {
                Heard::Lost(at) => unsafe { self.kept.position.lend(|__p| __p.position(at)) },
                Heard::Ok(at) => unsafe { self.kept.position.lend(|__p| __p.counted_from(at)) },
                Heard::Nothing => {}
            }
            return;
        }
        unsafe { self.kept.position.lend(|__p| __p.whole(st.consumed())) };
        self.kept.got.lock().unwrap().push(std::mem::take(&mut self.p));
    }
    fn on_window(&mut self, st: &mut RxState) {
        let b = unsafe { self.kept.position.lend(|__p| __p.due(st.consumed(), self.control().shift())) };
        if 0 < b {
            self.control().confirm(b);
        }
    }
    fn position(&self) -> Option<&crate::lib::adhoc::utils::OwnCell<Position>> {
        Some(&self.kept.position)
    }
    fn service(&self, id: u32) -> bool {
        (SVC..SVC + 5).contains(&id)
    }
    fn on_service(&mut self) -> Heard {
        if self.service == 0 {
            return Heard::Nothing;
        }
        let kind = self.service - 1;
        self.service = 0;
        let kept = self.kept.clone();
        self.control().heard(kind, self.value, || unsafe { kept.position.lend(|__p| __p.named()) }, false)
    }
    fn failure(&self, e: &(dyn std::error::Error + 'static)) {
        self.kept.failures.lock().unwrap().push(e.to_string());
    }
}

/// What the receiver of a session says, as the sender hears it.
fn tell_kept(kept: &Kept, sender: &Sender) {
    while let Some((kind, value)) = kept.control.as_ref().unwrap().next(false) {
        sender.control.heard(kind, value, || -1, false);
    }
}

/// Drains `source` into the receiver in pieces of `piece` bytes, up to `limit`, the confirmations
/// going back.
fn pump(source: &mut ResumableSource<Sender>, sink: &mut crate::lib::adhoc::connection::receiver::Receiver<Host>, kept: &Kept, sender: &Sender, piece: usize, limit: usize) -> usize {
    use crate::lib::adhoc::BytesDst as _;
    let mut total = 0;
    let mut b = vec![0u8; piece];
    while total < limit {
        let room = piece.min(limit - total);
        let n = source.read(&mut b[..room]);
        if n <= 0 {
            break;
        }
        let n = n as usize;
        assert_eq!(sink.on_part(&b[..n]), Some(n), "the receiver takes every byte");
        total += n;
        tell_kept(kept, sender);
    }
    total
}

/// The socket is lost in the middle of a guaranteed pack: the session keeps the parse of it, the
/// next socket's receiver goes on with it - the two sides agree first (Continue_, Ok_), the
/// sender's journal goes on from the byte the pack was cut at, not from its start. Every guaranteed
/// pack once, whole, in order.
#[test]
fn a_guaranteed_pack_goes_on_from_the_byte_a_lost_socket_cut_it() {
    use crate::lib::adhoc::connection::receiver::Receiver as Rcv;
    use crate::lib::adhoc::BytesDst as _;
    for cut in [5usize, 40, 333, 1500, 2700, 9000] {
        let dir = std::env::temp_dir().join(format!("adhoc-midpack-{}-{cut}", std::process::id()));
        let all = vec![
            P { id: 7, n: 1, s: "a".repeat(10) },
            P { id: 7, n: 2, s: "b".repeat(20_000) },
            P { id: 8, n: 3, s: "c".repeat(50) },
            P { id: 7, n: 4, s: "d".repeat(700) },
        ];
        let sender = Arc::new(Sender {
            q: Mutex::new((all.into(), None)),
            journal: Journal::new(Resumable::with_dir(100 << 20, dir)),
            control: control(),
            ext: None,
            parked: Default::default(),
        });
        let kept = Arc::new(Kept {
            control: Some(control()),
            ..Default::default()
        });

        // the first socket: the receiver names that it has no position, the sender answers where
        // its journal starts
        kept.control.as_ref().unwrap().continue_from(-1, false);
        tell_kept(&kept, &sender);
        let mut source = ResumableSource::new(sender.clone());
        let mut sink = Rcv::new(Host::of(&kept), 1);
        let first = pump(&mut source, &mut sink, &kept, &sender, 97, 30 + cut);
        let in_pack = unsafe { kept.position.lend(|__p| __p.in_pack()) };
        sink.suspend(); // the socket is gone: the session parks
        drop(sink);
        drop(source);

        // the next socket: its receiver takes the parse over, the two agree, the pack goes on
        kept.control.as_ref().unwrap().silent();
        let mut sink = Rcv::new(Host::of(&kept), 1);
        let named = unsafe { kept.position.lend(|__p| __p.named()) };
        kept.control.as_ref().unwrap().continue_from(named, false);
        tell_kept(&kept, &sender);
        let mut source = ResumableSource::new(sender.clone());
        sender.control.confirm(1); // the sender receives too: a confirmation of its own is due when it answers - it goes behind the
                                   // replay
        let second = pump(&mut source, &mut sink, &kept, &sender, 211, usize::MAX);

        let got: Vec<(u32, usize)> = kept.got.lock().unwrap().iter().filter(|p| p.id == 7).map(|p| (p.n, p.s.len())).collect();
        assert_eq!(got, vec![(1, 10), (2, 20_000), (4, 700)], "cut {cut}: every guaranteed pack once, whole, in order");
        assert!(kept.failures.lock().unwrap().is_empty(), "cut {cut}: nothing dropped");
        if in_pack {
            assert!(first + second < 20_000 + 900 + 200, "cut {cut}: the pack went on, not again from its start ( {first} + {second} )");
        }
    }
}

/// The peer of the next socket is a session over: its journal does not hold the bytes of the pack
/// cut - the pack is dropped, the handler of the transport is told, the stream goes on from where
/// the peer's journal does.
#[test]
fn a_guaranteed_pack_the_peer_no_longer_holds_is_dropped_and_told() {
    use crate::lib::adhoc::connection::receiver::Receiver as Rcv;
    use crate::lib::adhoc::BytesDst as _;
    let dir = std::env::temp_dir().join(format!("adhoc-midpack-lost-{}", std::process::id()));
    let sender = Arc::new(Sender {
        q: Mutex::new((vec![P { id: 7, n: 2, s: "b".repeat(20_000) }].into(), None)),
        journal: Journal::new(Resumable::with_dir(100 << 20, dir.clone())),
        control: control(),
        ext: None,
        parked: Default::default(),
    });
    let kept = Arc::new(Kept {
        control: Some(control()),
        ..Default::default()
    });
    kept.control.as_ref().unwrap().continue_from(-1, false);
    tell_kept(&kept, &sender);
    let mut source = ResumableSource::new(sender.clone());
    let mut sink = Rcv::new(Host::of(&kept), 1);
    pump(&mut source, &mut sink, &kept, &sender, 97, 3000);
    sink.suspend();
    drop(sink);
    drop(source);
    assert!(unsafe { kept.position.lend(|__p| __p.in_pack()) });

    // a new session of the peer: a journal of its own
    let fresh = Arc::new(Sender {
        q: Mutex::new((vec![P { id: 7, n: 9, s: "z".repeat(30) }].into(), None)),
        journal: Journal::new(Resumable::with_dir(100 << 20, dir.join("fresh"))),
        control: control(),
        ext: None,
        parked: Default::default(),
    });
    kept.control.as_ref().unwrap().silent();
    let mut sink = Rcv::new(Host::of(&kept), 1);
    let named = unsafe { kept.position.lend(|__p| __p.named()) };
    kept.control.as_ref().unwrap().continue_from(named, false);
    tell_kept(&kept, &fresh);
    let mut source = ResumableSource::new(fresh.clone());
    pump(&mut source, &mut sink, &kept, &fresh, 211, usize::MAX);
    let got: Vec<u32> = kept.got.lock().unwrap().iter().map(|p| p.n).collect();
    assert_eq!(got, vec![9], "the pack cut is dropped, the new session's goes");
    let f = kept.failures.lock().unwrap();
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].contains("The guaranteed pack 7 "), "{f:?}");
}

/// A session in files of its own ( [`Control`] ): the sender, and what its `save` writes besides the control - a word of the
/// application.
struct Filed {
    s: Arc<Sender>,
    word: std::sync::atomic::AtomicU32,
}

impl Session for Filed {
    fn journal(&self) -> Option<&Journal> {
        self.s.journal()
    }
    fn control(&self) -> &Control {
        &self.s.control
    }
    fn ext_connection(&self) -> Option<Arc<dyn crate::lib::adhoc::connection::external::External>> {
        self.s.ext_connection()
    }
    fn files(&self) -> Option<&'static str> {
        Some("filed")
    }
    fn save(&self, out: &mut Vec<u8>) -> bool {
        self.s.control.save(out);
        out.extend_from_slice(&self.word.load(std::sync::atomic::Ordering::Relaxed).to_le_bytes());
        true
    }
    fn load(&self, from: &mut &[u8]) -> std::io::Result<()> {
        self.s.control.load(self, from)?;
        self.word
            .store(crate::lib::adhoc::connection::transmitter::resumable::take_u32(from)?, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

impl TxHost for &Filed {
    fn on_serializing(&mut self) -> bool {
        let mut s: &Sender = &self.s;
        s.on_serializing()
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let mut s: &Sender = &self.s;
        s.get_bytes(dst)
    }
    fn on_serialized(&mut self) {
        let mut s: &Sender = &self.s;
        s.on_serialized()
    }
    fn guaranteed(&self) -> bool {
        let s: &Sender = &self.s;
        s.guaranteed()
    }
}

/// The process ends with a parked session in its files: what is queued goes into the journal, the files are closed cleanly. The
/// next process opens them: what `save` wrote comes back, the journal is as it was - the next socket replays every guaranteed
/// pack once, in order. The session over, its files go.
#[test]
fn a_session_in_its_files_goes_on_in_the_next_process() {
    let root = std::env::temp_dir().join(format!("adhoc-session-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    crate::lib::adhoc::connection::transmitter::resumable::set_dir(root.clone());
    let sender = |queued: Vec<P>, start: i64| {
        Arc::new(Sender {
            q: Mutex::new((queued.into(), None)),
            journal: Journal::new(Resumable::with_start(100 << 20, root.join("tmp"), start)),
            control: control(),
            ext: None,
            parked: std::sync::atomic::AtomicBool::new(true),
        })
    };
    let one = Filed {
        s: sender(packs(30), 1 << 50),
        word: std::sync::atomic::AtomicU32::new(7),
    };
    assert!(!one.s.control.open(&one, "one"), "no files yet: a fresh session");
    assert!(root.join("one.head").is_file() && root.join("one.ring").is_file());
    journal_queue(&one); // what is queued while parked goes into the journal, in the files
    let next = one.s.journal.next();
    one.s.control.stop(&one);
    drop(one);

    let other = Filed {
        s: sender(Vec::new(), 1 << 40),
        word: std::sync::atomic::AtomicU32::new(0),
    };
    assert!(!other.s.control.open(&other, "other") && !other.s.control.open(&other, "one"), "a session has its files once");
    other.s.control.ended(&other); // its journal still maps its ring: the files go all the same
    let files = |prefix: &str| {
        std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(prefix))
            .collect::<Vec<_>>()
    };
    assert!(files("other.").is_empty(), "{:?}", files("other."));
    let two = Filed {
        s: sender(Vec::new(), 1 << 40),
        word: std::sync::atomic::AtomicU32::new(0),
    };
    assert!(two.s.control.open(&two, "one"), "the files of the process before");
    assert_eq!(two.word.load(std::sync::atomic::Ordering::Relaxed), 7, "what save wrote");
    assert_eq!(two.s.journal.next(), next, "the journal up to its end");

    let mut receiver = Receiver {
        p: P::default(),
        id: 0,
        service: 0,
        value: 0,
        got: vec![],
        position: Position::default(),
        control: control(),
        lost: None,
    };
    reconnect(&two.s, &mut receiver);
    let guaranteed: Vec<u32> = receiver.got.iter().filter(|p| p.id == 7).map(|p| p.n).collect();
    let expected: Vec<u32> = packs(30).iter().filter(|p| p.id == 7).map(|p| p.n).collect();
    assert_eq!(guaranteed, expected, "every guaranteed pack once, in order");

    two.s.journal.end();
    two.s.control.ended(&two);
    assert!(files("one.").is_empty(), "the session is over: its files go - {:?}", files("one."));

    // the process falls - no stop: the header is not clean, this boot of the system left it, the journal is taken up; a
    // pack the transmitter had begun is told lost
    let fall = Filed {
        s: sender(packs(20), 1 << 45),
        word: std::sync::atomic::AtomicU32::new(3),
    };
    assert!(!fall.s.control.open(&fall, "fall"));
    journal_queue(&fall);
    let next = fall.s.journal.next();
    {
        use std::io::{Seek, Write};
        let mut h = std::fs::OpenOptions::new().write(true).open(root.join("fall.head")).unwrap();
        h.seek(std::io::SeekFrom::Start(Control::H_OPEN as u64)).unwrap();
        h.write_all(&8i32.to_le_bytes()).unwrap(); // id + 1 of the pack begun
    }
    drop(fall);
    let after = Filed {
        s: sender(Vec::new(), 1 << 41),
        word: std::sync::atomic::AtomicU32::new(0),
    };
    assert!(after.s.control.open(&after, "fall"));
    if crate::lib::adhoc::connection::transmitter::resumable::boot() != 0 {
        assert_eq!(after.s.journal.next(), next, "left by this boot: trusted");
    }
    let lost = std::fs::read_to_string(root.join("lost.fall.txt")).unwrap();
    assert!(lost.contains("ended without a stop") && lost.contains("A guaranteed pack begun, lost: id 7."), "{lost}");
    after.s.journal.end();
    after.s.control.ended(&after);
    assert!(files("fall.").is_empty(), "{:?}", files("fall."));
}
