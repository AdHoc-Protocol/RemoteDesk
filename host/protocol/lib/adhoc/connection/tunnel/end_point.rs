// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! The end of a tunnel: the external connection of a virtual connection that rides the frames of
//! its carrier. Waking its transmitter queues one burst on the carrier ([`EndPoint::set_enqueue`]),
//! and the end is told the events of the tunnel. A frame on the carrier is
//! `[pack id][tunnel_id?][len u16 LE][bytes] … [0]`; `ABORT` in place of `[0]` ends the tunnel.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use crate::lib::adhoc::connection::transmitter::chunked::MAX;
use crate::lib::adhoc::chain::Stage;
use crate::lib::adhoc::connection::external::{Event, External};
use crate::lib::adhoc::connection::receiver::Rx;
use crate::lib::adhoc::connection::transmitter::{Tx, TxHost, TxState};
use crate::lib::adhoc::BytesDst;
use crate::lib::chain::Chain;
use crate::lib::network::noise_adapter::NoiseHandle;
use crate::lib::noise::KEY;

type Enqueue = Arc<dyn Fn() -> bool + Send + Sync>;

/// The bit of [`EndPoint::drain`]'s word: the far end ended the tunnel, the end of this side answers it.
const ANSWERING: u32 = 1 << 31;
type Events = Arc<dyn Fn(Event) + Send + Sync>;
type Resume = Arc<dyn Fn() + Send + Sync>;

/// The external connection of a virtual connection: a burst at a time on the carrier.
///
/// A tunnel ends when both ends have sent their end (`ABORT`), as an SSH channel does: the end that
/// is told the far end's end answers with its own. Till the answer to an end of this side comes, the
/// tunnel id stays taken and what arrives on it is of the session that ended - frames sent before
/// the far end saw the end - and is dropped ([`drain`](Self::drain)). So a frame of a tunnel id that
/// is not taken always starts a new session.
pub struct EndPoint {
    tunnel_id: AtomicU32,
    /// A burst is queued on the carrier or in flight.
    in_use: AtomicBool,
    /// The transmitter was woken since the burst in flight was armed: one more burst is due after
    /// it.
    wanted: AtomicBool,
    /// `close()` came: the next burst ends the tunnel.
    closing: AtomicBool,
    /// The tunnel has not ended yet: its end is told once.
    open: AtomicBool,
    /// The burst in flight stopped on a pack of the virtual connection that has nothing to give now
    /// ([`Tx::hold`]): the carrier sleeps on it, and the wake of that pack's producer wakes the
    /// carrier.
    parked: AtomicBool,
    receive_timeout: AtomicI32,
    transmit_timeout: AtomicI32,
    /// The hooks: set once, when the tunnel is made - read with no lock and no clone at every
    /// burst.
    enqueue: std::sync::OnceLock<Enqueue>,
    carrier: std::sync::OnceLock<Weak<dyn External>>,
    events: std::sync::OnceLock<Events>,
    resume: std::sync::OnceLock<Resume>,
    forget: std::sync::OnceLock<Resume>,
    /// The Noise stream between the virtual connection and the tunnel ([`secure`](Self::secure)):
    /// set once, before the tunnel carries anything; none - the bytes go as they are.
    noise: std::sync::OnceLock<NoiseHandle>,
    /// The key the far end proved by the handshake, once it is accepted: read with no lock - a
    /// handler of the virtual connection asks it while the stream is held.
    peer: std::sync::OnceLock<[u8; KEY]>,
    /// The handshake moved on while the stream was in use, or its first message waits for the start of the tunnel: its wake is
    /// done once the stream is let go - the transmit it wakes uses the stream too.
    woken: AtomicBool,
    /// Ends of this side on this tunnel id - its own, and those of the session before it on the id -
    /// the far end has not answered yet ([`drain`](Self::drain)); and [`ANSWERING`]: the far end
    /// ended the tunnel, the end of this side answers it and no answer comes to it. One word: the
    /// count of [`close`](External::close) and the look of [`answered`](Self::answered) are one
    /// decision.
    drain: AtomicU32,
    /// This side decided to end the tunnel ([`close`](External::close)): once.
    ending: AtomicBool,
    /// A burst of this session went out; a frame of the far end came on it. Neither - the far end
    /// does not know the session, and its end tells nothing: [`close`](External::close) ends it
    /// with no `ABORT`.
    said: AtomicBool,
    heard: AtomicBool,
    /// [`close`](External::close) of a session the far end does not know: its next turn on the
    /// carrier ends it there and sends nothing ([`silent`](Self::silent)).
    silent: AtomicBool,
    /// The end of this side is on the wire.
    gone: AtomicBool,
    /// The session before this one on the tunnel id ([`follow`](Self::follow)): this one sends
    /// nothing before the end of it is on the wire.
    after: std::sync::OnceLock<Weak<EndPoint>>,
    /// The wait for the end of [`after`](Self::after) is not over.
    waiting: AtomicBool,
    /// The session after this one on the tunnel id, waiting for the end of this one.
    next: std::sync::OnceLock<Weak<EndPoint>>,
    /// The identity the session of this tunnel was given ([`identify`](External::identify)); 0 - none.
    /// Not the tunnel id: that is the address of the far end.
    ident: AtomicU32,
    /// The number of this session among the tunnel sessions of the process, given at its first
    /// identify: the slot its token names. 0 - none yet.
    slot: AtomicU32,
    /// A [`parked::now`](super::parked::now) deadline, in seconds: till then the session of this tunnel,
    /// lost with it, is parked and waits for a reclaim on a new tunnel. 0 - not parked.
    resumable: AtomicI64,
    /// The parked session let go of what the lost tunnel held ([`settle`](Self::set_settle)): once.
    settled: AtomicBool,
    /// The seconds a lost session of the virtual connection waits for a reclaim ( its
    /// `resumable_seconds` ); 0 - it does not park. Once.
    seconds: std::sync::OnceLock<Arc<dyn Fn(Event) -> i32 + Send + Sync>>,
    /// The lost session parks: the virtual connection hears `on_close`, its close event waits. Once.
    park_hook: std::sync::OnceLock<Resume>,
    /// The parked session lets go of the lost tunnel and waits ( [`parked::put`](super::parked::put) ). Once.
    settle: std::sync::OnceLock<Resume>,
}

impl EndPoint {
    pub fn new(tunnel_id: u32) -> Arc<Self> {
        Arc::new(Self {
            tunnel_id: AtomicU32::new(tunnel_id),
            in_use: AtomicBool::new(false),
            wanted: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            open: AtomicBool::new(true),
            parked: AtomicBool::new(false),
            receive_timeout: AtomicI32::new(i32::MAX),
            transmit_timeout: AtomicI32::new(i32::MAX),
            enqueue: std::sync::OnceLock::new(),
            carrier: std::sync::OnceLock::new(),
            events: std::sync::OnceLock::new(),
            resume: std::sync::OnceLock::new(),
            forget: std::sync::OnceLock::new(),
            noise: std::sync::OnceLock::new(),
            peer: std::sync::OnceLock::new(),
            woken: AtomicBool::new(false),
            drain: AtomicU32::new(0),
            ending: AtomicBool::new(false),
            said: AtomicBool::new(false),
            heard: AtomicBool::new(false),
            silent: AtomicBool::new(false),
            gone: AtomicBool::new(false),
            after: std::sync::OnceLock::new(),
            waiting: AtomicBool::new(false),
            next: std::sync::OnceLock::new(),
            ident: AtomicU32::new(0),
            slot: AtomicU32::new(0),
            resumable: AtomicI64::new(0),
            settled: AtomicBool::new(false),
            seconds: std::sync::OnceLock::new(),
            park_hook: std::sync::OnceLock::new(),
            settle: std::sync::OnceLock::new(),
        })
    }

    /// Ends of this side on this tunnel id - its own, and those of the session before it on the id -
    /// the far end has not answered yet. While there are any, what arrives on the id is of a session
    /// that ended: this end takes it and drops it ([`on_part`](Self::on_part)). Counted by
    /// [`close`](External::close), taken off under the lock of the carrier's tunnels.
    #[inline]
    pub fn drain(&self) -> u32 {
        self.drain.load(Ordering::Acquire) & !ANSWERING
    }

    /// The end of the far end came, under the lock of the carrier's tunnels. `true` - it answers an
    /// end of this side: nothing to tell, and the tunnel id is free once [`drain`](Self::drain) is 0
    /// and the tunnel is [`gone`](Self::gone). `false` - the far end ended the tunnel: tell the
    /// virtual connection ([`closed`](Self::closed)), which answers.
    pub fn answered(&self) -> bool {
        // none due: the tunnel stays on its id till its answer is out - a frame after the far end's end is of its next session
        let was = self.drain.try_update(Ordering::AcqRel, Ordering::Acquire, |d| Some(if d & !ANSWERING == 0 { d | ANSWERING } else { d - 1 })).unwrap_or_default();
        was & !ANSWERING != 0
    }

    /// The end of this side is on the wire: the id is free once [`drain`](Self::drain) is 0.
    #[inline]
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// This session follows `was` on the tunnel id, under the lock of the carrier's tunnels: it takes
    /// over the answers due to it, and sends nothing before the end of it is on the wire - a frame of
    /// this one never comes ahead of that end.
    pub fn follow(self: &Arc<Self>, was: &Arc<EndPoint>) {
        self.drain.store(was.drain(), Ordering::Release);
        if was.gone.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.after.set(Arc::downgrade(was));
        self.waiting.store(true, Ordering::SeqCst);
        let _ = was.next.set(Arc::downgrade(self));
    }

    /// The first look of [`wake_transmit`](External::wake_transmit): `true` - the session before this
    /// one on the id has not sent its end yet, the wake waits for it ([`go`](Self::go)).
    #[inline]
    fn held(&self) -> bool {
        if !self.waiting.load(Ordering::Acquire) {
            return false;
        }
        self.wanted.store(true, Ordering::SeqCst); // the wish is out before the end of the one before is looked at: one of the two sees the other
        if self.after.get().and_then(Weak::upgrade).is_some_and(|was| !was.gone.load(Ordering::SeqCst)) {
            return true;
        }
        self.waiting.store(false, Ordering::Release);
        false
    }

    /// The end of this session is on the wire: the session after it on the id, when there is one,
    /// sends what waited.
    fn went(&self) {
        self.gone.store(true, Ordering::SeqCst);
        if let Some(n) = self.next.get().and_then(Weak::upgrade) {
            n.go();
        }
    }

    /// The end of the session before this one is on the wire: what waited goes.
    fn go(&self) {
        if self.waiting.swap(false, Ordering::SeqCst) && self.wanted.load(Ordering::SeqCst) {
            self.wake_transmit();
        }
    }

    /// The id of the tunnel on the carrier: the identity of the far end.
    #[inline]
    pub fn tunnel_id(&self) -> u32 {
        self.tunnel_id.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_tunnel_id(&self, id: u32) {
        self.tunnel_id.store(id, Ordering::Relaxed)
    }

    /// What queues a burst of this end on its carrier: `false` — refused, the queue is full or the
    /// carrier closes. Once, when the tunnel is made: a second one is ignored.
    pub fn set_enqueue(&self, f: impl Fn() -> bool + Send + Sync + 'static) {
        let _ = self.enqueue.set(Arc::new(f));
    }

    /// The external connection of the carrier: it times the deadlines of the instances of the
    /// virtual connection. Once.
    pub fn set_carrier(&self, carrier: Weak<dyn External>) {
        let _ = self.carrier.set(carrier);
    }

    /// The external connection of the carrier - the socket the tunnel came on; `None` - not set yet, or gone.
    pub fn carrier(&self) -> Option<Arc<dyn External>> {
        self.carrier.get().and_then(Weak::upgrade)
    }

    /// Who hears the events of the tunnel - its start ([`connected`](Self::connected)) and its end: the
    /// virtual connection. Once.
    pub fn set_events(&self, f: impl Fn(Event) + Send + Sync + 'static) {
        let _ = self.events.set(Arc::new(f));
    }

    /// What the virtual connection does when its session is named ([`identify`](External::identify)):
    /// its `resume` - the journal goes online, the receiving side names its position. Once.
    pub fn set_resume(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.resume.set(Arc::new(f));
    }

    /// How long, in seconds, a lost session of the virtual connection waits for a reclaim on a new
    /// tunnel: its `resumable_seconds`. Once.
    pub fn set_seconds(&self, f: impl Fn(Event) -> i32 + Send + Sync + 'static) {
        let _ = self.seconds.set(Arc::new(f));
    }

    /// What the virtual connection does when its lost session parks: `on_close`, its close event
    /// waits for the end of the wait. Once.
    pub fn set_park(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.park_hook.set(Arc::new(f));
    }

    /// What lets the parked session go of the lost tunnel - its journal parks, its receiver keeps its
    /// position - and puts it on the list of the parked ( [`parked::put`](super::parked::put) ): when
    /// the tunnel has nothing more to send. Once.
    pub fn set_settle(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.settle.set(Arc::new(f));
    }

    /// The slot of the session of this tunnel: what its token names.
    #[inline]
    pub fn slot(&self) -> u32 {
        self.slot.load(Ordering::Acquire)
    }

    /// The deadline of the parked session, [`parked::now`](super::parked::now) seconds; 0 - not parked.
    #[inline]
    pub fn resumable(&self) -> i64 {
        self.resumable.load(Ordering::Acquire)
    }

    /// The session parked on `parked` goes on in this tunnel ( a reclaim ): it keeps its slot - the
    /// token the peer holds names it - and is identified again.
    pub fn took(&self, parked: &EndPoint) {
        self.slot.store(parked.slot(), Ordering::Release);
        self.ident.store(0, Ordering::Release);
    }

    /// The wait of the parked session ran out.
    pub fn reaped(&self) {
        self.resumable.store(0, Ordering::Release);
    }

    /// Once: the parked session lets go of the lost tunnel.
    fn settle_once(&self) {
        if !self.settled.swap(true, Ordering::AcqRel) {
            if let Some(f) = self.settle.get() {
                f();
            }
        }
    }

    /// What lets the carrier forget the tunnel, called at every close of it ([`closed`](Self::closed)),
    /// after the virtual connection is told: the carrier forgets it once its end is answered and on
    /// the wire ( [`drain`](Self::drain) 0, [`gone`](Self::gone) ). Once.
    pub fn set_forget(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.forget.set(Arc::new(f));
    }

    /// The tunnel started: `remote` - its first frame came from the far end, else this side opened it.
    /// The virtual connection hears the connect event as on a socket (`REMOTE_CONNECT` / `THIS_CONNECT`):
    /// between two hosts of one type it takes by it the half of the instance ids that is its own - the
    /// side that opened the tunnel is the left one.
    pub fn connected(&self, remote: bool) {
        if remote {
            self.heard.store(true, Ordering::Release); // a frame of the far end: it knows this session
        }
        if self.woken.load(Ordering::Acquire) && self.woken.swap(false, Ordering::AcqRel) {
            self.wake_transmit(); // the first message of the handshake ( `secure` )
        }
        if let Some(f) = self.events.get() {
            f(if remote { Event::REMOTE_CONNECT } else { Event::THIS_CONNECT });
        }
    }

    /// The Noise stream between the virtual connection and the tunnel ([`secure`](Self::secure)):
    /// none - the bytes go as they are. The transmit and the receive of the tunnel use it while the
    /// packs of the virtual connection go through, on the loop of the carrier: not for its handlers.
    #[inline]
    pub(crate) fn noise(&self) -> Option<&NoiseHandle> {
        self.noise.get()
    }

    /// The key the far end proved by the handshake of the tunnel: for a pattern of Noise that
    /// brings it.
    pub fn peer_key(&self) -> Option<[u8; KEY]> {
        self.peer.get().copied()
    }

    /// Encrypts the tunnel end to end by the pattern of Noise of `chain`, before it carries
    /// anything: the relays forward records they cannot read or change. `left` - this end writes
    /// the first message of the handshake, sent when the tunnel starts ([`connected`](Self::connected));
    /// the other end must have the other side. Called before the tunnel can be found. The packs of the
    /// virtual connection wait for the keys of the handshake; none of them goes into the keys of the
    /// virtual connection: a [ChaCha20] in a tunnel encrypted by Noise ciphers nothing more - the agent
    /// refuses it. A record that does not open, or a key
    /// [`Chain::on_peer`] refuses, ends the tunnel and goes to [`failure`](External::failure).
    /// Once: a second one is ignored.
    pub fn secure(self: &Arc<Self>, chain: &Chain, left: bool) {
        assert!(0 <= chain.pattern && !chain.has_tls_for(true) && !chain.has_tls_for(false), "A tunnel is encrypted by a pattern of Noise alone: TLS belongs to the socket of its carrier.");
        let me = Arc::downgrade(self); // weak: the end holds its stream, the stream these hooks
        let mut chain = chain.clone();
        let (asks, end) = (chain.on_peer.take(), me.clone());
        chain.on_peer = Some(Arc::new(move |k: &[u8; KEY]| {
            let accepted = asks.as_ref().is_none_or(|f| f(k));
            if accepted {
                if let Some(e) = end.upgrade() {
                    let _ = e.peer.set(*k);
                }
            }
            accepted
        }));
        let mut ns = chain.noise_stream(left, Some(Box::new(move || if let Some(e) = me.upgrade() { e.woken.store(true, Ordering::Release) })));
        ns.burst = true;
        if ns.pending() {
            self.woken.store(true, Ordering::Release); // the first message of the handshake waits: the start of the tunnel wakes it
        }
        let _ = self.noise.set(NoiseHandle::of(ns));
    }

    /// What the carrier hands of a frame of this tunnel to `dst`, the receiver of the virtual
    /// connection: through the Noise stream ([`secure`](Self::secure)), else as they are. How many
    /// it takes; `None` - the stream is broken. While an end of this side waits for its answer
    /// ([`drain`](Self::drain)), the bytes are of the session that ended: taken and dropped.
    #[inline]
    pub fn on_part<D: BytesDst>(&self, dst: &mut D, bytes: &[u8]) -> Option<usize> {
        if !self.heard.load(Ordering::Relaxed) {
            self.heard.store(true, Ordering::Release); // a frame of the far end: it knows this session
        }
        if 0 < self.drain() {
            return Some(bytes.len());
        }
        let Some(noise) = self.noise.get() else { return dst.on_part(bytes) };
        let (took, broke) = {
            // SAFETY: the receive of the carrier, on its loop - the one carrier of this end ( `set_carrier` )
            let ns = unsafe { noise.0.stream() };
            let whole = !ns.failed();
            let took = ns.push_to(dst as &mut dyn BytesDst, bytes).ok();
            (took, whole && ns.failed())
        };
        if self.woken.load(Ordering::Acquire) && self.woken.swap(false, Ordering::AcqRel) {
            self.wake_transmit(); // the answer of the handshake, or the packs that waited for its keys
        }
        if broke {
            self.broke();
        }
        took
    }

    /// The Noise stream broke: a record that does not open, a handshake that fails or is refused.
    #[cold]
    fn broke(&self) {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("The tunnel {}: its handshake failed, or a record did not open. The tunnel ends.", self.tunnel_id()));
        External::failure(self, &e);
        External::close(self);
    }

    /// The burst in flight carried its share and ended between packs, packs left: one more burst is
    /// due after it - behind the other tunnels of the carrier.
    #[inline]
    pub(crate) fn more(&self) {
        self.wanted.store(true, Ordering::SeqCst);
    }

    /// The burst being armed: the wakes from now on ask for the next one.
    #[inline]
    pub(crate) fn armed(&self) -> bool {
        self.wanted.store(false, Ordering::Release);
        self.closing.load(Ordering::Acquire)
    }

    /// The burst in flight waits for a pack's producer
    /// ([`Out`](crate::lib::adhoc::connection::transmitter::tunnel::Out)): the next wake of this
    /// end wakes the carrier. The caller looks at the pack once more after this - a wake that came
    /// before found nothing parked.
    #[inline]
    pub(crate) fn park(&self) {
        self.parked.store(true, Ordering::SeqCst);
    }

    /// The burst goes on: no wake is owed to the carrier.
    #[inline]
    pub(crate) fn unpark(&self) {
        self.parked.store(false, Ordering::Relaxed);
    }

    /// The burst is out: `farewell` — it ended the tunnel.
    pub(crate) fn sent(&self, farewell: bool) {
        self.said.store(true, Ordering::SeqCst); // before the gate opens: a close that takes the gate sees it
        self.in_use.store(false, Ordering::SeqCst); // the gate first, the wish second: a waker writes its wish first
        if farewell {
            self.went(); // the end of this side is on the wire: the session after it on the id may go
            self.closed(Event::THIS_CLOSE_GRACEFUL);
        } else if self.wanted.load(Ordering::SeqCst) || self.closing.load(Ordering::Acquire) {
            self.wake_transmit(); // a pack came, or `close()`, while the burst was in flight
        }
    }

    /// Asked first by the turn of this tunnel on the carrier, on the carrier's thread: `true` - the
    /// turn sends nothing, the session ended without the far end knowing it
    /// ([`close`](External::close)), and its end is told here - on the thread every end of a
    /// tunnel comes on, so the carrier forgets it there. A frame of the far end that came meanwhile
    /// makes the session known: then `false`, and the end goes as usual.
    pub fn silent(&self) -> bool {
        if !self.silent.load(Ordering::Acquire) || !self.silent.swap(false, Ordering::AcqRel) {
            return false;
        }
        if self.heard.load(Ordering::Acquire) {
            // the far end answers this end, unless this end answers the far end's: till then what comes on the id is of this session
            let _ = self.drain.try_update(Ordering::AcqRel, Ordering::Acquire, |d| if d & ANSWERING == 0 { Some(d + 1) } else { None });
            return false;
        }
        self.went();
        self.closed(Event::THIS_CLOSE_GRACEFUL);
        true
    }

    /// The carrier dropped the burst queued or in flight on it: the gate opens.
    pub fn dropped(&self) {
        self.in_use.store(false, Ordering::Release);
    }

    #[inline]
    fn signed(&self, t: &AtomicI32) -> i32 {
        let ms = t.load(Ordering::Relaxed);
        if self.closing.load(Ordering::Acquire) { -ms } else { ms }
    }

    #[inline]
    fn set_timeout(&self, t: &AtomicI32, ms: i32) {
        t.store(if ms < 0 { ms.checked_neg().unwrap_or(i32::MAX) } else { ms }, Ordering::Relaxed);
        if ms < 0 {
            External::close(self);
        }
    }

    /// The tunnel ended — the last burst of this end is out, or the far end's arrived: the virtual
    /// connection is told, once. The far end's end is answered with the end of this side.
    pub fn closed(&self, e: Event) {
        let answer = if matches!(e, Event::REMOTE_CLOSE_GRACEFUL | Event::REMOTE_CLOSE_ABRUPTLY) {
            self.drain.fetch_or(ANSWERING, Ordering::AcqRel); // before the end of this side goes: it answers, it waits for no answer
            !self.closing.swap(true, Ordering::AcqRel)
        } else {
            self.closing.store(true, Ordering::Release); // no burst after this one: is_open is false
            false
        };
        let first = self.open.swap(false, Ordering::AcqRel); // once: the far end's end and the last burst of this one may both come
        // A loss - the carrier gone, or the end of the far end, which a relay sends too when the far end's leg is lost - parks an
        // identified session for resumable_seconds, as a socket does: its close event waits for the end of the wait. Decided before
        // the answer can go out: the burst of the answer settles the parked session
        if first && self.ident.load(Ordering::Acquire) != 0 && matches!(e, Event::REMOTE_CLOSE_GRACEFUL | Event::REMOTE_CLOSE_ABRUPTLY | Event::THIS_CLOSE_ABRUPTLY) {
            let seconds = self.seconds.get().map_or(0, |f| f(e));
            if 0 < seconds {
                self.resumable.store(super::parked::now() + seconds as i64, Ordering::Release);
            }
        }
        if answer {
            self.wake_transmit(); // the answer, behind what is queued: its burst finds the tunnel told already
        }
        let parked = self.resumable.load(Ordering::Acquire) != 0;
        if first {
            if parked {
                if let Some(f) = self.park_hook.get() {
                    f();
                }
                if e == Event::THIS_CLOSE_ABRUPTLY || self.gone.load(Ordering::Acquire) {
                    self.settle_once(); // nothing of this tunnel goes out any more: else it settles when its end is on the wire
                }
            } else if let Some(f) = self.events.get() {
                f(e);
            }
        } else if parked && matches!(e, Event::THIS_CLOSE_GRACEFUL | Event::THIS_CLOSE_ABRUPTLY) {
            self.settle_once(); // the answer of the parked session is on the wire, or its carrier is gone
        }
        if let Some(f) = self.forget.get() {
            f();
        }
    }
}

impl External for EndPoint {
    /// A virtual connection runs on the thread of its carrier.
    #[inline]
    fn on_its_thread(&self) -> bool {
        self.carrier.get().and_then(Weak::upgrade).is_some_and(|c| c.on_its_thread())
    }

    /// On a virtual link the identity of the far end is the tunnel.
    #[inline]
    fn id(&self) -> u32 {
        self.tunnel_id()
    }

    /// Names the session of this tunnel, so that its virtual connection may carry guaranteed packs: the
    /// virtual connection resumes ([`set_resume`](EndPoint::set_resume)) - once per identity. The far end stays the tunnel.
    /// The first takes the session a slot among the parked ( [`parked::slot`](super::parked::slot) ): the token,
    /// `(slot << 32) | id`, is what a reclaim on a new tunnel hands back.
    fn identify(&self, id: u32) -> u64 {
        if self.slot.load(Ordering::Acquire) == 0 {
            let _ = self.slot.compare_exchange(0, super::parked::slot(), Ordering::AcqRel, Ordering::Acquire);
        }
        if self.ident.swap(id, Ordering::AcqRel) != id {
            if let Some(f) = self.resume.get() {
                f();
            }
        }
        External::session(self)
    }

    /// The identity of the session of this tunnel ([`identify`](External::identify)): `id` is the tunnel.
    #[inline]
    fn identity(&self) -> u32 {
        self.ident.load(Ordering::Acquire)
    }

    /// The token of the session of this tunnel: its slot and its identity.
    fn session(&self) -> u64 {
        (self.slot.load(Ordering::Acquire) as u64) << 32 | self.ident.load(Ordering::Acquire) as u64
    }

    /// The seconds the parked session of this tunnel still waits, -1 - none.
    fn parked(&self) -> i32 {
        let r = self.resumable.load(Ordering::Acquire);
        if r == 0 { -1 } else { (r - super::parked::now()).max(0) as i32 }
    }

    #[inline]
    fn is_parked(&self) -> bool {
        self.resumable.load(Ordering::Acquire) != 0
    }

    /// The token of a session of an earlier tunnel, lost with it and parked: that session, taken off the
    /// list of the parked, when it still waits under that identity - for the virtual connection to
    /// move it onto this tunnel.
    fn reclaim_session(&self, session_id: u64) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        let at = (session_id >> 32) as u32;
        if at == 0 || at == self.slot.load(Ordering::Acquire) {
            return None;
        }
        super::parked::take(at, session_id as u32)
    }

    /// To the carrier: its host hears what failed in a session riding it.
    fn failure(&self, e: &(dyn std::error::Error + 'static)) {
        if let Some(c) = self.carrier.get().and_then(Weak::upgrade) {
            c.failure(e);
        }
    }

    #[inline]
    fn is_open(&self) -> bool {
        !self.closing.load(Ordering::Acquire)
    }

    /// Queues one burst on the carrier, unless one is queued or in flight: that one takes what is
    /// queued by then, or asks for the next burst when it is out.
    fn wake_transmit(&self) {
        if self.held() {
            return; // the session before this one on the id sends its end first
        }
        self.wanted.store(true, Ordering::SeqCst); // the wish first, the gate second: the burst opens the gate first
        if self.in_use.load(Ordering::SeqCst) || self.in_use.swap(true, Ordering::AcqRel) {
            // the burst queued or in flight asks for the next one when it is out; parked on a pack
            // that waited for this wake, it goes on now - its carrier is woken
            if self.parked.load(Ordering::SeqCst) && self.parked.swap(false, Ordering::SeqCst) {
                if let Some(c) = self.carrier.get().and_then(Weak::upgrade) {
                    c.wake_transmit();
                }
            }
            return;
        }
        let Some(enqueue) = self.enqueue.get() else {
            // no carrier: no thread to wait for, a session nobody knows ends here
            if !self.silent() {
                self.in_use.store(false, Ordering::Release);
            }
            return;
        };
        if !enqueue() {
            self.in_use.store(false, Ordering::Release); // refused: nothing is queued, nothing would open the gate
        }
    }

    /// Negative while the tunnel closes, `i32::MIN` once it has ended (as the TCP connection).
    #[inline]
    fn receive_timeout(&self) -> i32 {
        if !self.open.load(Ordering::Acquire) {
            return i32::MIN;
        }
        self.signed(&self.receive_timeout)
    }
    /// Negative: the magnitude is kept and the tunnel closes, what is queued first.
    #[inline]
    fn set_receive_timeout(&self, ms: i32) {
        self.set_timeout(&self.receive_timeout, ms)
    }
    /// Negative while the tunnel closes.
    #[inline]
    fn transmit_timeout(&self) -> i32 {
        self.signed(&self.transmit_timeout)
    }
    /// Negative: the magnitude is kept and the tunnel closes, what is queued first.
    #[inline]
    fn set_transmit_timeout(&self, ms: i32) {
        self.set_timeout(&self.transmit_timeout, ms)
    }

    /// A tunnel has nothing to cut short: the same as [`close`](External::close).
    fn abort(&self) {
        self.close()
    }

    /// Ends the tunnel: what is queued goes first, then the far end is told with `ABORT`, and this
    /// end closes when that burst is out. From now on the tunnel id waits for the far end's answer
    /// ([`drain`](EndPoint::drain)). A session nothing of which went out or came in is not known to
    /// the far end: it ends with no `ABORT` and nothing to wait for - on the carrier's thread, as
    /// every end does: its turn there sends nothing ([`silent`](EndPoint::silent)). From any
    /// thread.
    fn close(&self) {
        if self.closing.load(Ordering::Acquire) || self.ending.swap(true, Ordering::AcqRel) {
            return;
        }
        if !self.said.load(Ordering::SeqCst) && !self.heard.load(Ordering::Acquire) && self.in_use.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            if !self.said.load(Ordering::SeqCst) {
                // the gate kept any burst away: the far end does not know the session. It ends on the carrier's thread
                self.silent.store(true, Ordering::Release);
                self.closing.store(true, Ordering::Release);
                self.in_use.store(false, Ordering::SeqCst);
                self.wake_transmit();
                return;
            }
            self.in_use.store(false, Ordering::SeqCst); // a burst went out meanwhile: the gate goes back, the end goes the usual way
        }
        // the far end answers this end, unless this end answers the far end's: till then what comes on the id is of this session
        let _ = self.drain.try_update(Ordering::AcqRel, Ordering::Acquire, |d| if d & ANSWERING == 0 { Some(d + 1) } else { None });
        self.closing.store(true, Ordering::Release);
        self.wake_transmit();
    }

    /// A relay of the virtual connection has room again: the carrier's receive offers the frame's
    /// rest once more.
    fn wake_receive(&self) {
        if let Some(c) = self.carrier.get().and_then(Weak::upgrade) {
            c.wake_receive();
        }
    }

    /// A deadline of an instance of the virtual connection: the carrier times it.
    fn deadline(&self, ms: i64) -> i64 {
        self.carrier.get().and_then(Weak::upgrade).map_or(0, |c| c.deadline(ms))
    }
}

#[cfg(test)]
#[path = "end_point_tests.rs"]
mod tests;
