// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! A relay between two legs: frames in, frames out, the body unchanged, the regions re-framed.

use super::*;
use crate::lib::adhoc::connection::receiver::{RxHost, RxState};
use crate::lib::adhoc::connection::transmitter::TxHost;
use crate::lib::adhoc::connection::receiver::Receiver;
use crate::lib::adhoc::connection::transmitter::Transmitter;
use crate::lib::adhoc::{BytesDst, BytesSrc};
use std::collections::VecDeque;
use std::sync::Mutex;

const ID: u32 = 50;

struct Wake;

impl External for Wake {
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
}

/// The outbound leg: the relays queued on its transmitter.
#[derive(Default)]
struct Out {
    q: Mutex<(VecDeque<Arc<Relay>>, Option<Arc<Relay>>)>,
}

impl TxHost for &Out {
    fn on_serializing(&mut self) -> bool {
        let mut q = self.q.lock().unwrap();
        let r = q.0.pop_front();
        q.1 = r;
        q.1.is_some()
    }
    fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
        let r = self.q.lock().unwrap().1.clone().unwrap();
        r.get_bytes(dst)
    }
    fn on_serialized(&mut self) {
        self.q.lock().unwrap().1 = None;
    }
}

/// The inbound leg: a frame of the tunnel goes to the relay, routed to the outbound leg.
struct In {
    relay: Arc<Relay>,
    out: Arc<Out>,
    on: bool,
}

impl RxHost for In {
    fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
        self.on = id == ID;
        self.relay.arm(id, 1, 2, 1, None);
        self.on
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        let out = self.out.clone();
        self.relay.clone().put_bytes(src, move |r, _tid| {
            out.q.lock().unwrap().0.push_back(r.clone());
            Some(Arc::new(Wake) as Arc<dyn External>)
        }, |_| {})
    }
    fn on_received(&mut self, _st: &mut RxState) {}
}

/// A frame as a source writes it: chunks of `chunk` bytes.
fn frame(tid: u16, body: &[u8], chunk: usize, abort: bool) -> Vec<u8> {
    let mut f = vec![ID as u8];
    f.extend_from_slice(&tid.to_le_bytes());
    for c in body.chunks(chunk.max(1)) {
        f.extend_from_slice(&(c.len() as u16).to_le_bytes());
        f.extend_from_slice(c);
    }
    f.extend_from_slice(&(if abort { 0xFFFFu16 } else { 0 }).to_le_bytes());
    f
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

#[test]
fn relay_forwards_frames() {
    let bodies: Vec<(u16, Vec<u8>, bool)> = (0..12u16)
        .map(|i| (i * 257, (0..(i as usize * 37_777) % 200_000).map(|j| (j * 7 + i as usize) as u8).collect(), i % 5 == 4))
        .collect();
    let mut wire = vec![];
    for (i, (tid, b, abort)) in bodies.iter().enumerate() {
        wire.extend(frame(*tid, b, [1, 100, 65_534][i % 3], *abort));
    }
    for step in [1usize, 3, 1000, 70_000] {
        let out = Arc::new(Out::default());
        let mut sink = Receiver::new(In { relay: Relay::new(), out: out.clone(), on: false }, 1);
        let mut source = Transmitter::new(out.clone());
        let mut got = vec![];
        let mut drain = |source: &mut Transmitter<Out>, got: &mut Vec<u8>| loop {
            let mut o = Vec::with_capacity(5000);
            source.try_drain(&mut o);
            if o.is_empty() {
                break;
            }
            got.extend_from_slice(&o);
        };
        for w in wire.chunks(step) {
            // what the sink does not take stays here and is offered again
            let mut at = 0;
            let mut spins = 0;
            while at < w.len() {
                at += sink.on_part(&w[at..]).expect("the stream is broken");
                drain(&mut source, &mut got);
                spins += 1;
                assert!(spins < 1_000_000, "step {step}: the relay does not move");
            }
        }
        drain(&mut source, &mut got);
        assert!(frames(&got) == bodies, "step {step}");
    }
}

/// The receive buffer of a connection (`on_block`), as the engine runs it: a region that runs to
/// the end of the bytes received takes the buffer over and the connection reads on into the one the
/// relay put in its place; the outbound copies into its one transmit buffer - the frames come out
/// the same.
#[test]
fn relay_takes_receive_buffers_over() {
    use crate::lib::network::buf::RxBuf;
    let bodies: Vec<(u16, Vec<u8>, bool)> = (0..12u16)
        .map(|i| (i * 257, (0..(i as usize * 37_777) % 200_000).map(|j| (j * 7 + i as usize) as u8).collect(), i % 5 == 4))
        .collect();
    let mut wire = vec![];
    for (i, (tid, b, abort)) in bodies.iter().enumerate() {
        wire.extend(frame(*tid, b, [1, 100, 65_534][i % 3], *abort));
    }
    for step in [1usize, 3, 1000, 8192, 70_000] {
        let out = Arc::new(Out::default());
        let mut sink = Receiver::new(In { relay: Relay::new(), out: out.clone(), on: false }, 1);
        let mut source = Transmitter::new(out.clone());
        let mut got = vec![];
        let mut tbuf = vec![0u8; 1024];
        let drain = |source: &mut Transmitter<Out>, got: &mut Vec<u8>, tbuf: &mut Vec<u8>| loop {
            let n = crate::lib::adhoc::BytesSrc::read(source, tbuf);
            if n <= 0 {
                break;
            }
            got.extend_from_slice(&tbuf[..n as usize]);
        };
        let b = RxBuf::new(step);
        let mut taken = 0;
        for w in wire.chunks(step) {
            unsafe { b.slice_mut(0, w.len()) }.copy_from_slice(w);
            let (mut at, end) = (0, w.len());
            let mut spins = 0;
            while at < end {
                b.set_filled(end); // as the engine: a region running to here may take the buffer over
                let storage = b.as_ptr();
                at += sink.on_block(&b, at, end).expect("the stream is broken");
                if b.as_ptr() != storage {
                    taken += 1;
                }

                drain(&mut source, &mut got, &mut tbuf);
                spins += 1;
                assert!(spins < 1_000_000, "step {step}: the relay does not move");
            }
        }
        drain(&mut source, &mut got, &mut tbuf);
        assert!(frames(&got) == bodies, "step {step}");
        assert!(0 < taken, "step {step}: no receive buffer was taken over");
    }
}

/// The inbound connection as the engine runs it: woken by the relay (`wake_receive`), nothing else
/// retries a parked tail.
struct InboundWake(std::sync::atomic::AtomicBool);

impl External for InboundWake {
    fn is_open(&self) -> bool {
        true
    }
    fn wake_transmit(&self) {}
    fn wake_receive(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
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

/// The inbound leg of [`relay_survives_engine_wakes`]: the relay armed with the inbound connection
/// to wake.
struct InW {
    relay: Arc<Relay>,
    out: Arc<Out>,
    wake: Arc<InboundWake>,
    rx_id: usize,
}

impl RxHost for InW {
    fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
        self.relay.arm(id, 1, 2, self.rx_id, Some(self.wake.clone() as Arc<dyn External>));
        id & 0xFF == ID
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        let out = self.out.clone();
        self.relay.clone().put_bytes(src, move |r, _tid| {
            out.q.lock().unwrap().0.push_back(r.clone());
            Some(Arc::new(Wake) as Arc<dyn External>)
        }, |_| {})
    }
    fn on_received(&mut self, _st: &mut RxState) {}
}

/// The engine's rules, deterministically: windows of every size come through `on_slice` (a
/// WebSocket: the buffer is not handed over); a refused tail parks the receive and only the relay's
/// `wake_receive` offers it again; the outbound side drains a little at a time. Every frame must
/// come out, whatever the cut.
#[test]
fn relay_survives_engine_wakes() {
    let bodies: Vec<(u16, Vec<u8>, bool)> = (0..40u16)
        .map(|i| (1 + i % 3, (0..(i as usize * 9_973) % 30_000).map(|j| (j * 7 + i as usize) as u8).collect(), false))
        .collect();
    let mut wire = vec![];
    for (i, (tid, b, abort)) in bodies.iter().enumerate() {
        wire.extend(frame(*tid, b, [1, 100, 1000, 65_534][i % 4], *abort));
    }
    for rx_id in 1..=4usize {
    for step in [1usize, 2, 3, 4, 5, 7, 64, 1000, 1014] {
        for drain_room in [128usize, 200, 1014, 5000] {
            let out = Arc::new(Out::default());
            let wake = Arc::new(InboundWake(std::sync::atomic::AtomicBool::new(false)));
            let mut sink = Receiver::new(InW { relay: Relay::new(), out: out.clone(), wake: wake.clone(), rx_id }, rx_id as i32);
            let mut source = Transmitter::new(out.clone());
            let mut got = vec![];
            let mut drain_some = |source: &mut Transmitter<Out>, got: &mut Vec<u8>| {
                let mut o = vec![0u8; drain_room];
                let n = source.read(&mut o);
                if 0 < n {
                    got.extend_from_slice(&o[..n as usize]);
                }
                n
            };
            for w in wire.chunks(step) {
                let mut buf = w.to_vec();
                let mut at = 0;
                let mut idle = 0;
                while at < buf.len() {
                    let n = sink.on_slice(&mut buf[at..]).expect("the stream is broken");
                    at += n;
                    if at == buf.len() {
                        break;
                    }
                    if 0 < n {
                        continue;
                    }
                    // parked: only a wake offers the tail again
                    wake.0.store(false, std::sync::atomic::Ordering::Release);
                    loop {
                        let d = drain_some(&mut source, &mut got);
                        if wake.0.swap(false, std::sync::atomic::Ordering::AcqRel) {
                            break;
                        }
                        if d <= 0 {
                            idle += 1;
                            assert!(idle < 3, "rx_id {rx_id} step {step} room {drain_room}: parked, the outbound is dry and nobody wakes the receive");
                        }
                    }
                }
                drain_some(&mut source, &mut got);
            }
            for _ in 0..1_000_000 {
                if drain_some(&mut source, &mut got) < 0 {
                    break;
                }
            }
            assert!(frames(&got) == bodies, "rx_id {rx_id} step {step} room {drain_room}: {} of {} frames came out", frames(&got).len(), bodies.len());
        }
    }
    }
}

/// The inbound leg of the lifecycle tests: the outbound it routes to can be replaced (a new
/// connection after the old one closed).
struct InR {
    relay: Arc<Relay>,
    out: Arc<Mutex<Arc<Out>>>,
}

fn route_to(out: &Arc<Mutex<Arc<Out>>>) -> impl Fn(&Arc<Relay>, u32) -> Option<Arc<dyn External>> {
    let out = out.clone();
    move |r, _tid| {
        let o = out.lock().unwrap().clone();
        o.q.lock().unwrap().0.push_back(r.clone());
        r.serving(&o);
        Some(Arc::new(Wake) as Arc<dyn External>)
    }
}

/// The end of a tunnel whose source is gone, queued on the outbound its frames went to.
fn forward(out: &std::sync::Weak<dyn std::any::Any + Send + Sync>, frame: Arc<Relay>) {
    if let Some(o) = out.upgrade().and_then(|o| o.downcast::<Out>().ok()) {
        o.q.lock().unwrap().0.push_back(frame);
    }
}

impl RxHost for InR {
    fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
        self.relay.arm(id, 1, 2, 1, None);
        id == ID
    }
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        self.relay.clone().put_bytes(src, route_to(&self.out), |_| {})
    }
    fn on_received(&mut self, _st: &mut RxState) {}
    fn on_abandon(&mut self) {
        self.relay.inbound_lost();
    }
}

fn drain_all(source: &mut Transmitter<Out>, got: &mut Vec<u8>) {
    loop {
        let mut o = Vec::with_capacity(5000);
        source.try_drain(&mut o);
        if o.is_empty() {
            break;
        }
        got.extend_from_slice(&o);
    }
}

/// Feeds `w` to the sink in `step` cuts, the outbound drained after every one.
fn feed(sink: &mut Receiver<InR>, source: &mut Transmitter<Out>, got: &mut Vec<u8>, w: &[u8], step: usize) {
    for w in w.chunks(step) {
        let mut at = 0;
        let mut spins = 0;
        while at < w.len() {
            at += sink.on_part(&w[at..]).expect("the stream is broken");
            drain_all(source, got);
            spins += 1;
            assert!(spins < 1_000_000, "step {step}: the relay does not move");
        }
    }
}

/// The inbound is gone: the frame half out ends with ABORT, the far end of every other tunnel it
/// fed hears `[id][tunnel_id][ABORT]`, after the frames queued before.
#[test]
fn gone_ends_the_frame_and_bids_farewell() {
    let body = |n: usize, s: usize| -> Vec<u8> { (0..n).map(|j| (j * 13 + s) as u8).collect() };
    for step in [1usize, 7, 1000, 70_000] {
        let out = Arc::new(Mutex::new(Arc::new(Out::default())));
        let o = out.lock().unwrap().clone();
        let relay = Relay::new();
        let mut sink = Receiver::new(InR { relay: relay.clone(), out: out.clone() }, 1);
        let mut source = Transmitter::new(o.clone());
        let mut got = vec![];
        let mut wire = frame(1, &body(10, 1), 100, false);
        wire.extend(frame(5, &body(300, 5), 100, false));
        wire.extend(frame(1, &body(20, 2), 100, false)); // the same tunnel again: fed once
        feed(&mut sink, &mut source, &mut got, &wire, step);
        let half = frame(2, &body(50_000, 2), 1000, false);
        feed(&mut sink, &mut source, &mut got, &half[..20_000], step);
        relay.gone(forward);
        drain_all(&mut source, &mut got);
        let f = frames(&got);
        assert_eq!(f.len(), 6, "step {step}: {:?}", f.iter().map(|x| (x.0, x.1.len(), x.2)).collect::<Vec<_>>());
        assert!(f[0] == (1, body(10, 1), false) && f[1] == (5, body(300, 5), false) && f[2] == (1, body(20, 2), false), "step {step}");
        assert!(f[3].0 == 2 && f[3].2 && body(50_000, 2).starts_with(&f[3].1), "step {step}: the frame half out ends with ABORT");
        let mut bye: Vec<(u16, Vec<u8>, bool)> = f[4..].to_vec();
        bye.sort();
        assert!(bye == vec![(1, vec![], true), (5, vec![], true)], "step {step}: a farewell to every other tunnel fed");

        // a frame routed, nothing of it out yet: it vanishes, and its tunnel hears no farewell
        let relay = Relay::new();
        let o = Arc::new(Out::default());
        *out.lock().unwrap() = o.clone();
        let mut sink = Receiver::new(InR { relay: relay.clone(), out: out.clone() }, 1);
        let mut source = Transmitter::new(o.clone());
        let mut got = vec![];
        let mut wire = frame(7, &body(30, 7), 100, false);
        wire.extend_from_slice(&frame(3, &body(30, 3), 100, false)[..3]); // [id][tunnel_id]: routed, no body yet
        feed(&mut sink, &mut source, &mut got, &wire, step);
        relay.gone(forward);
        drain_all(&mut source, &mut got);
        let f = frames(&got);
        assert!(f.first() == Some(&(7, body(30, 7), false)), "step {step}");
        assert!(f[1..] == [(7, vec![], true)], "step {step}: {:?}", f.iter().map(|x| (x.0, x.1.len(), x.2)).collect::<Vec<_>>());
    }
}

/// The outbound closes holding the frame: the receive drops the rest of that frame, and the next
/// one goes out whole on the connection the router gives then.
#[test]
fn outbound_lost_drops_the_rest_of_the_frame() {
    let body = |n: usize, s: usize| -> Vec<u8> { (0..n).map(|j| (j * 11 + s) as u8).collect() };
    for step in [1usize, 7, 1000, 70_000] {
        for cut in [3usize, 4, 500, 20_000, 49_000] {
            let first = Arc::new(Out::default());
            let out = Arc::new(Mutex::new(first.clone()));
            let relay = Relay::new();
            let mut sink = Receiver::new(InR { relay: relay.clone(), out: out.clone() }, 1);
            let mut source = Transmitter::new(first.clone());
            let mut got = vec![];
            let lost = frame(1, &body(50_000, 1), 1000, false);
            feed(&mut sink, &mut source, &mut got, &lost[..cut], step);
            // the outbound connection closes: what it holds and what is queued on it are discarded
            {
                let q = &mut *first.q.lock().unwrap();
                for r in q.1.take().into_iter().chain(q.0.drain(..)) {
                    r.outbound_lost();
                }
            }
            let second = Arc::new(Out::default());
            *out.lock().unwrap() = second.clone();
            let mut source = Transmitter::new(second.clone());
            let mut got = vec![];
            let mut wire = lost[cut..].to_vec();
            wire.extend(frame(2, &body(3000, 2), 100, false));
            wire.extend(frame(3, &body(10, 3), 100, true));
            feed(&mut sink, &mut source, &mut got, &wire, step);
            let f = frames(&got);
            assert!(f == vec![(2, body(3000, 2), false), (3, body(10, 3), true)], "step {step} cut {cut}: {:?}", f.iter().map(|x| (x.0, x.1.len(), x.2)).collect::<Vec<_>>());
        }
    }
}

/// The receiver drops the frame in flight and the inbound goes on: the frame half out ends with
/// ABORT, one not out yet vanishes, the frames that follow go out whole.
#[test]
fn a_frame_abandoned_mid_way_ends_and_the_stream_goes_on() {
    let body = |n: usize, s: usize| -> Vec<u8> { (0..n).map(|j| (j * 7 + s) as u8).collect() };
    for step in [1usize, 7, 1000, 70_000] {
        for cut in [3usize, 4, 500, 20_000] {
            let o = Arc::new(Out::default());
            let out = Arc::new(Mutex::new(o.clone()));
            let relay = Relay::new();
            let mut sink = Receiver::new(InR { relay: relay.clone(), out: out.clone() }, 1);
            let mut source = Transmitter::new(o.clone());
            let mut got = vec![];
            let half = frame(2, &body(50_000, 2), 1000, false);
            feed(&mut sink, &mut source, &mut got, &half[..cut], step);
            BytesDst::abandon(&mut sink);
            let mut wire = frame(4, &body(3000, 4), 100, false);
            wire.extend(frame(6, &body(10, 6), 100, true));
            feed(&mut sink, &mut source, &mut got, &wire, step);
            drain_all(&mut source, &mut got);
            let f = frames(&got);
            let shown = f.iter().map(|x| (x.0, x.1.len(), x.2)).collect::<Vec<_>>();
            let rest = if cut == 3 {
                &f[..] // routed, nothing of it out: it vanishes
            } else {
                assert!(f[0].0 == 2 && f[0].2 && body(50_000, 2).starts_with(&f[0].1), "step {step} cut {cut}: {shown:?}");
                &f[1..]
            };
            assert!(rest == [(4, body(3000, 4), false), (6, body(10, 6), true)], "step {step} cut {cut}: {shown:?}");
        }
    }
}

/// The inbound on the same loop as the outbound: its wake runs its receive at once, which arms this
/// relay for the next frame
/// - the wake holds no lock of the relay then (it would be taken twice by one thread).
#[test]
fn a_wake_that_runs_the_receive_at_once_arms_the_relay() {
    struct Reenter(std::sync::Mutex<Option<std::sync::Weak<Relay>>>, std::sync::atomic::AtomicUsize);
    impl External for Reenter {
        fn is_open(&self) -> bool {
            true
        }
        fn wake_transmit(&self) {}
        fn wake_receive(&self) {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let r = self.0.lock().unwrap().as_ref().and_then(|w| w.upgrade());
            if let Some(r) = r {
                r.arm(ID, 1, 2, 1, None); // the receive of the next frame, on this very thread
            }
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
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let relay = Relay::new();
        let inbound = Arc::new(Reenter(std::sync::Mutex::new(Some(Arc::downgrade(&relay))), std::sync::atomic::AtomicUsize::new(0)));
        relay.arm(ID, 1, 2, 1, Some(inbound.clone() as Arc<dyn External>));
        relay.wake_in();
        let _ = tx.send(inbound.1.load(std::sync::atomic::Ordering::Relaxed));
    });
    assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(1), "the wake took the lock of the relay the receive takes again");
}

/// A tunnel its source ended with ABORT is over for the far end already: when the inbound goes,
/// only the tunnels still open hear a farewell - a farewell to the ended one would close a new
/// tunnel of the same id.
#[test]
fn a_tunnel_its_source_ended_hears_no_farewell() {
    let body = |n: usize, s: usize| -> Vec<u8> { (0..n).map(|j| (j * 5 + s) as u8).collect() };
    let o = Arc::new(Out::default());
    let out = Arc::new(Mutex::new(o.clone()));
    let relay = Relay::new();
    let mut sink = Receiver::new(InR { relay: relay.clone(), out: out.clone() }, 1);
    let mut source = Transmitter::new(o.clone());
    let mut got = vec![];
    let mut wire = frame(1, &body(30, 1), 100, false);
    wire.extend(frame(5, &body(30, 5), 100, true)); // the source ends tunnel 5
    feed(&mut sink, &mut source, &mut got, &wire, 1000);
    relay.gone(forward);
    drain_all(&mut source, &mut got);
    let f = frames(&got);
    let bye: Vec<u16> = f.iter().filter(|x| x.1.is_empty() && x.2).map(|x| x.0).collect();
    assert_eq!(bye, vec![1], "{:?}", f.iter().map(|x| (x.0, x.1.len(), x.2)).collect::<Vec<_>>());
}

/// The id field of the receiver reads the frame to its end (an id of 4 bytes: the pack id, a
/// `tunnel_id` of 1 byte and a terminator - a farewell, an empty frame): it goes out at once, not
/// when bytes of the next frame come.
#[test]
fn a_frame_that_ends_in_the_bytes_read_with_its_id_goes_out_at_once() {
    struct Wide {
        relay: Arc<Relay>,
        out: Arc<Out>,
    }
    impl RxHost for Wide {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            self.relay.arm(id, 1, 1, 4, None);
            id & 0xFF == ID
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            let out = self.out.clone();
            self.relay.clone().put_bytes(src, move |r, _tid| {
                out.q.lock().unwrap().0.push_back(r.clone());
                Some(Arc::new(Wake) as Arc<dyn External>)
            }, |_| {})
        }
        fn on_received(&mut self, _st: &mut RxState) {}
    }
    for (wire, want) in [
        (vec![ID as u8, 7, 0, 0], vec![ID as u8, 7, 0, 0]),             // empty: the terminator in the id
        (vec![ID as u8, 7, 0xFF, 0xFF], vec![ID as u8, 7, 0xFF, 0xFF]), // a farewell: ABORT in the id
    ] {
        let out = Arc::new(Out::default());
        let mut sink = Receiver::new(Wide { relay: Relay::new(), out: out.clone() }, 4);
        let mut source = Transmitter::new(out.clone());
        assert_eq!(sink.on_part(&wire).expect("the stream is broken"), wire.len());
        let mut got = vec![];
        source.try_drain(&mut got);
        assert_eq!(got, want, "nothing more came: the frame is out all the same");
    }
}

/// A relay closed half way through a frame: what is out of that frame ends with `ABORT`; the frames
/// that come to it after are read to their end and go nowhere - the stream of the inbound goes on.
#[test]
fn a_closed_relay_ends_its_frame_and_lets_nothing_through() {
    let out = Arc::new(Out::default());
    let relay = Relay::new();
    let mut sink = Receiver::new(In { relay: relay.clone(), out: out.clone(), on: false }, 1);
    let mut source = Transmitter::new(out.clone());
    let a = frame(5, &[1; 300], 100, false);
    let b = frame(6, &[2; 50], 100, false);
    let half = 3 + 2 + 100 + 2 + 50; // the prefix, a chunk and a half
    let mut got = vec![];
    let fed = |sink: &mut Receiver<In>, source: &mut Transmitter<Out>, got: &mut Vec<u8>, w: &[u8]| {
        let mut at = 0;
        let mut spins = 0;
        while at < w.len() {
            at += sink.on_part(&w[at..]).expect("the stream is broken");
            source.try_drain(got); // the gate opens as the transmit ends the frame
            spins += 1;
            assert!(spins < 10_000, "the relay does not move");
        }
    };
    fed(&mut sink, &mut source, &mut got, &a[..half]);
    assert!(relay.is_open());
    relay.close();
    assert!(!relay.is_open());
    fed(&mut sink, &mut source, &mut got, &a[half..]);
    fed(&mut sink, &mut source, &mut got, &b);
    loop {
        let n = got.len();
        source.try_drain(&mut got);
        if got.len() == n {
            break;
        }
    }
    let f = frames(&got);
    assert_eq!(f.len(), 1, "only the frame half through");
    assert!(f[0].0 == 5 && f[0].2 && f[0].1.iter().all(|&x| x == 1), "it ends with ABORT");
}

/// A tunnel nothing serves: when its frame is over, its source hears the end of it - once, till the
/// source answers with its own end, and never for a frame that is an end itself. A target that
/// refused the frame ( a full queue ) is no reason for it.
#[test]
fn a_tunnel_nothing_serves_hears_its_end_once() {
    struct Lost {
        relay: Arc<Relay>,
        back: Arc<Out>,
        /// The tunnel whose target refuses.
        refuses: u16,
    }
    impl RxHost for Lost {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            self.relay.arm(id, 1, 2, 1, None);
            id == ID
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            let (back, refuses) = (self.back.clone(), self.refuses);
            self.relay.clone().put_bytes(
                src,
                move |r, tid| {
                    if tid == refuses as u32 {
                        r.refused();
                    }
                    None
                },
                move |f| back.q.lock().unwrap().0.push_back(f),
            )
        }
        fn on_received(&mut self, _st: &mut RxState) {}
    }
    let back = Arc::new(Out::default());
    let relay = Relay::new();
    let mut sink = Receiver::new(Lost { relay: relay.clone(), back: back.clone(), refuses: 8 }, 1);
    let mut source = Transmitter::new(back.clone());
    let mut wire = frame(7, &[1, 2, 3], 2, false);
    wire.extend(frame(7, &[4], 1, false)); // told already: nothing
    wire.extend(frame(9, &[5], 1, false)); // another tunnel: told
    wire.extend(frame(8, &[6], 1, false)); // its target refused: nothing
    wire.extend(frame(7, &[], 1, true)); // the source answered: nothing, and 7 is forgotten
    wire.extend(frame(7, &[7], 1, false)); // its next session finds no target: told again
    for w in wire.chunks(3) {
        assert!(sink.on_bytes(w));
    }
    let mut got = vec![];
    source.try_drain(&mut got);
    let f = frames(&got);
    assert_eq!(f, vec![(7, vec![], true), (9, vec![], true), (7, vec![], true)]);
}

/// The inbound is gone: the far end of every tunnel it fed hears the end on the outbound its frames
/// went to, with the id they had there - a router that rewrote the id to the source's address would
/// send a new routing back to the source; two tunnels to different far ends known by one id are two.
#[test]
fn gone_tells_each_far_end_where_its_frames_went() {
    struct Two {
        relay: Arc<Relay>,
        outs: [Arc<Out>; 2],
    }
    impl RxHost for Two {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            self.relay.arm(id, 1, 2, 1, None);
            id == ID
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            let outs = self.outs.clone();
            self.relay.clone().put_bytes(
                src,
                move |r, tid| {
                    let o = &outs[(tid & 1) as usize];
                    r.set_tunnel_id(9); // the far end knows the tunnel by the address of its source
                    o.q.lock().unwrap().0.push_back(r.clone());
                    r.serving(o);
                    Some(Arc::new(Wake) as Arc<dyn External>)
                },
                |_| {},
            )
        }
        fn on_received(&mut self, _st: &mut RxState) {}
    }
    let outs = [Arc::new(Out::default()), Arc::new(Out::default())];
    let relay = Relay::new();
    let mut sink = Receiver::new(Two { relay: relay.clone(), outs: outs.clone() }, 1);
    let mut sources = [Transmitter::new(outs[0].clone()), Transmitter::new(outs[1].clone())];
    let mut got = [vec![], vec![]];
    let mut wire = frame(4, &[1; 10], 100, false); // to the first far end
    wire.extend(frame(5, &[2; 10], 100, false)); // to the second
    wire.extend(frame(4, &[3; 10], 100, false));
    for w in wire.chunks(7) {
        let mut at = 0;
        while at < w.len() {
            at += sink.on_part(&w[at..]).expect("the stream is broken");
            for i in 0..2 {
                drain_all(&mut sources[i], &mut got[i]);
            }
        }
    }
    relay.gone(forward);
    for i in 0..2 {
        drain_all(&mut sources[i], &mut got[i]);
    }
    let (a, b) = (frames(&got[0]), frames(&got[1]));
    assert_eq!(a, vec![(9, vec![1; 10], false), (9, vec![3; 10], false), (9, vec![], true)]);
    assert_eq!(b, vec![(9, vec![2; 10], false), (9, vec![], true)]);
}

/// A tunnel through two relays, a circuit: the first relay ( `through` ) names it on its outbound by a label of its own and opens
/// it there with the first frame - `[id][mark][label][tunnel id there]` -, the second gives the end a label of its own; what the end
/// sends back on that label goes back to the first relay under the first label.
#[test]
fn relay_opens_circuits() {
    struct Dialed;
    impl External for Dialed {
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
        fn dialing(&self) -> bool {
            true // the far relay dialed: its labels are the odd ones there, the ones of this side even
        }
    }
    /// A relay of one leg: `mode` 0 - through to tunnel id 7 on the next relay, 1 - as the route sends it, 2 - by its circuit only.
    struct Leg {
        relay: Arc<Relay>,
        out: Arc<Out>,
        me: Arc<u8>,
        to: Arc<u8>,
        mode: u8,
    }
    impl RxHost for Leg {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            self.relay.arm(id, 1, 2, 1, Some(if self.mode == 1 { Arc::new(Dialed) as Arc<dyn External> } else { Arc::new(Wake) as Arc<dyn External> }));
            self.relay.carrier(&self.me);
            id == ID
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            let (out, to, mode) = (self.out.clone(), self.to.clone(), self.mode);
            self.relay.clone().put_bytes(
                src,
                move |r, tid| {
                    let target = match mode {
                        0 => r.through(to.clone() as Arc<dyn std::any::Any + Send + Sync>, 7),
                        1 => r.circuit(tid).unwrap_or(Some(to.clone() as Arc<dyn std::any::Any + Send + Sync>)),
                        _ => r.circuit(tid).expect("a frame of the circuit"),
                    };
                    target.as_ref()?;
                    r.serving(&to);
                    out.q.lock().unwrap().0.push_back(r.clone());
                    Some(Arc::new(Wake) as Arc<dyn External>)
                },
                |_| {},
            )
        }
        fn on_received(&mut self, _st: &mut RxState) {}
    }
    let pump = |leg: Leg, wire: &[u8]| -> Vec<u8> {
        let out = leg.out.clone();
        let mut sink = Receiver::new(leg, 1);
        let mut source = Transmitter::new(out);
        let mut got = vec![];
        for w in wire.chunks(3) {
            let mut at = 0;
            while at < w.len() {
                at += sink.on_part(&w[at..]).expect("the stream is broken");
                drain_all(&mut source, &mut got);
            }
        }
        drain_all(&mut source, &mut got);
        got
    };
    super::super::labels(0x4000, 0x100);
    let (a, n1, n2, b) = (Arc::new(1u8), Arc::new(2u8), Arc::new(3u8), Arc::new(4u8)); // the connections: A - N1 - N2 - B

    // N1: A's frames of tunnel 5 go through to tunnel 7 on N2
    let mut wire = frame(5, b"hello", 2, false);
    wire.extend(frame(5, b"again", 100, false));
    wire.extend(frame(5, b"", 100, true));
    let on = pump(Leg { relay: Relay::new(), out: Arc::new(Out::default()), me: a.clone(), to: n2.clone(), mode: 0 }, &wire);
    assert_eq!(&on[..3], &[ID as u8, 0xFF, 0xFF], "the first frame opens the circuit");
    let label = u16::from_le_bytes([on[3], on[4]]);
    assert_eq!(u16::from_le_bytes([on[5], on[6]]), 7, "the tunnel id on the far relay");
    assert!((0x4000..0x4100).contains(&label) && label & 1 == 0, "a label of this side: {label:#x}");
    let mut plain = vec![on[0]];
    plain.extend_from_slice(&on[3..5]);
    plain.extend_from_slice(&on[7..]);
    assert_eq!(frames(&plain), vec![(label, b"hello".to_vec(), false), (label, b"again".to_vec(), false), (label, vec![], true)]);

    // N2: the open names tunnel 7 - the end gets a label of N2
    let at_b = pump(Leg { relay: Relay::new(), out: Arc::new(Out::default()), me: n1.clone(), to: b.clone(), mode: 1 }, &on);
    let got = frames(&at_b);
    let m = got[0].0;
    assert_ne!(m, label);
    assert_eq!(got, vec![(m, b"hello".to_vec(), false), (m, b"again".to_vec(), false), (m, vec![], true)]);

    // B answers on its label: back to N1 under N1's label, and its end closes the circuit
    let mut back = frame(m, b"heard", 100, false);
    back.extend(frame(m, b"", 100, true));
    let at_n1 = pump(Leg { relay: Relay::new(), out: Arc::new(Out::default()), me: b.clone(), to: n1.clone(), mode: 2 }, &back);
    assert_eq!(frames(&at_n1), vec![(label, b"heard".to_vec(), false), (label, vec![], true)]);
    let m_index = ((m as u32 - 0x4000) >> 1) as u32;
    assert!(!labels_().lock().given.contains_key(&m_index), "both sides ended: the circuit of N2 is over");
}
