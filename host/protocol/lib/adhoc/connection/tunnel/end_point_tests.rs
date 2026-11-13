// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! The ends of a tunnel: bursts out on a carrier, frames in to the virtual connection.

use super::*;
use crate::lib::adhoc::connection::receiver::tunnel::In;
use crate::lib::adhoc::connection::transmitter::tunnel::Out;
use crate::lib::adhoc::connection::receiver::{RxHost, RxState};
use crate::lib::adhoc::connection::receiver::Receiver;
use crate::lib::adhoc::BytesDst;
use std::collections::VecDeque;
use std::sync::Mutex;
use crate::lib::adhoc::connection::keyring::Keyring;
use crate::lib::chain::Chain;

const ID: u32 = 9;

/// The transmitter of the virtual connection: packs of raw bytes.
#[derive(Default)]
struct V {
    q: VecDeque<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
}

impl TxHost for V {
    fn on_serializing(&mut self) -> bool {
        match self.q.pop_front() {
            Some(p) => {
                self.cur = p;
                self.pos = 0;
                true
            }
            None => false,
        }
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let n = dst.remaining().min(self.cur.len() - self.pos);
        let at = dst.byte_;
        dst.buffer[at..at + n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        dst.byte_ += n;
        self.pos += n;
        self.pos == self.cur.len()
    }
    fn on_serialized(&mut self) {}
}

/// The carrier's transmitter: the bursts of one end queued on it.
struct C {
    out: Out,
    v: V,
    end: Arc<EndPoint>,
    queued: Arc<AtomicBool>,
}

impl TxHost for C {
    fn on_serializing(&mut self) -> bool {
        self.queued.swap(false, Ordering::AcqRel) && !self.end.silent() // the turn of the tunnel, as the generated carrier takes it
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let end = self.end.clone();
        self.out.get_bytes(dst, &mut self.v, &end, ID, 1, 2)
    }
    fn on_serialized(&mut self) {}
}

fn carrier(tid: u32) -> (C, Arc<Mutex<Vec<Event>>>) {
    let end = EndPoint::new(tid);
    let queued = Arc::new(AtomicBool::new(false));
    let q = queued.clone();
    end.set_enqueue(move || {
        assert!(!q.swap(true, Ordering::AcqRel), "one burst at a time");
        true
    });
    let events = Arc::new(Mutex::new(vec![]));
    let e = events.clone();
    end.set_events(move |ev| e.lock().unwrap().push(ev));
    (C { out: Out::default(), v: V::default(), end, queued }, events)
}

/// The wire the carrier writes, `window` bytes a read.
fn drain<H: TxHost>(c: &mut H, tx: &mut TxState, window: usize, wire: &mut Vec<u8>) {
    let mut buf = vec![0u8; window];
    loop {
        let n = tx.read(&mut buf, c);
        if n < 0 {
            return;
        }
        assert!(0 < n, "the window {window} is too small");
        wire.extend_from_slice(&buf[..n as usize]);
    }
}

/// The frames of a wire: (tunnel_id, body, aborted).
fn frames(mut w: &[u8]) -> Vec<(u16, Vec<u8>, bool)> {
    let mut r = vec![];
    while !w.is_empty() {
        assert_eq!(w[0] as u32, ID);
        let tid = u16::from_le_bytes([w[1], w[2]]);
        w = &w[3..];
        let mut body = vec![];
        loop {
            let len = u16::from_le_bytes([w[0], w[1]]);
            w = &w[2..];
            if len == 0 || len == 0xFFFF {
                r.push((tid, body, len == 0xFFFF));
                break;
            }
            body.extend_from_slice(&w[..len as usize]);
            w = &w[len as usize..];
        }
    }
    r
}

fn packs(seed: usize, count: usize) -> Vec<Vec<u8>> {
    (0..count).map(|i| (0..(i * 7_919 + seed) % 150_000 + 1).map(|j| (j * 31 + i + seed) as u8).collect()).collect()
}

#[test]
fn bursts_carry_the_packs_in_order() {
    for window in [8usize, 100, 70_000] {
        let (mut c, events) = carrier(0x1234);
        let mut tx = TxState::new();
        let mut wire = vec![];
        let mut sent = vec![];
        for round in 0..4 {
            let ps = packs(round, 5);
            for p in &ps {
                c.v.q.push_back(p.clone());
                sent.extend_from_slice(p);
                c.end.wake_transmit(); // a wake per pack: one burst queued
            }
            drain(&mut c, &mut tx, window, &mut wire);
        }
        let f = frames(&wire);
        // a burst a round, cut into more where it carried its share: only the last of a round is
        // shorter than that
        assert!(4 <= f.len() && f.iter().filter(|x| x.1.len() < MAX).count() <= 4, "window {window}: {:?}", f.iter().map(|x| x.1.len()).collect::<Vec<_>>());
        assert!(f.iter().all(|(tid, _, abort)| *tid == 0x1234 && !abort));
        let body: Vec<u8> = f.into_iter().flat_map(|x| x.1).collect();
        assert!(body == sent, "window {window}");
        assert!(events.lock().unwrap().is_empty());
        assert!(c.end.is_open());
    }
}

#[test]
fn a_wake_in_flight_asks_for_the_next_burst() {
    let (mut c, _) = carrier(1);
    let mut tx = TxState::new();
    c.v.q.push_back(vec![1; 50]);
    c.end.wake_transmit();
    let mut buf = vec![0u8; 20];
    let n = tx.read(&mut buf, &mut c); // the burst is armed and cut
    assert!(0 < n);
    let mut wire = buf[..n as usize].to_vec();
    c.v.q.push_back(vec![2; 10]);
    c.end.wake_transmit(); // the gate is closed: it asks for one more burst
    drain(&mut c, &mut tx, 20, &mut wire);
    let f = frames(&wire);
    let body: Vec<u8> = f.iter().flat_map(|x| x.1.clone()).collect();
    assert_eq!(body.len(), 60);
    assert_eq!(&body[50..], &[2; 10]);
}

#[test]
fn close_ends_the_tunnel_with_abort() {
    let (mut c, events) = carrier(5);
    let mut tx = TxState::new();
    c.v.q.push_back(vec![3; 1000]);
    c.end.wake_transmit(); // the pack is sent: its burst is queued
    c.end.close();
    assert!(!c.end.is_open());
    let mut wire = vec![];
    drain(&mut c, &mut tx, 64, &mut wire);
    let f = frames(&wire);
    assert_eq!(f.len(), 1);
    assert!(f[0].2, "the last burst ends with ABORT");
    assert_eq!(f[0].1, vec![3; 1000]);
    assert_eq!(*events.lock().unwrap(), vec![Event::THIS_CLOSE_GRACEFUL]);
    c.end.close(); // once
    assert_eq!(events.lock().unwrap().len(), 1);
}

/// A negative timeout set ends the tunnel as `close` does: the getters are negative while it
/// closes, the magnitude kept; the receive one is `i32::MIN` once it has ended.
#[test]
fn a_negative_timeout_closes_the_tunnel() {
    for receive in [true, false] {
        let (mut c, events) = carrier(5);
        let mut tx = TxState::new();
        c.v.q.push_back(vec![4; 300]);
        c.end.wake_transmit();
        c.end.set_receive_timeout(700);
        c.end.set_transmit_timeout(900);
        assert_eq!((c.end.receive_timeout(), c.end.transmit_timeout()), (700, 900));
        if receive {
            c.end.set_receive_timeout(-500);
        } else {
            c.end.set_transmit_timeout(-500);
        }
        assert!(!c.end.is_open());
        assert_eq!(if receive { c.end.receive_timeout() } else { c.end.transmit_timeout() }, -500);
        let mut wire = vec![];
        drain(&mut c, &mut tx, 64, &mut wire);
        let f = frames(&wire);
        assert!(f.len() == 1 && f[0].2 && f[0].1 == vec![4; 300], "what is queued goes, then ABORT");
        assert_eq!(*events.lock().unwrap(), vec![Event::THIS_CLOSE_GRACEFUL]);
        assert_eq!(c.end.receive_timeout(), i32::MIN);
    }
    let (c, _) = carrier(6);
    c.end.set_receive_timeout(i32::MIN);
    assert_eq!(c.end.transmit_timeout(), -i32::MAX, "closing");
}

/// The carrier's receiver: the frames of the tunnel go to the virtual connections, by `tunnel_id`.
struct R {
    tin: In<Arc<Mutex<Vec<u8>>>>,
    targets: Vec<(u32, Arc<Mutex<Vec<u8>>>)>,
    ended: Vec<u32>,
    /// The virtual connections whose frame was dropped half way.
    abandoned: usize,
    /// How the frames were routed: (tunnel_id, made when there is none).
    routes: Vec<(u32, bool)>,
}

impl RxHost for &mut R {
    fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
        assert_eq!(id, ID);
        true
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        let targets = &self.targets;
        let ended = &mut self.ended;
        let routes = &mut self.routes;
        self.tin.put_bytes(
            src,
            2,
            |tid, make| {
                routes.push((tid, make));
                targets.iter().find(|t| t.0 == tid).map(|t| t.1.clone())
            },
            |t, b| { t.lock().unwrap().extend_from_slice(b); b.len() },
            |_, tid| ended.push(tid),
        )
    }
    fn on_received(&mut self, _st: &mut RxState) {}
    fn on_abandon(&mut self) {
        if self.tin.abandon().is_some() {
            self.abandoned += 1;
        }
    }
}

#[test]
fn frames_in_go_to_their_tunnels() {
    let a = Arc::new(Mutex::new(vec![]));
    let b = Arc::new(Mutex::new(vec![]));
    let mut wire = vec![];
    let mut want_a = vec![];
    let mut want_b = vec![];
    for (i, p) in packs(3, 9).into_iter().enumerate() {
        let tid: u16 = [10, 20, 30][i % 3]; // 30: nobody, dropped
        wire.push(ID as u8);
        wire.extend_from_slice(&tid.to_le_bytes());
        for c in p.chunks([1, 500, 65_534][i % 3]) {
            wire.extend_from_slice(&(c.len() as u16).to_le_bytes());
            wire.extend_from_slice(c);
        }
        let abort = i == 7;
        wire.extend_from_slice(&(if abort { 0xFFFFu16 } else { 0 }).to_le_bytes());
        match tid {
            10 => want_a.extend_from_slice(&p),
            20 => want_b.extend_from_slice(&p),
            _ => {}
        }
    }
    for step in [1usize, 7, 4096, 1 << 20] {
        a.lock().unwrap().clear();
        b.lock().unwrap().clear();
        let mut r = R { tin: In::default(), targets: vec![(10, a.clone()), (20, b.clone())], ended: vec![], abandoned: 0, routes: vec![] };
        {
            let mut sink = Receiver::new(&mut r, 1);
            for w in wire.chunks(step) {
                assert!(sink.on_bytes(w));
            }
        }
        assert!(*a.lock().unwrap() == want_a, "step {step}");
        assert!(*b.lock().unwrap() == want_b, "step {step}");
        assert_eq!(r.ended, vec![20], "step {step}: the frame with ABORT ends its tunnel");
    }
}

/// The carrier's receiver drops a frame half way: the virtual connection it fed is told, the next
/// frame is read from its `tunnel_id` - wherever the cut was.
#[test]
fn a_frame_abandoned_half_way_is_told_and_the_next_one_routes() {
    let mut half = vec![ID as u8];
    half.extend_from_slice(&10u16.to_le_bytes());
    half.extend_from_slice(&5u16.to_le_bytes());
    half.extend_from_slice(&[1, 2, 3, 4, 5]);
    half.extend_from_slice(&5u16.to_le_bytes());
    half.extend_from_slice(&[6, 7]); // the frame is cut here
    let mut next = vec![ID as u8];
    next.extend_from_slice(&20u16.to_le_bytes());
    next.extend_from_slice(&3u16.to_le_bytes());
    next.extend_from_slice(&[8, 9, 10]);
    next.extend_from_slice(&0u16.to_le_bytes());
    for cut in 1..=half.len() {
        let a = Arc::new(Mutex::new(vec![]));
        let b = Arc::new(Mutex::new(vec![]));
        let mut r = R { tin: In::default(), targets: vec![(10, a.clone()), (20, b.clone())], ended: vec![], abandoned: 0, routes: vec![] };
        {
            let mut sink = Receiver::new(&mut r, 1);
            assert!(sink.on_bytes(&half[..cut]));
            sink.abandon();
            assert!(sink.on_bytes(&next));
        }
        assert_eq!(*b.lock().unwrap(), vec![8, 9, 10], "cut {cut}");
        assert_eq!(r.abandoned, if cut < 5 { 0 } else { 1 }, "cut {cut}: routed at its first length - its virtual connection is told");
        assert!(r.ended.is_empty(), "cut {cut}");
    }
}

/// A frame that is only ABORT (a relay's farewell, the close of the far end with nothing to send)
/// ends the tunnel that is there and makes none; a frame with bytes or an empty one may make its
/// tunnel.
#[test]
fn an_abort_only_frame_makes_no_tunnel() {
    let frame = |tid: u16, body: &[u8], end: u16| {
        let mut w = vec![ID as u8];
        w.extend_from_slice(&tid.to_le_bytes());
        if !body.is_empty() {
            w.extend_from_slice(&(body.len() as u16).to_le_bytes());
            w.extend_from_slice(body);
        }
        w.extend_from_slice(&end.to_le_bytes());
        w
    };
    let mut wire = frame(10, &[], 0xFFFF); // a tunnel that is there
    wire.extend(frame(30, &[], 0xFFFF)); // none: nothing is made
    wire.extend(frame(20, &[1, 2], 0)); // bytes: made when there is none
    wire.extend(frame(40, &[], 0)); // empty: made too
    for step in [1usize, 2, 3, 1000] {
        let a = Arc::new(Mutex::new(vec![]));
        let b = Arc::new(Mutex::new(vec![]));
        let mut r = R { tin: In::default(), targets: vec![(10, a.clone()), (20, b.clone())], ended: vec![], abandoned: 0, routes: vec![] };
        {
            let mut sink = Receiver::new(&mut r, 1);
            for w in wire.chunks(step) {
                assert!(sink.on_bytes(w));
            }
        }
        assert_eq!(r.routes, vec![(10, false), (30, false), (20, true), (40, true)], "step {step}");
        assert_eq!(r.ended, vec![10], "step {step}");
        assert_eq!(*b.lock().unwrap(), vec![1, 2], "step {step}");
    }
}

// a tunnel with a chain

/// A custom stage of the test: every byte XOR-ed with the place it has in the stream — its state
/// goes on across windows and starts again with every burst.
#[derive(Default)]
struct Xor(u8);

impl crate::lib::adhoc::chain::CustomTx for Xor {
    fn rearm(&mut self) {
        self.0 = 0;
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>, inner: &mut dyn FnMut(&mut Tx<'_>) -> bool) -> bool {
        let from = dst.byte_;
        let done = inner(dst);
        for b in &mut dst.buffer[from..dst.byte_] {
            *b ^= self.0;
            self.0 = self.0.wrapping_add(1);
        }
        done
    }
}

impl crate::lib::adhoc::chain::CustomRx for Xor {
    fn rearm(&mut self) {
        self.0 = 0;
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>, inner: &mut dyn FnMut(&mut Rx<'_>)) {
        let mut plain = src.buffer[src.byte_..src.byte_max].to_vec(); // the window is the wire's: undone into a buffer of its own
        for b in &mut plain {
            *b ^= self.0;
            self.0 = self.0.wrapping_add(1);
        }
        src.byte_ = src.byte_max;
        inner(&mut src.sub(&plain))
    }
}

fn xor_tx(_: &'static [crate::lib::adhoc::chain::Arg]) -> Box<dyn crate::lib::adhoc::chain::CustomTx> {
    Box::new(Xor::default())
}

fn xor_rx(_: &'static [crate::lib::adhoc::chain::Arg]) -> Box<dyn crate::lib::adhoc::chain::CustomRx> {
    Box::new(Xor::default())
}

const XOR: Stage = Stage::Custom(crate::lib::adhoc::chain::CustomStage { name: "Xor", args: &[], tx: xor_tx, rx: xor_rx });

const SPEC: &[Stage] = &[XOR, Stage::Stages(crate::lib::stages::Run { kind1: crate::lib::stages::ZSTD, level1: 3, kind2: 0, level2: 0 })];

/// The carrier's transmitter of a tunnel with a chain.
struct CC(C);

impl TxHost for CC {
    fn on_serializing(&mut self) -> bool {
        self.0.queued.swap(false, Ordering::AcqRel)
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let end = self.0.end.clone();
        self.0.out.get_bytes_chain(dst, &mut self.0.v, &end, ID, 1, 2, SPEC, &crate::lib::stages::Keys::none())
    }
    fn on_serialized(&mut self) {}
}

/// The carrier's receiver of a tunnel with a chain.
struct RC {
    tin: In<Arc<Mutex<Vec<u8>>>>,
    target: Arc<Mutex<Vec<u8>>>,
}

impl RxHost for &mut RC {
    fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
        assert_eq!(id, ID);
        true
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        let t = self.target.clone();
        self.tin.put_bytes_chain(src, 2, SPEC, move |_, _| Some(t), |_| crate::lib::stages::Keys::none(), |t, b| { t.lock().unwrap().extend_from_slice(b); b.len() }, |_, _| {})
    }
    fn on_received(&mut self, _st: &mut RxState) {}
}

#[test]
fn chained_bursts_round_trip() {
    for window in [8usize, 100, 70_000] {
        let (c, _) = carrier(0x55);
        let mut c = CC(c);
        let mut tx = TxState::new();
        let mut wire = vec![];
        let mut sent = vec![];
        for round in 0..4 {
            for p in packs(round, 4) {
                c.0.v.q.push_back(p.clone());
                sent.extend_from_slice(&p);
                c.0.end.wake_transmit();
            }
            let mut buf = vec![0u8; window];
            loop {
                let n = tx.read(&mut buf, &mut c);
                if n < 0 {
                    break;
                }
                assert!(0 < n, "the window {window} is too small");
                wire.extend_from_slice(&buf[..n as usize]);
            }
        }
        for step in [1usize, 7, 4096, 1 << 20] {
            let mut r = RC { tin: In::default(), target: Arc::new(Mutex::new(vec![])) };
            {
                let mut sink = Receiver::new(&mut r, 1);
                for w in wire.chunks(step) {
                    assert!(sink.on_bytes(w));
                }
            }
            assert!(*r.target.lock().unwrap() == sent, "window {window}, step {step}");
        }
    }
}

/// A stage that gives nothing until the end of the burst, then all it got: the end of a frame comes
/// with bytes to deliver.
#[derive(Default)]
struct Hold(Vec<u8>);

struct Pass;

impl crate::lib::adhoc::chain::CustomTx for Pass {
    fn rearm(&mut self) {}
    fn get_bytes(&mut self, dst: &mut Tx<'_>, inner: &mut dyn FnMut(&mut Tx<'_>) -> bool) -> bool {
        inner(dst)
    }
}

impl crate::lib::adhoc::chain::CustomRx for Hold {
    fn rearm(&mut self) {
        self.0.clear();
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>, inner: &mut dyn FnMut(&mut Rx<'_>)) {
        if src.byte_ < src.byte_max {
            self.0.extend_from_slice(&src.buffer[src.byte_..src.byte_max]);
            src.byte_ = src.byte_max;
            return;
        }
        let all = std::mem::take(&mut self.0);
        inner(&mut src.sub(&all))
    }
}

const HOLD: Stage = Stage::Custom(crate::lib::adhoc::chain::CustomStage {
    name: "Hold",
    args: &[],
    tx: |_| Box::new(Pass),
    rx: |_| Box::new(Hold::default()),
});

/// The carrier's transmitter of a tunnel with the chain `spec`.
struct CS(C, &'static [Stage]);

impl TxHost for CS {
    fn on_serializing(&mut self) -> bool {
        self.0.queued.swap(false, Ordering::AcqRel)
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let end = self.0.end.clone();
        self.0.out.get_bytes_chain(dst, &mut self.0.v, &end, ID, 1, 2, self.1, &crate::lib::stages::Keys::none())
    }
    fn on_serialized(&mut self) {}
}

/// The carrier's receiver of a tunnel with the chain `spec`, its virtual connection taking no more
/// than `take` bytes a call.
struct RS {
    tin: In<Arc<Mutex<Vec<u8>>>>,
    target: Arc<Mutex<Vec<u8>>>,
    spec: &'static [Stage],
    take: usize,
}

impl RxHost for &mut RS {
    fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
        assert_eq!(id, ID);
        true
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        let (t, take) = (self.target.clone(), self.take);
        self.tin.put_bytes_chain(
            src,
            2,
            self.spec,
            move |_, _| Some(t),
            |_| crate::lib::stages::Keys::none(),
            |t, b| {
                let k = b.len().min(take);
                t.lock().unwrap().extend_from_slice(&b[..k]);
                k
            },
            |_, _| {},
        )
    }
    fn on_received(&mut self, _st: &mut RxState) {}
}

/// The virtual connection behind a chain takes part of what the stages give (a relay of it is
/// behind): the rest is kept, no byte of the carrier is taken before it goes, and the carrier's
/// transport offers its bytes again - every byte arrives, in order, the end of a frame too.
#[test]
fn a_chained_tunnel_keeps_what_its_connection_refused() {
    for spec in [&[XOR][..], &[HOLD][..], &[XOR, HOLD][..]] {
        let (c, _) = carrier(0x66);
        let mut c = CS(c, spec);
        let mut tx = TxState::new();
        let mut wire = vec![];
        let mut sent = vec![];
        for round in 0..3 {
            for p in packs(round, 3).into_iter().map(|p| p[..p.len().min(20_000)].to_vec()) {
                c.0.v.q.push_back(p.clone());
                sent.extend_from_slice(&p);
                c.0.end.wake_transmit();
            }
            drain(&mut c, &mut tx, 4096, &mut wire);
        }
        for step in [1usize, 13, 4096, 1 << 20] {
            let mut r = RS { tin: In::default(), target: Arc::new(Mutex::new(vec![])), spec, take: 7 };
            {
                let mut sink = Receiver::new(&mut r, 1);
                for w in wire.chunks(step) {
                    let mut at = 0;
                    let mut stuck = 0;
                    while at < w.len() {
                        let n = sink.on_part(&w[at..]).expect("the stream is whole");
                        at += n;
                        stuck = if n == 0 { stuck + 1 } else { 0 };
                        assert!(stuck < 1_000_000, "no progress");
                    }
                }
            }
            let got = r.target.lock().unwrap();
            assert!(*got == sent, "{} stages, step {step}: {} of {} bytes", spec.len(), got.len(), sent.len());
        }
    }
}

/// The external connection of a carrier: counts its wakes.
#[derive(Default)]
struct Wakes(std::sync::atomic::AtomicUsize);

impl External for Wakes {
    fn is_open(&self) -> bool {
        true
    }
    fn wake_transmit(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn receive_timeout(&self) -> i32 {
        0
    }
    fn set_receive_timeout(&self, _ms: i32) {}
    fn transmit_timeout(&self) -> i32 {
        0
    }
    fn set_transmit_timeout(&self, _ms: i32) {}
    fn abort(&self) {}
}

/// A pack of the virtual connection whose producer gives its bytes as they come: `avail` of them
/// now.
struct Slow {
    cur: Vec<u8>,
    pos: usize,
    avail: usize,
    queued: bool,
}

impl TxHost for Slow {
    fn on_serializing(&mut self) -> bool {
        std::mem::take(&mut self.queued)
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let n = dst.remaining().min(self.avail - self.pos);
        if n == 0 {
            dst.hold(); // nothing now: the producer wakes the transmitter
            return false;
        }
        let at = dst.byte_;
        dst.buffer[at..at + n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        dst.byte_ += n;
        self.pos += n;
        self.pos == self.cur.len()
    }
    fn on_serialized(&mut self) {}
}

struct SlowCarrier {
    out: Out,
    v: Slow,
    end: Arc<EndPoint>,
    queued: Arc<AtomicBool>,
    chain: Option<&'static [Stage]>,
}

impl TxHost for SlowCarrier {
    fn on_serializing(&mut self) -> bool {
        self.queued.swap(false, Ordering::AcqRel)
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let end = self.end.clone();
        match self.chain {
            Some(spec) => self.out.get_bytes_chain(dst, &mut self.v, &end, ID, 1, 2, spec, &crate::lib::stages::Keys::none()),
            None => self.out.get_bytes(dst, &mut self.v, &end, ID, 1, 2),
        }
    }
    fn on_serialized(&mut self) {}
}

/// A pack of the virtual connection waits for its producer mid-burst: the carrier sleeps on it,
/// held, and the producer's wake of the end - the burst in flight - wakes the carrier (a wake of
/// the end alone was lost: the carrier slept for good).
#[test]
fn a_burst_waiting_for_its_producer_is_woken_with_it() {
    for chain in [None, Some(&[XOR][..])] {
        let (c, _) = carrier(0x77);
        let wakes = Arc::new(Wakes::default());
        let w: Arc<dyn External> = wakes.clone();
        c.end.set_carrier(Arc::downgrade(&w));
        let body: Vec<u8> = (0..1000u32).map(|i| (i * 13) as u8).collect();
        let mut s = SlowCarrier { out: Out::default(), v: Slow { cur: body.clone(), pos: 0, avail: 300, queued: true }, end: c.end, queued: c.queued, chain };
        s.end.wake_transmit();
        let mut tx = TxState::new();
        let mut wire = vec![];
        let mut buf = vec![0u8; 64];
        let mut read = |s: &mut SlowCarrier, tx: &mut TxState, wire: &mut Vec<u8>| loop {
            let n = tx.read(&mut buf, s);
            if n <= 0 {
                return n;
            }
            wire.extend_from_slice(&buf[..n as usize]);
        };
        assert_eq!(read(&mut s, &mut tx, &mut wire), 0, "the pack waits");
        assert!(tx.held, "the carrier sleeps held, not for room");
        assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
        s.v.avail = body.len();
        s.end.wake_transmit(); // the producer of the pack
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1, "the carrier is woken");
        s.end.wake_transmit();
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1, "once");
        assert_eq!(read(&mut s, &mut tx, &mut wire), -1);
        let got: Vec<u8> = match chain {
            None => frames(&wire).into_iter().flat_map(|x| x.1).collect(),
            Some(spec) => {
                let mut r = RS { tin: In::default(), target: Arc::new(Mutex::new(vec![])), spec, take: usize::MAX };
                assert!(Receiver::new(&mut r, 1).on_bytes(&wire));
                let got = r.target.lock().unwrap().clone();
                got
            }
        };
        assert!(got == body, "chain {}", chain.is_some());
    }
}

// the keys of the chain of a tunnel

/// The carrier's transmitter of a tunnel with the chain `spec`, keyed by `keys`.
struct CK(C, &'static [Stage], crate::lib::stages::Keys);

impl TxHost for CK {
    fn on_serializing(&mut self) -> bool {
        self.0.queued.swap(false, Ordering::AcqRel)
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let end = self.0.end.clone();
        self.0.out.get_bytes_chain(dst, &mut self.0.v, &end, ID, 1, 2, self.1, &self.2)
    }
    fn on_serialized(&mut self) {}
}

/// The carrier's receiver of a tunnel with the chain `spec`, keyed by `keys`.
struct RK {
    tin: In<Arc<Mutex<Vec<u8>>>>,
    target: Arc<Mutex<Vec<u8>>>,
    spec: &'static [Stage],
    keys: crate::lib::stages::Keys,
}

impl RxHost for &mut RK {
    fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
        assert_eq!(id, ID);
        true
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        let (t, k) = (self.target.clone(), self.keys.clone());
        self.tin.put_bytes_chain(src, 2, self.spec, move |_, _| Some(t), move |_| k, |t, b| { t.lock().unwrap().extend_from_slice(b); b.len() }, |_, _| {})
    }
    fn on_received(&mut self, _st: &mut RxState) {}
}

fn key(id: u32, seed: u8) -> crate::lib::adhoc::connection::keyring::Key {
    crate::lib::adhoc::connection::keyring::Key::new(id, std::array::from_fn(|i| seed ^ i as u8), std::array::from_fn(|i| seed.wrapping_mul(3) ^ i as u8))
}

fn ring(keys: &[crate::lib::adhoc::connection::keyring::Key]) -> Arc<Keyring> {
    let r = Arc::new(Keyring::new());
    for k in keys {
        r.add(k.clone());
    }
    r
}

const ZSTD_CHACHA: &[Stage] = &[Stage::Stages(crate::lib::stages::Run { kind1: crate::lib::stages::ZSTD, level1: 3, kind2: crate::lib::stages::CHACHA20_PACK, level2: 0 })];
const CHACHA: &[Stage] = &[Stage::Stages(crate::lib::stages::Run { kind1: crate::lib::stages::CHACHA20_PACK, level1: 0, kind2: 0, level2: 0 })];

/// The bursts of a tunnel through `spec` keyed by `tx_keys`, on a carrier with keys of its own: the wire, what was sent.
fn keyed_wire(spec: &'static [Stage], tx_keys: crate::lib::stages::Keys) -> (Vec<u8>, Vec<u8>) {
    let (c, _) = carrier(0x88);
    let mut c = CK(c, spec, tx_keys);
    let mut tx = TxState::new();
    tx.keys.add(key(1, 0x11)); // the carrier's: a relay knows it
    let mut wire = vec![];
    let mut sent = vec![];
    for round in 0..3 {
        for p in packs(round, 3).into_iter().map(|p| p[..p.len().min(30_000)].to_vec()) {
            c.0.v.q.push_back(p.clone());
            sent.extend_from_slice(&p);
            c.0.end.wake_transmit();
        }
        drain(&mut c, &mut tx, 4096, &mut wire);
    }
    (wire, sent)
}

/// What a carrier with the carrier's key gets of `wire` through `spec` keyed by `rx_keys`.
fn keyed_take(spec: &'static [Stage], wire: &[u8], rx_keys: crate::lib::stages::Keys, step: usize) -> Vec<u8> {
    let mut r = RK { tin: In::default(), target: Arc::new(Mutex::new(vec![])), spec, keys: rx_keys };
    {
        let mut sink = Receiver::new(&mut r, 1).keys(ring(&[key(1, 0x11)]));
        for w in wire.chunks(step) {
            assert!(sink.on_bytes(w));
        }
    }
    let got = r.target.lock().unwrap().clone();
    got
}

/// The chain of a tunnel is keyed by the tunnel, not by its carrier: the far end with the tunnel's key reads every burst whatever
/// the keys of its carrier; one that has only the carrier's key - a relay - reads nothing; a tunnel with no key sends nothing.
#[test]
fn a_chained_tunnel_is_keyed_by_the_tunnel_not_its_carrier() {
    let tunnel = || crate::lib::stages::Keys::new(ring(&[key(7, 0x77)]), None);
    for spec in [ZSTD_CHACHA, CHACHA] {
        let (wire, sent) = keyed_wire(spec, tunnel());
        let probe = &sent[1000..1064];
        assert!(!wire.windows(64).any(|w| w == probe), "plaintext on the wire");
        for step in [1usize, 13, 4096, 1 << 20] {
            assert!(keyed_take(spec, &wire, tunnel(), step) == sent, "{} stages, step {step}: the far end with the tunnel's key", spec.len());
            // the key of the carrier, as a relay has it: no burst is read
            assert!(keyed_take(spec, &wire, crate::lib::stages::Keys::new(ring(&[key(1, 0x11)]), None), step).is_empty(), "step {step}: the carrier's key");
            assert!(keyed_take(spec, &wire, crate::lib::stages::Keys::none(), step).is_empty(), "step {step}: no key");
        }
        // a key both ends know in place of the tunnel's own ( the region of the keys of the chain ): another wire, read alike
        let shared = || crate::lib::stages::Keys::new(ring(&[key(42, 0x42)]), None);
        let (other, sent2) = keyed_wire(spec, shared());
        assert!(other != wire);
        assert!(keyed_take(spec, &other, shared(), 4096) == sent2);
        assert!(keyed_take(spec, &other, tunnel(), 4096).is_empty());
        // no key of the tunnel: the burst is refused - the carrier's key is not taken
        let refused = std::panic::catch_unwind(|| keyed_wire(spec, crate::lib::stages::Keys::none())).expect_err("refused");
        let why = refused.downcast_ref::<String>().cloned().or_else(|| refused.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
        assert!(why.contains("Tunnel::keys") && why.contains("The keys of the chain of the tunnel"), "{why}");
    }
}

/// A carrier with two tunnels, their bursts queued in the order they were woken.
struct Two {
    outs: [Out; 2],
    vs: [V; 2],
    ends: [Arc<EndPoint>; 2],
    queue: Arc<Mutex<VecDeque<usize>>>,
    cur: usize,
}

impl TxHost for Two {
    fn on_serializing(&mut self) -> bool {
        match self.queue.lock().unwrap().pop_front() {
            Some(i) => {
                self.cur = i;
                true
            }
            None => false,
        }
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let (i, end) = (self.cur, self.ends[self.cur].clone());
        self.outs[i].get_bytes(dst, &mut self.vs[i], &end, ID, 1, 2)
    }
    fn on_serialized(&mut self) {}
}

/// One tunnel keeps a carrier busy: its burst ends between packs once it carried its share, and the
/// other tunnel's burst goes before the rest of it. Every byte of both arrives, in order.
#[test]
fn a_busy_tunnel_lets_the_other_through_after_its_share() {
    for window in [100usize, 4096, 70_000] {
        let queue: Arc<Mutex<VecDeque<usize>>> = Default::default();
        let ends = [EndPoint::new(1), EndPoint::new(2)];
        for (i, e) in ends.iter().enumerate() {
            let q = queue.clone();
            e.set_enqueue(move || {
                q.lock().unwrap().push_back(i);
                true
            });
        }
        let mut c = Two { outs: [Out::default(), Out::default()], vs: [V::default(), V::default()], ends: ends.clone(), queue, cur: 0 };
        let big: Vec<Vec<u8>> = (0..20).map(|i| (0..20_000u32).map(|j| (j * 13 + i) as u8).collect()).collect();
        for p in &big {
            c.vs[0].q.push_back(p.clone());
        }
        c.ends[0].wake_transmit();
        let small = vec![7u8; 100];
        c.vs[1].q.push_back(small.clone());
        c.ends[1].wake_transmit();
        let mut tx = TxState::new();
        let mut wire = vec![];
        drain(&mut c, &mut tx, window, &mut wire);
        let f = frames(&wire);
        let at = f.iter().position(|x| x.0 == 2).expect("the second tunnel got no burst");
        let before: usize = f[..at].iter().filter(|x| x.0 == 1).map(|x| x.1.len()).sum();
        assert!(before <= MAX + 20_000, "window {window}: {before} bytes of the busy tunnel went first");
        let busy: Vec<u8> = f.iter().filter(|x| x.0 == 1).flat_map(|x| x.1.clone()).collect();
        assert!(busy == big.concat(), "window {window}: the busy tunnel's bytes");
        assert!(f[at].1 == small, "window {window}");
    }
}

/// Its share carried exactly at the end of a pack, the burst ends there: it takes no other pack -
/// the other tunnel waits a share, not a share and one more pack.
#[test]
fn a_burst_ends_at_the_pack_its_share_ended() {
    for window in [70_000usize] { // a chunk of MAX: the share ends with it, at the end of the second pack
        let queue: Arc<Mutex<VecDeque<usize>>> = Default::default();
        let ends = [EndPoint::new(1), EndPoint::new(2)];
        for (i, e) in ends.iter().enumerate() {
            let q = queue.clone();
            e.set_enqueue(move || {
                q.lock().unwrap().push_back(i);
                true
            });
        }
        let mut c = Two { outs: [Out::default(), Out::default()], vs: [V::default(), V::default()], ends: ends.clone(), queue, cur: 0 };
        let half: Vec<Vec<u8>> = (0..6).map(|i| vec![i as u8; MAX / 2]).collect(); // two of them are the share exactly
        for p in &half {
            c.vs[0].q.push_back(p.clone());
        }
        c.ends[0].wake_transmit();
        c.vs[1].q.push_back(vec![7u8; 100]);
        c.ends[1].wake_transmit();
        let mut tx = TxState::new();
        let mut wire = vec![];
        drain(&mut c, &mut tx, window, &mut wire);
        let f = frames(&wire);
        assert_eq!((f[0].0, f[0].1.len()), (1, MAX), "window {window}: the first burst is two packs, the share");
        assert_eq!(f[1].0, 2, "window {window}: the other tunnel next");
        let busy: Vec<u8> = f.iter().filter(|x| x.0 == 1).flat_map(|x| x.1.clone()).collect();
        assert!(busy == half.concat(), "window {window}: the busy tunnel's bytes");
    }
}

/// The carrier's socket goes with a burst half out: the pack it cut is dropped, the gate opens, and
/// the next burst starts afresh - its `[id][tunnel_id]` first, at a pack boundary.
#[test]
fn a_burst_dropped_half_out_leaves_the_next_whole() {
    let (mut c, _) = carrier(0x0102);
    let mut tx = TxState::new();
    c.v.q.push_back(vec![1u8; 3000]);
    c.end.wake_transmit();
    let mut buf = vec![0u8; 100];
    assert!(0 < tx.read(&mut buf, &mut c), "the burst begins");
    // the socket is gone: the transport drops what it was sending, the carrier tells the tunnel
    tx = TxState::new();
    c.out.abandon();
    c.end.dropped();
    let next = vec![2u8; 500];
    c.v.q.push_back(next.clone());
    c.end.wake_transmit();
    let mut wire = vec![];
    drain(&mut c, &mut tx, 64, &mut wire);
    let f = frames(&wire);
    assert_eq!(f.len(), 1, "{:?}", f.iter().map(|x| (x.0, x.1.len())).collect::<Vec<_>>());
    assert!(f[0].0 == 0x0102 && f[0].1 == next && !f[0].2);
}

/// The identity of a virtual connection is its tunnel.
#[test]
fn the_identity_of_an_end_is_its_tunnel() {
    let e = EndPoint::new(0x0A0B);
    assert_eq!(External::id(&*e), 0x0A0B);
    e.set_tunnel_id(7);
    assert_eq!(External::id(&*e), 7);
}

#[test]
fn naming_the_session_of_an_end_resumes_its_virtual_connection() {
    let e = EndPoint::new(5);
    let resumed = Arc::new(AtomicU32::new(0));
    let r = resumed.clone();
    e.set_resume(move || {
        r.fetch_add(1, Ordering::Relaxed);
    });
    let token = External::identify(&*e, 42);
    let slot = (e.slot() as u64) << 32; // the first identify takes a slot among the parked: what a reclaim on a new tunnel names
    assert!(e.slot() != 0 && e.slot() != u32::MAX);
    assert_eq!(token, slot | 42);
    assert_eq!(External::session(&*e), token);
    assert_eq!(External::identity(&*e), 42);
    assert_eq!(resumed.load(Ordering::Relaxed), 1);
    assert_eq!(External::identify(&*e, 42), token); // the same identity: no second resume
    assert_eq!(resumed.load(Ordering::Relaxed), 1);
    assert_eq!(External::id(&*e), 5); // the far end is still the tunnel
    assert_eq!(External::reclaim(&*e, (u64::MAX << 32) | 43), slot | 43); // nothing parked to take over: identify, the slot stays
    assert_eq!(resumed.load(Ordering::Relaxed), 2);
    assert_eq!(External::reclaim_id(&*e, (9 << 32) | 44, 45), slot | 45); // a token of another identity: the proved one
    assert_eq!(External::reclaim_id(&*e, (9 << 32) | 46, 46), slot | 46); // its own: reclaim, nothing parked there
    assert_eq!(resumed.load(Ordering::Relaxed), 4);
}

#[test]
fn the_start_of_a_tunnel_is_its_connect_event() {
    let (c, events) = carrier(ID);
    c.end.connected(false);
    c.end.connected(true);
    assert_eq!(*events.lock().unwrap(), vec![Event::THIS_CONNECT, Event::REMOTE_CONNECT]);
}

/// The receiver of a virtual connection: what came, as it came.
#[derive(Default)]
struct Got(Vec<u8>);

impl BytesDst for Got {
    fn on_bytes(&mut self, bytes: &[u8]) -> bool {
        self.0.extend_from_slice(bytes);
        true
    }
}

/// The external connection of a carrier: counts the failures its tunnels tell it.
#[derive(Default)]
struct Failures(std::sync::atomic::AtomicUsize);

impl External for Failures {
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
    fn abort(&self) {}
    fn failure(&self, _e: &(dyn std::error::Error + 'static)) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// What `from` sends now, its frames through a relay that changes nothing, into `to`: how many bytes.
fn relay(from: &mut C, tx: &mut TxState, to: &C, got: &mut Got) -> usize {
    let mut wire = vec![];
    drain(from, tx, 64, &mut wire);
    for (_, body, _) in frames(&wire) {
        let mut at = 0;
        while at < body.len() {
            at += to.end.on_part(got, &body[at..]).expect("the records open");
        }
    }
    wire.len()
}

/// Two ends of a tunnel, encrypted by Noise KK, the handshake through the bursts: the packs that
/// waited for the keys go, both ways, and each end proves the key the other expects.
#[test]
fn a_secured_tunnel_carries_records_after_its_handshake() {
    let ((sa, pa), (sb, pb)) = (crate::lib::x25519::keypair(), crate::lib::x25519::keypair());
    let (mut a, _) = carrier(1);
    let (mut b, _) = carrier(2);
    let (mut txa, mut txb, mut ga, mut gb) = (TxState::new(), TxState::new(), Got::default(), Got::default());
    let (to_b, to_a) = (packs(1, 3), packs(2, 2));
    a.v.q.extend(to_b.iter().cloned());
    b.v.q.extend(to_a.iter().cloned());
    a.end.wake_transmit(); // the packs are there before the keys
    b.end.secure(&Chain::default().noise_kk(&sb, &pa), false);
    a.end.secure(&Chain::default().noise_kk(&sa, &pb), true);
    a.end.connected(false); // the tunnel starts: the first message goes
    for _ in 0..4 {
        relay(&mut a, &mut txa, &b, &mut gb);
        relay(&mut b, &mut txb, &a, &mut ga);
    }
    assert!(gb.0 == to_b.concat() && ga.0 == to_a.concat(), "{} of {}, {} of {}", gb.0.len(), to_b.concat().len(), ga.0.len(), to_a.concat().len());
    assert_eq!((a.end.peer_key(), b.end.peer_key()), (Some(pb), Some(pa)));
}

// the chain of a tunnel inside its Noise stream

/// The packs of a virtual connection as a byte source: a pack at a time, between packs - a boundary.
struct Packs(VecDeque<Vec<u8>>, usize);

impl crate::lib::adhoc::BytesSrc for Packs {
    fn read(&mut self, dst: &mut [u8]) -> isize {
        let Some(p) = self.0.front() else { return -1 };
        let n = (p.len() - self.1).min(dst.len());
        dst[..n].copy_from_slice(&p[self.1..self.1 + n]);
        self.1 += n;
        if self.1 == p.len() {
            self.0.pop_front();
            self.1 = 0;
        }
        n as isize
    }
    fn at_boundary(&self) -> bool {
        self.1 == 0
    }
}

/// The carrier of a tunnel encrypted by Noise: its bursts from `src` - the packs, through the stages of the chain when it has one.
struct CN {
    out: Out,
    end: Arc<EndPoint>,
    queued: Arc<AtomicBool>,
    src: Box<dyn crate::lib::adhoc::BytesSrc>,
}

impl TxHost for CN {
    fn on_serializing(&mut self) -> bool {
        self.queued.swap(false, Ordering::AcqRel) && !self.end.silent()
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let end = self.end.clone();
        self.out.get_bytes_src(dst, &mut *self.src, &end, ID, 1, 2)
    }
    fn on_serialized(&mut self) {}
}

/// An end of a tunnel encrypted by Noise, its chain `run` over the Noise stream as the chain of a connection over its transport:
/// the carrier of its bursts and the stages of what comes - both made after the handshake.
fn noise_end(tid: u32, packs: Vec<Vec<u8>>, run: Option<crate::lib::stages::Run>) -> (CN, Option<crate::lib::stages::Unstaged>) {
    let (c, _) = carrier(tid);
    let p: Box<dyn crate::lib::adhoc::BytesSrc> = Box::new(Packs(packs.into(), 0));
    let Some(run) = run else { return (CN { out: c.out, end: c.end, queued: c.queued, src: p }, None) };
    let src: Box<dyn crate::lib::adhoc::BytesSrc> = Box::new(crate::lib::stages::Staged::new(move || Box::new(crate::lib::stages::transmitter::Stage::new(p, run, None))));
    let un = crate::lib::stages::Unstaged::new(move |x| Box::new(crate::lib::stages::receiver::Stage::new(x, run, None)));
    (CN { out: c.out, end: c.end, queued: c.queued, src }, Some(un))
}

/// What `from` sends now, through a relay that changes nothing, into `to`: its records opened, then through `stages` when there are
/// any, into `got`. How many bytes went on the wire.
fn relay_noise(from: &mut CN, tx: &mut TxState, to: &CN, stages: &mut Option<crate::lib::stages::Unstaged>, got: &mut Got) -> usize {
    let mut wire = vec![];
    drain(from, tx, 4096, &mut wire);
    for (_, body, _) in frames(&wire) {
        let mut at = 0;
        while at < body.len() {
            at += match stages.as_mut() {
                Some(st) => st.with(got, |st| to.end.on_part(st, &body[at..])),
                None => to.end.on_part(got, &body[at..]),
            }
            .expect("the records open");
        }
    }
    wire.len()
}

/// Two ends of a tunnel encrypted by Noise KK, the chain `run` of the tunnel inside the Noise stream:
/// (what A got, what B got, what A and B sent, the bytes on the wire).
fn noise_tunnel(run: Option<crate::lib::stages::Run>) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, usize) {
    let ((sa, pa), (sb, pb)) = (crate::lib::x25519::keypair(), crate::lib::x25519::keypair());
    let (to_b, to_a): (Vec<Vec<u8>>, Vec<Vec<u8>>) = (packs(1, 4).into_iter().map(|p| p.iter().map(|b| b % 7).collect()).collect(), packs(2, 3).into_iter().map(|p| p.iter().map(|b| b % 5).collect()).collect());
    let (mut a, mut sta) = noise_end(1, to_b.clone(), run);
    let (mut b, mut stb) = noise_end(2, to_a.clone(), run);
    a.end.wake_transmit();
    b.end.secure(&Chain::default().noise_kk(&sb, &pa), false);
    a.end.secure(&Chain::default().noise_kk(&sa, &pb), true);
    a.end.connected(false);
    let (mut txa, mut txb, mut ga, mut gb) = (TxState::new(), TxState::new(), Got::default(), Got::default());
    let mut wire = 0;
    for _ in 0..8 {
        a.end.wake_transmit();
        b.end.wake_transmit();
        wire += relay_noise(&mut a, &mut txa, &b, &mut stb, &mut gb);
        wire += relay_noise(&mut b, &mut txb, &a, &mut sta, &mut ga);
    }
    (ga.0, gb.0, to_b.concat(), to_a.concat(), wire)
}

/// A tunnel encrypted by Noise runs its chain over the Noise stream, as a connection over its transport: the packs through the
/// stages, then into records; the records opened, then through the stages - the handshake with no stage. [Zstd] compresses the
/// plaintext: the wire is smaller than with no chain. Both ways, across bursts.
#[test]
fn a_tunnel_under_noise_runs_its_chain_over_it() {
    let zstd = crate::lib::stages::Run { kind1: crate::lib::stages::ZSTD, level1: 3, kind2: 0, level2: 0 };
    let mut sizes = vec![];
    for run in [Some(zstd), None] {
        let (ga, gb, to_b, to_a, wire) = noise_tunnel(run);
        assert!(gb == to_b && ga == to_a, "{run:?}: {} of {}, {} of {}", gb.len(), to_b.len(), ga.len(), to_a.len());
        sizes.push(wire);
    }
    println!("the wire of a tunnel under Noise: {} bytes with [Zstd] over it, {} with no chain", sizes[0], sizes[1]);
    assert!(sizes[0] * 2 < sizes[1], "Zstd over Noise compresses the plaintext: {} bytes on the wire, {} with no chain", sizes[0], sizes[1]);
}

/// A record a relay changed: the end that gets it is told, once, and closes - its carrier goes on -,
/// and its last burst tells the other end.
#[test]
fn a_forged_record_ends_the_tunnel() {
    let ((sa, pa), (sb, pb)) = (crate::lib::x25519::keypair(), crate::lib::x25519::keypair());
    let (mut a, _) = carrier(1);
    let (mut b, events) = carrier(2);
    let heard: Arc<Failures> = Arc::default();
    let h: Arc<dyn External> = heard.clone();
    b.end.set_carrier(Arc::downgrade(&h));
    let (mut txa, mut txb, mut ga, mut gb) = (TxState::new(), TxState::new(), Got::default(), Got::default());
    b.end.secure(&Chain::default().noise_kk(&sb, &pa), false);
    a.end.secure(&Chain::default().noise_kk(&sa, &pb), true);
    a.end.connected(false);
    relay(&mut a, &mut txa, &b, &mut gb);
    relay(&mut b, &mut txb, &a, &mut ga);
    assert!(b.end.peer_key().is_some());
    let mut forged = [0u8; 2 + 5 + 16];
    forged[0] = 5;
    assert_eq!(b.end.on_part(&mut gb, &forged), None);
    assert_eq!(b.end.on_part(&mut gb, &forged), Some(forged.len()), "closed: what comes till the far end answers is dropped");
    assert_eq!(heard.0.load(Ordering::SeqCst), 1, "told once");
    assert!(!b.end.is_open() && gb.0.is_empty());
    let mut wire = vec![];
    drain(&mut b, &mut txb, 64, &mut wire);
    let f = frames(&wire);
    assert!(f.len() == 1 && f[0].2, "its last burst ends with ABORT: the other end hears the close");
    assert_eq!(*events.lock().unwrap(), vec![Event::THIS_CLOSE_GRACEFUL]);
}

/// A wrong key expected: the handshake fails at the end that reads it, which closes.
#[test]
fn a_wrong_key_fails_the_handshake() {
    let ((sa, pa), (sb, pb), (_, stranger)) = (crate::lib::x25519::keypair(), crate::lib::x25519::keypair(), crate::lib::x25519::keypair());
    let (mut a, _) = carrier(1);
    let (b, _) = carrier(2);
    let heard: Arc<Failures> = Arc::default();
    let h: Arc<dyn External> = heard.clone();
    b.end.set_carrier(Arc::downgrade(&h));
    let mut txa = TxState::new();
    b.end.secure(&Chain::default().noise_kk(&sb, &stranger), false);
    a.end.secure(&Chain::default().noise_kk(&sa, &pb), true);
    a.end.connected(false);
    let _ = pa;
    let mut wire = vec![];
    drain(&mut a, &mut txa, 64, &mut wire);
    let f = frames(&wire);
    assert_eq!(b.end.on_part(&mut Got::default(), &f[0].1), None);
    assert!(heard.0.load(Ordering::SeqCst) == 1 && !b.end.is_open() && b.end.peer_key().is_none());
}

// the two ends of a tunnel: it ends when both have sent their end

/// A close of this side counts its end: what comes on the tunnel till the far end answers is of the
/// session that ended and is dropped; the answer is told from an end of the far end.
#[test]
fn an_end_of_this_side_waits_for_its_answer() {
    let (mut c, events) = carrier(3);
    let mut tx = TxState::new();
    c.v.q.push_back(vec![1; 10]);
    c.end.wake_transmit();
    let mut wire = vec![];
    drain(&mut c, &mut tx, 64, &mut wire); // the far end knows the session
    c.end.close();
    assert!(c.end.drain() == 1 && !c.end.gone());
    let mut got = Got::default();
    assert_eq!(c.end.on_part(&mut got, &[7; 5]), Some(5));
    assert!(got.0.is_empty(), "dropped: the far end sent it before it saw the end");
    wire.clear();
    drain(&mut c, &mut tx, 64, &mut wire);
    let f = frames(&wire);
    assert!(f.len() == 1 && f[0].2, "the end goes with ABORT");
    assert!(c.end.gone());
    assert_eq!(*events.lock().unwrap(), vec![Event::THIS_CLOSE_GRACEFUL]);
    assert!(c.end.answered(), "the far end's end answers this one");
    assert_eq!(c.end.drain(), 0);
    assert!(!c.end.answered(), "nothing more is due: an end now is of the far end");
}

/// The far end ended the tunnel: the virtual connection is told once, and the end of this side
/// answers it - behind what is queued - and waits for no answer.
#[test]
fn the_end_of_the_far_end_is_answered() {
    let (mut c, events) = carrier(4);
    let mut tx = TxState::new();
    c.end.connected(true);
    assert!(!c.end.answered(), "no end of this side is due: the far end ended the tunnel");
    c.v.q.push_back(vec![2; 20]);
    c.end.closed(Event::REMOTE_CLOSE_GRACEFUL);
    c.end.closed(Event::REMOTE_CLOSE_GRACEFUL);
    let mut wire = vec![];
    drain(&mut c, &mut tx, 64, &mut wire);
    let f = frames(&wire);
    assert!(f.len() == 1 && f[0].2 && f[0].1 == vec![2; 20], "the answer, behind what is queued");
    assert!(c.end.gone() && c.end.drain() == 0, "the answer waits for no answer");
    assert_eq!(*events.lock().unwrap(), vec![Event::REMOTE_CONNECT, Event::REMOTE_CLOSE_GRACEFUL]);
    c.end.close();
    assert_eq!(c.end.drain(), 0);
}

/// A session nothing of which went out or came in is not known to the far end: its close ends it
/// with no ABORT and nothing to wait for, on the carrier's thread - at its turn there. A session the
/// far end opened is known to it; one whose far end spoke before its turn came ends as usual.
#[test]
fn a_session_the_far_end_does_not_know_ends_at_its_turn() {
    let (mut c, events) = carrier(5);
    let mut tx = TxState::new();
    c.end.connected(false);
    c.end.close();
    assert!(!c.end.is_open() && c.queued.load(Ordering::SeqCst), "its turn is queued on the carrier");
    assert_eq!(*events.lock().unwrap(), vec![Event::THIS_CONNECT], "nothing told off the carrier's thread");
    let mut wire = vec![];
    drain(&mut c, &mut tx, 64, &mut wire);
    assert!(wire.is_empty(), "no ABORT");
    assert!(c.end.gone() && c.end.drain() == 0);
    assert_eq!(*events.lock().unwrap(), vec![Event::THIS_CONNECT, Event::THIS_CLOSE_GRACEFUL]);
    c.end.wake_transmit();
    assert!(!c.queued.load(Ordering::SeqCst), "ended: nothing more is queued");
    let (heard, _) = carrier(6);
    heard.end.connected(true);
    heard.end.close();
    assert!(heard.end.drain() == 1 && !heard.end.gone(), "its end goes and waits for the answer");
    let (mut late, events) = carrier(7);
    let mut tx = TxState::new();
    late.end.connected(false);
    late.end.close();
    let mut got = Got::default();
    assert_eq!(late.end.on_part(&mut got, &[1; 3]), Some(3), "the far end spoke before the turn");
    let mut wire = vec![];
    drain(&mut late, &mut tx, 64, &mut wire);
    let f = frames(&wire);
    assert!(f.len() == 1 && f[0].2, "known now: its end goes with ABORT");
    assert_eq!(late.end.drain(), 1, "and waits for the answer");
    assert_eq!(*events.lock().unwrap(), vec![Event::THIS_CONNECT, Event::THIS_CLOSE_GRACEFUL]);
}

/// A session on a tunnel id after one that ends: it takes over the answers due to that one and sends
/// nothing before the end of it is on the wire.
#[test]
fn a_session_after_one_that_ends_goes_after_its_end() {
    let (mut old, _) = carrier(8);
    let (mut new, _) = carrier(8);
    let (mut txo, mut txn) = (TxState::new(), TxState::new());
    let mut wire = vec![];
    old.v.q.push_back(vec![1; 10]);
    old.end.wake_transmit();
    drain(&mut old, &mut txo, 64, &mut wire);
    old.end.close();
    new.end.follow(&old.end);
    assert_eq!(new.end.drain(), 1, "the answer due to the one before");
    new.v.q.push_back(vec![2; 10]);
    new.end.wake_transmit();
    assert!(!new.queued.load(Ordering::SeqCst), "nothing of it goes before the end of the one before");
    let mut got = Got::default();
    assert_eq!(new.end.on_part(&mut got, &[9; 4]), Some(4));
    assert!(got.0.is_empty(), "the far end's frames of the session before: dropped");
    drain(&mut old, &mut txo, 64, &mut wire);
    assert!(old.end.gone() && new.queued.load(Ordering::SeqCst), "the end of the one before is out: it goes");
    drain(&mut new, &mut txn, 64, &mut wire);
    let f = frames(&wire);
    assert_eq!(f.into_iter().map(|x| (x.1, x.2)).collect::<Vec<_>>(), vec![(vec![1; 10], false), (vec![], true), (vec![2; 10], false)]);
    assert!(new.end.answered() && new.end.drain() == 0);
    assert_eq!(new.end.on_part(&mut got, &[3; 2]), Some(2));
    assert_eq!(got.0, vec![3; 2], "the answer came: what comes now is of this session");
    let (late, _) = carrier(8);
    late.end.follow(&old.end);
    late.end.wake_transmit();
    assert!(late.queued.load(Ordering::SeqCst), "after one whose end is out: nothing to wait for");
}

/// The close of this side and the far end's end at the same moment are one decision: either the end
/// that came answers the close, or the close answers it - never a count that no answer will take.
#[test]
fn a_close_and_the_end_of_the_far_end_meet_in_one_decision() {
    let (first, _) = carrier(10);
    first.end.connected(true);
    assert!(!first.end.answered(), "no end of this side yet: the far end ended the tunnel");
    first.end.close();
    assert_eq!(first.end.drain(), 0, "the close answers: it waits for no answer");
    let (other, _) = carrier(11);
    other.end.connected(true);
    other.end.close();
    assert!(other.end.answered() && other.end.drain() == 0, "the end that came answers the close");
    for _ in 0..2_000 {
        let (c, _) = carrier(12);
        c.end.connected(true);
        let end = c.end.clone();
        let closing = std::thread::spawn(move || end.close());
        let answer = c.end.answered();
        closing.join().unwrap();
        assert_eq!(c.end.drain(), 0, "answered {answer}: nothing is left to wait for");
    }
}
