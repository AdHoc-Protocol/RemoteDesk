// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Forward tunnel frames between connections without decoding their payloads.
//!
//! A frame contains a packet identifier, optional tunnel identifier, and length-prefixed data
//! chunks ending in zero. `ABORT` terminates an interrupted tunnel. [`Relay`] asks the application
//! router for an outbound connection and streams regions to its transmitter.
//!
//! A region reaching the end of a receive buffer can take that allocation by exchanging it for a
//! free buffer of equal length. Other regions are copied when published. The outbound side copies
//! retained regions into its transmit buffer, then returns drained storage for reuse. Input chunk
//! headers are skipped and each region receives its own output chunk framing, so the relay can emit
//! `ABORT` at a chunk boundary after inbound loss.
//!
//! The two sides exchange regions through one slot guarded by try-locks. A full slot pauses input
//! and wakes output; taking a region wakes input through [`External::wake_receive`]. Only one frame
//! is active until its output terminator has been written.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use crate::lib::adhoc::connection::external::External;
use crate::lib::adhoc::connection::receiver::Rx;
use crate::lib::adhoc::connection::transmitter::Tx;
use crate::lib::network::buf::Buf;

/// The width of a chunk header (`chunk_length_store_bytes` of the protocol).
const PREFIX: usize = 2;

type Conn = Weak<dyn std::any::Any + Send + Sync>;

/// The address of a connection: what tells two apart.
#[inline]
fn at_of(c: &Conn) -> usize {
    c.as_ptr() as *const () as usize
}

/// A circuit this host gave a label ([`super::labels`]): a tunnel it relays to another relay, or one that came from a relay.
struct Given {
    /// The connection the side of its opener came on, and the tunnel id that side sends.
    up: Conn,
    up_at: usize,
    up_tid: u32,
    /// The connection it goes on, and its label there.
    down: Conn,
    down_at: usize,
    label: u32,
    /// 1 - the up side sent its end, 2 - the down side did: both - the circuit is over.
    ends: u8,
    /// Tells this circuit from one that has its label later.
    generation: u32,
}

/// The circuits of the labels this host gave, by the index of the label.
struct Labels {
    given: std::collections::HashMap<u32, Given>,
    next: u32,
    generation: u32,
}

fn labels_() -> &'static parking_lot::Mutex<Labels> {
    static L: std::sync::OnceLock<parking_lot::Mutex<Labels>> = std::sync::OnceLock::new();
    L.get_or_init(|| parking_lot::Mutex::new(Labels { given: std::collections::HashMap::new(), next: 0, generation: 0 }))
}

/// The index of `tunnel_id` if it is a label this host gave on a connection - `dialing` it or not.
#[inline]
fn mine(tunnel_id: u32, dialing: bool) -> Option<u32> {
    let k = tunnel_id.wrapping_sub(super::LABEL_FIRST.load(Ordering::Relaxed));
    (k < super::LABEL_PAIRS.load(Ordering::Relaxed) * 2 && (k & 1) == dialing as u32).then_some(k >> 1)
}

/// The tunnel id of a frame that opens a circuit, `[id][mark][label][tunnel id][body]`: all ones of its width.
#[inline]
fn open_mark(tid_bytes: usize) -> u32 {
    if 4 <= tid_bytes { u32::MAX } else { (1u32 << (8 * tid_bytes)) - 1 }
}

/// A circuit the up side of which is the inbound of a relay.
#[derive(Clone, Copy)]
enum Side {
    /// It ends on this host.
    Here,
    /// It goes on: the index of its label and its generation.
    Given(u32, u32),
}

/// What a relay knows of the circuits it is a side of: the receive's, touched in it only and never across the call of the router.
#[derive(Default)]
struct Circ {
    /// The circuits the inbound opened here, by the tunnel id it sends.
    up: std::collections::HashMap<u32, Side>,
    /// The generated connection of the inbound ([`Relay::arm`]): where the frames of the circuits it opened go back to.
    inbound: Option<Conn>,
    inbound_at: usize,
    dialing: bool,
    /// The frame routed now opens a circuit: the label the far relay gave it, the bytes of `[label][tunnel id]` read.
    opening: bool,
    open_got: usize,
    open_label: u32,
    /// The tunnel id of the frame as its source knows it.
    in_tid: u32,
    /// The circuit of the frame routed now: the index of its label, its generation, it came from the up side.
    hit: Option<(u32, u32, bool)>,
    /// [`Relay::through`] asked for a circuit: the tunnel id on the far relay.
    through: Option<u32>,
    /// The frame opens a circuit on its outbound: its prefix names this tunnel id.
    open_tx: Option<u32>,
}

/// A run of the frame's bytes: `len` bytes at `ptr` - in a receive buffer taken over (`own`), or in
/// `copy` - the source's framing in it at `holes` (`[at, len]` pairs relative to `ptr`, ascending,
/// adjacent ones merged), `payload` bytes of the body: `lead` first, then those of the run.
struct Region {
    /// The receive buffer the run lies in, taken over from the inbound connection.
    own: Option<Buf>,
    ptr: *const u8,
    /// The bytes when they lie in no receive buffer: made once, reused.
    copy: Vec<u8>,
    len: usize,
    holes: Vec<u32>,
    hole_bytes: usize,
    payload: usize,
    /// Bytes of the body the receiver read with the pack id: they go out before the run.
    lead: [u8; 4],
    lead_n: usize,
    /// It holds the end of the frame.
    last: bool,
    /// The source ended the frame with `ABORT`: forwarded as such.
    abort: bool,
}

impl Default for Region {
    fn default() -> Self {
        Self { own: None, ptr: std::ptr::null(), copy: Vec::new(), len: 0, holes: Vec::new(), hole_bytes: 0, payload: 0, lead: [0; 4], lead_n: 0, last: false, abort: false }
    }
}

impl Region {
    #[inline]
    fn hole(&mut self, at: usize, len: usize) {
        if len == 0 {
            return;
        }
        self.hole_bytes += len;
        let n = self.holes.len();
        if 0 < n && self.holes[n - 2] as usize + self.holes[n - 1] as usize == at {
            self.holes[n - 1] += len as u32; // right after the previous hole: one hole
            return;
        }
        self.holes.push(at as u32);
        self.holes.push(len as u32);
    }
}

/// What the receive owns: the frame being parsed and the region being staged.
#[derive(Default)]
struct RxSide {
    pack_id: u32,
    id_bytes: usize,
    tid_bytes: usize,
    tid_got: usize,
    tunnel_id: u32,
    /// Bytes of the body the receiver read with the pack id (its id field is wider than the pack id
    /// and the `tunnel_id`).
    lead: u32,
    lead_n: usize,
    /// The router was asked for the current frame.
    resolved: bool,
    /// No live target: the body is consumed and forwarded nowhere.
    drop: bool,
    /// A region is staged, not published yet: its bytes are bound at the publish - a region that
    /// waits for the slot leaves them with the pump, and a retry starts at them.
    staged: bool,
    /// Bytes of the window the staged region takes.
    take: usize,
    /// The chunk parse, across windows.
    scan_state: u8,
    prefix_got: u8,
    chunk_len: u32,
    payload_left: u32,
    st: Region,
    /// The tunnels this relay fed: their far ends hear of it when the inbound is gone
    /// ([`Relay::gone`]). A tunnel is where its frames went: the `tunnel_id` they had there and the
    /// outbound connection ([`Relay::serving`]) - the far ends of two tunnels may know their source
    /// by one id, and the router may rewrite it, so neither the id alone nor a new routing finds it.
    fed: Vec<(u32, Weak<dyn std::any::Any + Send + Sync>)>,
    /// The tunnel of the frame routed now - its `tunnel_id` there and its outbound - and of the last
    /// one fed: the two compares a frame costs.
    cur: Option<(u32, usize)>,
    last: Option<(u32, usize)>,
    /// The free buffer that goes in place of the receive buffer taken over next.
    work: Option<Buf>,
    /// The frame being dropped found no target ([`Relay::refused`] did not say otherwise): when it is
    /// over, its source hears the end of its tunnel.
    bounce: bool,
    /// The tunnels of the inbound this relay told of their end, waiting for the answer.
    bounced: Vec<u32>,
    /// The router said the frame's tunnel ends on this host ([`Relay::here`]): the end of this host
    /// takes the rest of the frame ([`Relay::local`]).
    local: bool,
    /// The source ended the frame with `ABORT`: the circuit of the frame hears it.
    ended: bool,
}

impl RxSide {
    /// The frame in progress is abandoned: the parse starts clean at a pack id. The pack, the
    /// tunnels fed and the work buffer stay.
    fn abandon(&mut self) {
        self.st.own = None; // a region not published: nobody sends it
        self.tid_got = 0;
        self.tunnel_id = 0;
        self.lead = 0;
        self.lead_n = 0;
        self.resolved = false;
        self.drop = false;
        self.staged = false;
        self.take = 0;
        self.scan_state = 0;
        self.prefix_got = 0;
        self.chunk_len = 0;
        self.payload_left = 0;
        self.bounce = false;
        self.local = false;
    }

    fn fed(&mut self, tunnel: (u32, usize), outbound: &Weak<dyn std::any::Any + Send + Sync>) {
        self.last = Some(tunnel);
        if !self.fed.iter().any(|(t, o)| *t == tunnel.0 && o.as_ptr() as *const () as usize == tunnel.1) {
            self.fed.push((tunnel.0, outbound.clone()));
        }
    }

    /// The tunnel of the frame routed now ended: its far end hears no farewell.
    fn unfeed(&mut self) {
        if let Some((tid, out)) = self.cur {
            if let Some(i) = self.fed.iter().position(|(t, o)| *t == tid && o.as_ptr() as *const () as usize == out) {
                self.fed.swap_remove(i);
            }
        }
        self.last = None;
    }
}

/// What the transmit owns: the region in hand and where it is in it.
#[derive(Default)]
struct TxSide {
    state: u8,
    r: Region,
    /// The next byte of the region to look at, relative to its `ptr`.
    rel: usize,
    /// The bytes of its `lead` out.
    lead_at: usize,
    hole_at: usize,
    chunk_left: usize,
    end_mark: u16,
    prefix_done: bool,
    pack_id: u32,
    id_bytes: usize,
    tunnel_id: u32,
    tid_bytes: usize,
    /// The frame opens a circuit: its prefix names this tunnel id on the far relay.
    open: Option<u32>,
    /// The receive buffer of the region done: back to the relay at the next take.
    drained: Option<Buf>,
}

const TX_BETWEEN: u8 = 0;
const TX_PREFIX: u8 = 1;
const TX_REGION: u8 = 2;
const TX_END: u8 = 3;
/// A relay of one frame only: `[id][tunnel_id][ABORT]`, the end of a tunnel whose source is gone.
const TX_FAREWELL: u8 = 4;

/// No frame is lost ([`Relay::lost_seq`]): the numbers of frames never reach it.
const NO_SEQ: u32 = u32::MAX;

/// The one place the sides meet (under `lock`).
#[derive(Default)]
struct Slot {
    full: bool,
    r: Region,
    /// A drained receive buffer on its way back to the receive.
    free: Option<Buf>,
    pack_id: u32,
    id_bytes: usize,
    tunnel_id: u32,
    tid_bytes: usize,
    open: Option<u32>,
}

/// The relay of one virtual connection on one inbound leg: one frame at a time.
pub struct Relay {
    /// Touched only by the inbound receive (one thread at a time: the pump's).
    rx: UnsafeCell<RxSide>,
    /// Touched only by the outbound transmit.
    tx: UnsafeCell<TxSide>,
    lock: AtomicBool,
    slot: UnsafeCell<Slot>,
    /// A frame is queued on the outbound transmitter and its end is not out yet: the next frame
    /// waits (the gate).
    enqueued: AtomicBool,
    /// The outbound is gone holding the frame ([`Relay::outbound_lost`]): the receive drops the
    /// rest of it. Raised before the gate opens, so a receive that passes the gate always sees it.
    out_lost: AtomicBool,
    /// The number of the frame routed last, bumped by every route; never [`NO_SEQ`].
    seq: AtomicU32,
    /// The number of a frame whose inbound is gone in the middle ([`Relay::gone`]): the transmit
    /// ends it. A number, not a flag: when the outbound is gone too, it goes stale by itself
    /// instead of ending the frame routed next.
    lost_seq: AtomicU32,
    /// The `tunnel_id` the frame goes out with: the router may change it
    /// ([`Relay::set_tunnel_id`]).
    out_tid: AtomicU32,
    /// The receive stopped and waits to be woken (the gate is shut, the slot is full): the transmit
    /// wakes it only then.
    rx_waits: AtomicBool,
    /// The transmit holds and waits to be woken (the slot is empty): the receive wakes it only
    /// then.
    tx_held: AtomicBool,
    /// [`Relay::close`] came: nothing goes through any more.
    closed: AtomicBool,
    /// The router said the tunnel ends on this host ([`Relay::here`]).
    here: AtomicBool,
    /// The router found the connection that serves the tunnel, and that one refused the frame
    /// ([`Relay::refused`]).
    refused: AtomicBool,
    /// The connection the router queued the frame on ([`Relay::serving`]): touched only by the
    /// receive, in the router.
    serving: UnsafeCell<Option<Weak<dyn std::any::Any + Send + Sync>>>,
    inbound: parking_lot::Mutex<Option<Arc<dyn External>>>,
    outbound: parking_lot::Mutex<Option<Arc<dyn External>>>,
    /// The circuits: the receive's ([`Relay::through`]).
    circ: UnsafeCell<Circ>,
    /// The tunnel has a Noise of its own ([`Relay::new_clear`]): its frames go on in the clear of
    /// the Noise of the outbound socket.
    clear: bool,
}

// SAFETY: `rx` is touched by the receive only, `tx` by the transmit only, `slot` only under `lock`.
unsafe impl Sync for Relay {}
unsafe impl Send for Relay {}

impl Default for Relay {
    fn default() -> Self {
        Self {
            rx: UnsafeCell::new(RxSide::default()),
            tx: UnsafeCell::new(TxSide::default()),
            lock: AtomicBool::new(false),
            slot: UnsafeCell::new(Slot::default()),
            enqueued: AtomicBool::new(false),
            out_lost: AtomicBool::new(false),
            seq: AtomicU32::new(0),
            lost_seq: AtomicU32::new(NO_SEQ),
            out_tid: AtomicU32::new(0),
            rx_waits: AtomicBool::new(false),
            tx_held: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            here: AtomicBool::new(false),
            refused: AtomicBool::new(false),
            serving: UnsafeCell::new(None),
            inbound: parking_lot::Mutex::new(None),
            outbound: parking_lot::Mutex::new(None),
            circ: UnsafeCell::new(Circ::default()),
            clear: false,
        }
    }
}

impl Relay {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The relay of a tunnel with a Noise of its own ( `[Noise]` of its description ): its records
    /// are sealed end to end, the Noise of the outbound socket does not seal its frames again.
    pub fn new_clear() -> Arc<Self> {
        Arc::new(Self { clear: true, ..Self::default() })
    }

    /// The frames of this relay go in the clear of the Noise of the outbound socket
    /// ([`Relay::new_clear`]).
    #[inline(always)]
    pub fn clear(&self) -> bool {
        self.clear
    }

    /// A frame arrives: `id` is what the receiver read, `rx_id_bytes` wide; the pack id is
    /// `id_bytes` of it, the `tunnel_id` `tid_bytes` wide (0 — none). A receiver whose id field is
    /// wider than the pack id (a connection of actors) read the first bytes of the `tunnel_id` with
    /// it — even of the body: they are taken from `id`, the rest from the wire. `inbound` — the
    /// connection of the receiver: woken when the relay can take bytes again.
    pub fn arm(&self, id: u32, id_bytes: usize, tid_bytes: usize, rx_id_bytes: usize, inbound: Option<Arc<dyn External>>) {
        // SAFETY: the receive side.
        let rx = unsafe { &mut *self.rx.get() };
        if rx.resolved || rx.staged || rx.drop {
            return; // the frame in progress goes on
        }
        // SAFETY: the receive side.
        let c = unsafe { &mut *self.circ.get() };
        c.opening = false;
        c.open_got = 0;
        rx.ended = false; // an end of a frame dropped by a closed relay is of no circuit routed
        let (pack_id, tunnel_id, taken) = crate::lib::adhoc::connection::receiver::tunnel::split_id(id, id_bytes, tid_bytes, rx_id_bytes);
        rx.pack_id = pack_id;
        rx.id_bytes = id_bytes;
        rx.tid_bytes = tid_bytes;
        rx.tunnel_id = tunnel_id;
        rx.tid_got = taken;
        rx.lead_n = rx_id_bytes.saturating_sub(id_bytes + taken);
        rx.lead = if rx.lead_n == 0 { 0 } else { id >> (8 * (id_bytes + taken)) };
        if !Self::same(self.inbound_here(), &inbound) {
            c.dialing = inbound.as_ref().is_some_and(|i| i.dialing());
            *self.inbound.lock() = inbound; // a new connection of the receiver: the frames before came on another
        }
    }

    /// The receive is the one writer of `inbound` and `outbound`: it reads them with no lock, and takes the lock - for the
    /// transmit and [`Relay::close`], which read them on other threads - only to change them.
    #[inline(always)]
    fn same(a: &Option<Arc<dyn External>>, b: &Option<Arc<dyn External>>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
            (None, None) => true,
            _ => false,
        }
    }

    /// `inbound`, read by the receive.
    #[inline(always)]
    fn inbound_here(&self) -> &Option<Arc<dyn External>> {
        // SAFETY: the receive side, the one writer: the other threads only read it, under the lock
        unsafe { &*self.inbound.data_ptr() }
    }

    /// `outbound` set by the receive: the lock only when it changes.
    #[inline(always)]
    fn set_outbound(&self, o: Option<Arc<dyn External>>) {
        // SAFETY: the receive side, the one writer: the other threads only read it, under the lock
        if !Self::same(unsafe { &*self.outbound.data_ptr() }, &o) {
            *self.outbound.lock() = o;
        }
    }

    /// The generated connection the frames of this relay come on, with each frame ([`Relay::arm`]): the frames of a circuit it
    /// opened go back to it ([`Relay::through`]). One compare while it stays the same.
    #[inline]
    pub fn carrier<C: std::any::Any + Send + Sync>(&self, conn: &Arc<C>) {
        // SAFETY: the receive side.
        let c = unsafe { &mut *self.circ.get() };
        let at = Arc::as_ptr(conn) as *const () as usize;
        if c.inbound_at != at {
            c.inbound = Some(Arc::downgrade(conn) as Conn);
            c.inbound_at = at;
        }
    }

    /// The frame is of a circuit this host relays ([`Relay::through`]): `Some( target )` - it goes there, under the tunnel id the
    /// circuit has there ( `None` - nothing serves it any longer, or it ends here: [`Relay::here`] ); `None` - of no circuit here:
    /// route it. The usual route ([`Relay::route`]) and [`Relay::through`] ask this themselves; a route of one's own asks it first.
    pub fn circuit(&self, tunnel_id: u32) -> Option<Option<Arc<dyn std::any::Any + Send + Sync>>> {
        // SAFETY: the receive side - the router runs in it, the receive holds no reference to `circ` across the call.
        let c = unsafe { &mut *self.circ.get() };
        if c.opening {
            return None; // a frame that opens a circuit routes by the tunnel id it opens: the circuit is made after
        }
        if let Some(i) = mine(tunnel_id, c.dialing) {
            let l = labels_().lock();
            return Some(match l.given.get(&i) {
                Some(g) if g.down_at == c.inbound_at => {
                    c.hit = Some((i, g.generation, false));
                    self.set_tunnel_id(g.up_tid);
                    g.up.upgrade()
                }
                _ => None, // a label of a circuit that is over
            });
        }
        match c.up.get(&tunnel_id).copied()? {
            Side::Here => {
                self.here(); // a circuit that ends here: its end knows it by the label it came with
                Some(None)
            }
            Side::Given(i, generation) => {
                let l = labels_().lock();
                match l.given.get(&i) {
                    Some(g) if g.generation == generation => {
                        c.hit = Some((i, generation, true));
                        self.set_tunnel_id(g.label);
                        Some(g.down.upgrade())
                    }
                    _ => {
                        drop(l);
                        c.up.remove(&tunnel_id); // over: what comes on that id now is a tunnel anew
                        None
                    }
                }
            }
        }
    }

    /// The frame goes on `outbound` to the relay there, for `tunnel_id` on that relay - a circuit: this host names the tunnel on
    /// `outbound` by a label of its own ([`super::labels`]), opens it there with the first frame and remembers both sides till both
    /// send their end or a connection of the two is gone. A frame of a circuit known already goes where its circuit goes. What the
    /// region Tunneling serves - `downcast` it as the usual route's.
    pub fn through(&self, outbound: Arc<dyn std::any::Any + Send + Sync>, tunnel_id: u32) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        // SAFETY: the receive side.
        let in_tid = unsafe { &*self.circ.get() }.in_tid;
        if let Some(target) = self.circuit(in_tid) {
            return target;
        }
        unsafe { &mut *self.circ.get() }.through = Some(tunnel_id);
        Some(outbound)
    }

    /// The circuit the frame routed now makes, once its router queued it on `o`: it opens one here, or [`Relay::through`] asked.
    fn bind(&self, o: &Arc<dyn External>) {
        // SAFETY: the receive side, the router is done.
        let c = unsafe { &mut *self.circ.get() };
        if c.hit.is_some() || !c.opening && c.through.is_none() {
            return;
        }
        let (Some(up), Some(down)) = (c.inbound.clone(), unsafe { &*self.serving.get() }.clone()) else { return };
        let pairs = super::LABEL_PAIRS.load(Ordering::Relaxed);
        assert!(pairs != 0, "A circuit needs labels: set them with crate::lib::adhoc::connection::tunnel::labels( first, count )");
        let mut l = labels_().lock();
        if let Some(Side::Given(i, generation)) = c.up.get(&c.in_tid).copied() {
            if l.given.get(&i).is_some_and(|g| g.generation == generation) {
                l.given.remove(&i); // an open over a label still held: the far relay is done with the circuit it named so
            }
        }
        let mut i = l.next;
        let mut n = pairs;
        loop {
            i = if i + 1 >= pairs { 0 } else { i + 1 }; // round the ring: a label just freed is the last to come back
            if !l.given.contains_key(&i) {
                break;
            }
            n -= 1;
            assert!(n != 0, "All {pairs} circuit labels are taken");
        }
        l.next = i;
        l.generation = l.generation.wrapping_add(1);
        let generation = l.generation;
        let label = super::LABEL_FIRST.load(Ordering::Relaxed).wrapping_add(i * 2 + o.dialing() as u32);
        let (up_at, down_at) = (at_of(&up), at_of(&down));
        l.given.insert(i, Given { up, up_at, up_tid: c.in_tid, down, down_at, label, ends: 0, generation });
        drop(l);
        c.up.insert(c.in_tid, Side::Given(i, generation));
        c.hit = Some((i, generation, true));
        self.set_tunnel_id(label);
        c.open_tx = c.through;
    }

    /// The source ended the frame of a circuit: when both sides did, the circuit is over.
    fn circuit_end(&self) {
        // SAFETY: the receive side.
        let c = unsafe { &mut *self.circ.get() };
        let Some((i, generation, up)) = c.hit.take() else { return };
        let mut l = labels_().lock();
        let over = match l.given.get_mut(&i) {
            Some(g) if g.generation == generation => {
                g.ends |= if up { 1 } else { 2 };
                g.ends == 3
            }
            _ => false,
        };
        if over {
            l.given.remove(&i);
        }
    }

    /// Closes the relay, from any thread: a frame half through ends with `ABORT` - one not out yet
    /// vanishes -, and the frames that come to it after are read to their end and go nowhere; the
    /// receive and the transmit are woken to see it.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.lost_seq.store(self.seq.load(Ordering::Acquire), Ordering::Release); // the transmit ends the frame in flight
        self.wake_out();
        self.wake_in();
    }

    /// Called by the router when the connection that serves the tunnel refused the frame ( a full
    /// queue ): the frame is dropped, and that is no reason to end a live tunnel. When the router
    /// finds no connection, the source of the frame hears the end of its tunnel once the frame is
    /// over - once per tunnel, till the source answers with its own end, and never for a frame that
    /// is an end itself.
    pub fn refused(&self) {
        self.refused.store(true, Ordering::Relaxed);
    }

    /// What the router returns for a tunnel that ends on this host - a host that relays the tunnels
    /// of its kind and is an end of some of them: the frame goes on to this host's own end of the
    /// tunnel, as a frame of that end would.
    #[inline]
    pub fn here(&self) -> Option<Arc<dyn External>> {
        self.here.store(true, Ordering::Relaxed);
        None
    }

    /// The connection the frame being routed came on: a route of one's own (region Tunneling)
    /// knows by it who sent the tunnel.
    pub fn inbound(&self) -> Option<Arc<dyn External>> {
        self.inbound.lock().clone()
    }

    /// The usual route of a host that relays tunnels for the hosts logged in to it ([`super::hosts`]): what comes on a connection this
    /// host dialed - from the relay it logged in to - or from a host it did not identify, or is addressed to this host
    /// ([`super::ADDRESS`]) ends here ([`Relay::here`]); any other goes to the host of its address, under the address of its source:
    /// the far end knows the tunnel by the host that opened it, and answers on it. The generated connection of that host, for the
    /// region Tunneling to `downcast` and serve; `None` - it ends here, or nothing serves it.
    pub fn route(&self, tunnel_id: u32) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        if let Some(target) = self.circuit(tunnel_id) {
            return target;
        }
        // SAFETY: the receive side - the router runs in it.
        let opening = unsafe { &*self.circ.get() }.opening; // a tunnel a relay sent here: the end gets a label of this host
        let (from, dialing) = self.inbound_here().as_ref().map_or((0, false), |i| (i.id(), i.dialing()));
        if if opening { tunnel_id == super::ADDRESS.load(Ordering::Relaxed) } else { dialing || from == 0 || tunnel_id == super::ADDRESS.load(Ordering::Relaxed) } {
            self.here();
            return None;
        }
        let Some(all) = super::hosts_() else {
            panic!("The usual route of a tunnel needs crate::lib::adhoc::connection::tunnel::hosts and crate::lib::adhoc::connection::tunnel::ADDRESS: set them, or route the tunnel in region Tunneling")
        };
        let to = all.at(tunnel_id);
        if to.is_some() && !opening {
            self.set_tunnel_id(from);
        }
        to
    }

    /// The `tunnel_id` of the frame the router said ends on this host ([`Relay::here`]), once: the
    /// receive of that end takes the rest of the frame from the window the relay left it at. `None`
    /// - the frame is relayed. A frame no end of this host takes is dropped at the next receive.
    #[inline]
    pub fn local(&self) -> Option<u32> {
        // SAFETY: the receive side.
        let rx = unsafe { &mut *self.rx.get() };
        if !rx.local {
            return None;
        }
        rx.local = false;
        Some(rx.tunnel_id)
    }

    /// Called by the router once it queued the frame on `outbound`: the far end of the tunnel is
    /// there, and hears of it there when the inbound is gone ([`Relay::gone`]). One compare while
    /// the outbound stays the same.
    #[inline]
    pub fn serving<C: std::any::Any + Send + Sync>(&self, outbound: &Arc<C>) {
        // SAFETY: the receive side - the router runs in it.
        let s = unsafe { &mut *self.serving.get() };
        if !s.as_ref().is_some_and(|w| std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(outbound))) {
            *s = Some(Arc::downgrade(outbound) as Weak<dyn std::any::Any + Send + Sync>);
        }
    }

    /// The frame `[id][tunnel_id][ABORT]` alone: the end of the tunnel `tid`.
    fn farewell(pack_id: u32, id_bytes: usize, tid_bytes: usize, tid: u32) -> Arc<Relay> {
        let mut farewell = Relay::default();
        let tx = farewell.tx.get_mut();
        tx.state = TX_FAREWELL;
        tx.pack_id = pack_id;
        tx.id_bytes = id_bytes;
        tx.tid_bytes = tid_bytes;
        *farewell.out_tid.get_mut() = tid;
        Arc::new(farewell)
    }

    /// Not closed.
    #[inline]
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    /// The `tunnel_id` the frame goes out with: the router may change it to re-address the tunnel
    /// on the far side.
    pub fn set_tunnel_id(&self, tunnel_id: u32) {
        self.out_tid.store(tunnel_id, Ordering::Relaxed);
    }

    /// Not under the lock: on one loop the wake runs the other side at once - its transmit, or its
    /// receive that arms this relay again (a lock held here would be taken twice by one thread).
    #[inline]
    fn wake_out(&self) {
        let o = self.outbound.lock().clone();
        if let Some(o) = o {
            o.wake_transmit();
        }
    }

    #[inline]
    fn wake_in(&self) {
        let i = self.inbound.lock().clone();
        if let Some(i) = i {
            i.wake_receive();
        }
    }

    /// The transmit changed what the receive waits for (the slot is empty, the gate is open): the
    /// receive is woken when it waits - a wake of another loop is not spent on a receive that runs.
    #[inline]
    fn wake_in_if_waiting(&self) {
        std::sync::atomic::fence(Ordering::SeqCst); // the change first, the flag second: the receive writes its flag first
        if self.rx_waits.load(Ordering::Relaxed) && self.rx_waits.swap(false, Ordering::AcqRel) {
            self.wake_in();
        }
    }

    /// The receive published a region: the transmit is woken when it holds.
    #[inline]
    fn wake_out_if_held(&self) {
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.tx_held.load(Ordering::Relaxed) && self.tx_held.swap(false, Ordering::AcqRel) {
            self.wake_out();
        }
    }

    #[inline]
    fn try_lock(&self) -> bool {
        self.lock.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    #[inline]
    fn unlock(&self) {
        self.lock.store(false, Ordering::Release);
    }

    /// Scans `b[pos..end)` for the end of the chunked body, across calls. The index just past the
    /// terminator, or `None`. With `region` every framing byte met is noted as a hole relative to
    /// `start`.
    fn scan(rx: &mut RxSide, b: &[u8], mut pos: usize, end: usize, start: usize, region: bool) -> Option<usize> {
        while pos < end {
            if rx.scan_state == 0 {
                let from = pos;
                while (rx.prefix_got as usize) < PREFIX && pos < end {
                    rx.chunk_len |= (b[pos] as u32) << (8 * rx.prefix_got);
                    pos += 1;
                    rx.prefix_got += 1;
                }
                if region {
                    rx.st.hole(from - start, pos - from);
                }
                if (rx.prefix_got as usize) < PREFIX {
                    return None; // the header goes on in the next window
                }
                let len = rx.chunk_len;
                rx.chunk_len = 0;
                rx.prefix_got = 0;
                if len == 0 || len == crate::lib::adhoc::connection::transmitter::chunked::ABORT {
                    rx.st.abort = len != 0;
                    if rx.st.abort {
                        rx.unfeed(); // the source ended its tunnel itself: `gone` has nothing to tell of it
                        rx.ended = true;
                    }
                    rx.tid_got = 0;
                    rx.resolved = false;
                    return Some(pos); // the end of the frame
                }
                rx.payload_left = len;
                rx.scan_state = 1;
            } else {
                let n = (rx.payload_left as usize).min(end - pos);
                pos += n;
                rx.payload_left -= n as u32;
                if rx.payload_left != 0 {
                    return None; // the chunk goes on in the next window
                }
                rx.scan_state = 0;
            }
        }
        None
    }

    /// The body of a frame with no target: consumed to its end, forwarded nowhere.
    fn drop_body(rx: &mut RxSide, src: &mut Rx<'_>) -> bool {
        while 0 < rx.lead_n {
            let b = [rx.lead as u8];
            rx.lead >>= 8;
            rx.lead_n -= 1;
            if Self::scan(rx, &b, 0, 1, 0, false).is_some() {
                rx.drop = false;
                return true;
            }
        }
        match Self::scan(rx, src.buffer, src.byte_, src.byte_max, 0, false) {
            Some(at) => {
                src.byte_ = at;
                rx.drop = false;
                true
            }
            None => {
                src.byte_ = src.byte_max;
                false
            }
        }
    }

    /// The receive side (the inbound receiver's `put_bytes`). `route(relay, tunnel_id)` — the
    /// router: queues the relay on the transmitter of the connection that serves `tunnel_id` and
    /// gives its external connection, or `None` (no such, or it refused): the frame is consumed and
    /// dropped; or [`Relay::here`] - the tunnel ends on this host: `false`, the rest of the frame is
    /// for the end of this host ([`Relay::local`]). `back(frame)` - queues `frame`, the end of a tunnel nothing serves here, on the
    /// transmitter of the inbound connection. `true` — the frame is through; `false` — the window ran
    /// out, or the relay holds the rest until the transmit takes what it has (the receive is woken
    /// then).
    pub fn put_bytes<R: FnOnce(&Arc<Relay>, u32) -> Option<Arc<dyn External>>, B: FnOnce(Arc<Relay>)>(self: &Arc<Self>, src: &mut Rx<'_>, route: R, back: B) -> bool {
        // SAFETY: the receive side.
        let rx = unsafe { &mut *self.rx.get() };
        if rx.local {
            // the router said the tunnel ends here, and no end of this host took the frame: it is dropped
            rx.local = false;
            rx.resolved = true;
            rx.drop = true;
        }
        if !rx.staged && !rx.resolved && !rx.drop && self.enqueued.load(Ordering::Acquire) {
            // the gate: the end of the frame before is not out yet - the transmit wakes the receive
            // when it opens it
            self.rx_waits.store(true, Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::SeqCst); // the flag first, the gate second: the transmit opens first
            if self.enqueued.load(Ordering::Acquire) {
                return false;
            }
            self.rx_waits.store(false, Ordering::Relaxed); // opened meanwhile
        }
        // after the gate: `outbound_lost` raises it before it opens the gate, so a frame that got
        // through is never the lost one taken for a fresh start
        let closed = self.closed.load(Ordering::Acquire);
        if closed && !rx.drop || self.out_lost.load(Ordering::Relaxed) && self.out_lost.swap(false, Ordering::AcqRel) {
            self.discard_slot(); // the dead transmit never takes it
            if rx.staged {
                // a region scanned and not published: consumed as its publish would have
                rx.staged = false;
                src.byte_ += rx.take;
                if rx.st.last {
                    return true; // it held the end of the frame
                }
                rx.drop = true;
            } else if rx.resolved {
                rx.drop = true; // the lost frame is still arriving
            }
        }
        if !rx.staged {
            if !rx.resolved && !rx.drop {
                while rx.tid_got < rx.tid_bytes {
                    if src.byte_max <= src.byte_ {
                        return false; // the `tunnel_id` goes on in the next window
                    }
                    rx.tunnel_id |= (src.buffer[src.byte_] as u32) << (8 * rx.tid_got);
                    src.byte_ += 1;
                    rx.tid_got += 1;
                }
                {
                    // SAFETY: the receive side, not across the router.
                    let c = unsafe { &mut *self.circ.get() };
                    if !c.opening && 0 < rx.tid_bytes && rx.tunnel_id == open_mark(rx.tid_bytes) {
                        c.opening = true; // a relay opens a circuit: [label][tunnel id] follow
                        c.open_got = 0;
                        c.open_label = 0;
                        rx.tunnel_id = 0;
                    }
                    if c.opening {
                        let w = rx.tid_bytes;
                        while c.open_got < w * 2 {
                            let v = if 0 < rx.lead_n {
                                let v = rx.lead as u8; // read with the pack id
                                rx.lead >>= 8;
                                rx.lead_n -= 1;
                                v
                            } else if src.byte_ < src.byte_max {
                                src.byte_ += 1;
                                src.buffer[src.byte_ - 1]
                            } else {
                                return false; // goes on in the next window
                            };
                            if c.open_got < w {
                                c.open_label |= (v as u32) << (8 * c.open_got);
                            } else {
                                rx.tunnel_id |= (v as u32) << (8 * (c.open_got - w));
                            }
                            c.open_got += 1;
                        }
                    }
                    c.in_tid = if c.opening { c.open_label } else { rx.tunnel_id }; // as the source knows the tunnel
                    c.hit = None;
                    c.through = None;
                    c.open_tx = None;
                }
                rx.resolved = true;
                if closed {
                    rx.drop = true; // closed: read to its end, routed nowhere
                    return Self::drop_body(rx, src);
                }
                // a new frame: a `lost_seq` still around is of an older one
                self.seq.store((self.seq.load(Ordering::Relaxed) + 1) & i32::MAX as u32, Ordering::Release);
                self.out_tid.store(rx.tunnel_id, Ordering::Relaxed);
                self.enqueued.store(true, Ordering::Release); // before the queue: the transmit may take the relay at once
                self.refused.store(false, Ordering::Relaxed);
                self.here.store(false, Ordering::Relaxed);
                match route(self, rx.tunnel_id) {
                    None if self.here.load(Ordering::Relaxed) => {
                        // the tunnel ends on this host: its end takes the rest of the frame, the next frame is routed anew
                        assert!(rx.lead_n == 0, "Tunnel arming mismatch: the id field of the inbound holds body bytes past the tunnel_id, which the end of the tunnel on this host cannot replay");
                        rx.cur = None;
                        self.set_outbound(None);
                        self.enqueued.store(false, Ordering::Release);
                        rx.resolved = false;
                        rx.tid_got = 0;
                        {
                            // SAFETY: the receive side, the router is done.
                            let c = unsafe { &mut *self.circ.get() };
                            if c.opening {
                                // a circuit that ends on this host: its end knows it by the far relay's label; one that went on under it is over
                                if let Some(Side::Given(i, generation)) = c.up.insert(c.open_label, Side::Here) {
                                    let mut l = labels_().lock();
                                    if l.given.get(&i).is_some_and(|g| g.generation == generation) {
                                        l.given.remove(&i);
                                    }
                                }
                                self.set_tunnel_id(c.open_label);
                            }
                        }
                        rx.tunnel_id = self.out_tid.load(Ordering::Relaxed); // the end here knows the tunnel by the id the router gave it
                        rx.local = true;
                        return false;
                    }
                    Some(o) => {
                        self.bind(&o);
                        self.set_outbound(Some(o));
                        if !rx.bounced.is_empty() {
                            let tid = rx.tunnel_id;
                            rx.bounced.retain(|&t| t != tid); // served again
                        }
                        // SAFETY: the receive side - the router that set it is done
                        if let Some(out) = unsafe { &*self.serving.get() } {
                            let tunnel = (self.out_tid.load(Ordering::Relaxed), out.as_ptr() as *const () as usize); // the id the router gave it
                            rx.cur = Some(tunnel);
                            if rx.last != Some(tunnel) {
                                rx.fed(tunnel, out); // a frame of the tunnel before: two compares
                            }
                        } else {
                            rx.cur = None; // a router that names no outbound: nothing to tell when the inbound goes
                        }
                    }
                    None => {
                        rx.cur = None;
                        self.set_outbound(None); // no target: a wake has nobody to go to, not the connection of the frame before
                        self.enqueued.store(false, Ordering::Release);
                        rx.drop = true;
                        rx.bounce = !self.refused.load(Ordering::Relaxed); // nothing serves the tunnel: its source hears its end
                    }
                }
            }
            if rx.drop {
                let through = Self::drop_body(rx, src);
                if rx.ended {
                    rx.ended = false;
                    self.circuit_end();
                }
                if through && rx.bounce {
                    rx.bounce = false;
                    let tid = unsafe { &*self.circ.get() }.in_tid; // as the source knows the tunnel: the router changes only the id it goes out with
                    if rx.st.abort {
                        rx.bounced.retain(|&t| t != tid); // the source answered: the next frame that finds no target is told again
                    } else if !rx.bounced.contains(&tid) {
                        rx.bounced.push(tid);
                        back(Self::farewell(rx.pack_id, rx.id_bytes, rx.tid_bytes, tid));
                    }
                }
                return through;
            }
            let (pos, end) = (src.byte_, src.byte_max);
            if pos == end && rx.lead_n == 0 {
                // the body begins in the next window: nothing is staged
                return false;
            }
            rx.st.holes.clear();
            rx.st.hole_bytes = 0;
            rx.st.abort = false;
            rx.st.lead_n = 0;
            let mut eos = None;
            // body bytes the receiver read with the id: framing, or bytes of the body that go out
            // before the window's
            while 0 < rx.lead_n {
                let b = [rx.lead as u8];
                rx.lead >>= 8;
                rx.lead_n -= 1;
                let body = rx.scan_state == 1;
                if Self::scan(rx, &b, 0, 1, 0, false).is_some() {
                    rx.lead_n = 0;
                    eos = Some(pos); // the frame ends in them: nothing of the window is its
                    break;
                }
                if body {
                    rx.st.lead[rx.st.lead_n] = b[0];
                    rx.st.lead_n += 1;
                }
            }
            if eos.is_none() {
                if pos == end && rx.st.lead_n == 0 {
                    return false; // the lead was framing only, the body begins in the next window: nothing is staged
                }
                // the window's bytes: none when the lead holds body bytes only - they go as a
                // region of their own
                eos = Self::scan(rx, src.buffer, pos, end, pos, true);
            }
            // the bytes stay where they are: taken over with the receive buffer, or copied, at the
            // publish
            rx.st.len = eos.unwrap_or(end) - pos;
            rx.st.last = eos.is_some();
            rx.take = rx.st.len;
            rx.st.payload = rx.st.lead_n + rx.st.len - rx.st.hole_bytes;
            rx.staged = true;
            if rx.ended {
                rx.ended = false;
                self.circuit_end();
            }
        }

        let last = rx.st.last; // before the publish: it swaps the staged region into the slot
        if !self.publish(rx, src) {
            // the slot is full or held: the transmit takes it and wakes the receive
            self.rx_waits.store(true, Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::SeqCst); // the flag first, the slot second: the transmit empties it first
            if !self.publish(rx, src) {
                self.wake_out_if_held();
                return false;
            }
            self.rx_waits.store(false, Ordering::Relaxed); // emptied meanwhile
        }
        rx.staged = false;
        src.byte_ += rx.take;
        self.wake_out_if_held();
        last
    }

    /// The receive publishes its staged region into the slot (try-lock site #1): a region that runs
    /// to the end of the bytes received takes the receive buffer over, any other is copied.
    fn publish(&self, rx: &mut RxSide, src: &mut Rx<'_>) -> bool {
        if !self.try_lock() {
            return false;
        }
        // SAFETY: under `lock`.
        let slot = unsafe { &mut *self.slot.get() };
        if slot.full {
            self.unlock();
            return false;
        }
        // the window starts at the region here (the bytes before it were taken at the staging, a
        // retry starts at it)
        if rx.work.is_none() {
            rx.work = slot.free.take(); // the buffer the transmit drained
        }
        let st = &mut rx.st;
        if st.len == 0 {
            st.own = None;
        } else if src.byte_ + st.len == src.byte_max && src.can_take() {
            // the region runs to the end of what the connection received: its receive buffer is the
            // relay's from here, a free one goes in its place - the bytes stay where they are
            let with = rx.work.take().unwrap_or_else(|| vec![0u8; src.storage_len()].into_boxed_slice());
            st.ptr = src.buffer[src.byte_..].as_ptr();
            st.own = Some(src.take_storage(with));
        } else {
            st.own = None;
            st.copy.clear();
            st.copy.extend_from_slice(&src.buffer[src.byte_..src.byte_ + st.len]); // the end of the frame in the middle, or no receive buffer
            st.ptr = st.copy.as_ptr();
        }
        std::mem::swap(&mut slot.r, &mut rx.st); // the slot was empty: its region (holes kept) is the next staging
        slot.pack_id = rx.pack_id;
        slot.id_bytes = rx.id_bytes;
        slot.tunnel_id = self.out_tid.load(Ordering::Relaxed);
        slot.tid_bytes = rx.tid_bytes;
        slot.open = unsafe { &*self.circ.get() }.open_tx; // SAFETY: the receive side
        slot.full = true;
        self.unlock();
        true
    }

    /// The transmit takes the slot's region (try-lock site #2); the one it did before goes back to
    /// the slot.
    fn take(&self, tx: &mut TxSide) -> bool {
        if !self.try_lock() {
            return false;
        }
        // SAFETY: under `lock`.
        let slot = unsafe { &mut *self.slot.get() };
        if !slot.full {
            self.unlock();
            return false;
        }
        std::mem::swap(&mut tx.r, &mut slot.r); // the region done before goes back to the slot (its holes are reused)
        if slot.free.is_none() {
            slot.free = tx.drained.take(); // the buffer drained goes back to the receive
        }
        tx.pack_id = slot.pack_id;
        tx.id_bytes = slot.id_bytes;
        tx.tunnel_id = slot.tunnel_id;
        tx.tid_bytes = slot.tid_bytes;
        tx.open = slot.open;
        slot.full = false;
        self.unlock();
        tx.rel = 0;
        tx.lead_at = 0;
        tx.hole_at = 0;
        tx.chunk_left = 0;
        true
    }

    /// Empties the slot: its region was for an outbound that never takes it. The receive or a
    /// teardown calls it, when no live transmit holds the lock for more than a moment.
    fn discard_slot(&self) {
        while !self.try_lock() {
            std::hint::spin_loop();
        }
        // SAFETY: under `lock`.
        let slot = unsafe { &mut *self.slot.get() };
        if slot.full {
            let own = slot.r.own.take();
            if slot.free.is_none() {
                slot.free = own; // the buffer is free again
            }
        }
        slot.full = false;
        self.unlock();
    }

    /// The outbound connection is closed holding this relay's frame, queued or in flight: its
    /// transmit has stopped, the inbound receive may run. Only the transmit's state is touched
    /// here: the receive learns of it from `out_lost` and drops the rest of the frame.
    pub fn outbound_lost(&self) {
        // SAFETY: the transmit side - its transmit runs no more.
        let tx = unsafe { &mut *self.tx.get() };
        if tx.state == TX_FAREWELL {
            return; // a farewell: nothing waits for it
        }
        tx.r.own = None;
        tx.r.payload = 0;
        tx.chunk_left = 0;
        tx.state = TX_BETWEEN;
        tx.prefix_done = false;
        self.out_lost.store(true, Ordering::Release); // before the gate opens
        self.enqueued.store(false, Ordering::Release);
        self.wake_in(); // it may be parked on the gate or on the full slot
    }

    /// The frame being received will not be completed - its inbound is gone, or its receiver
    /// dropped it: the parse starts clean at the next frame; a frame half through ends with
    /// `ABORT`, one not out yet vanishes. On the receive side, between two receives of the inbound.
    pub fn inbound_lost(&self) {
        // SAFETY: the receive side.
        let rx = unsafe { &mut *self.rx.get() };
        let mid = rx.resolved || rx.staged; // the end of the frame is not published yet
        rx.abandon();
        if self.enqueued.load(Ordering::Acquire) {
            if mid {
                self.lost_seq.store(self.seq.load(Ordering::Relaxed), Ordering::Release);
                rx.unfeed(); // the ABORT that ends the frame ends its tunnel too
                self.wake_out();
            } // else the whole frame is published: the transmit ends it as usual
        } else {
            self.out_lost.store(false, Ordering::Relaxed);
            self.discard_slot();
        }
    }

    /// The inbound connection is gone, its receive runs no more. A frame half through ends with
    /// `ABORT` - nothing of it out yet: it vanishes; the far end of every other tunnel this relay
    /// fed is told by a frame `[id][tunnel_id][ABORT]` of its own, with the `tunnel_id` its frames
    /// had there: `forward( outbound, frame )` queues it on the outbound connection they went to,
    /// after them - not a new routing. The far end closes that tunnel's virtual connection.
    pub fn gone<F: Fn(&Weak<dyn std::any::Any + Send + Sync>, Arc<Relay>)>(&self, forward: F) {
        self.inbound_lost();
        // SAFETY: the receive side - called once no receive runs on the inbound any more.
        let rx = unsafe { &mut *self.rx.get() };
        // the circuits the inbound is a side of are over: the other side hears it, as of a tunnel this relay fed
        let c = unsafe { &mut *self.circ.get() };
        if !c.up.is_empty() || super::LABEL_PAIRS.load(Ordering::Relaxed) != 0 {
            let fed = |tid: u32, at: usize| rx.fed.iter().any(|(t, o)| *t == tid && at_of(o) == at);
            let mut told: Vec<(Conn, u32)> = Vec::new();
            {
                let mut l = labels_().lock();
                for (_, side) in c.up.drain() {
                    if let Side::Given(i, generation) = side {
                        if l.given.get(&i).is_some_and(|g| g.generation == generation) {
                            let g = l.given.remove(&i).unwrap();
                            if !fed(g.label, g.down_at) {
                                told.push((g.down, g.label));
                            }
                        }
                    }
                }
                if c.inbound_at != 0 {
                    l.given.retain(|_, g| {
                        if g.down_at != c.inbound_at {
                            return true;
                        }
                        if !fed(g.up_tid, g.up_at) {
                            told.push((g.up.clone(), g.up_tid));
                        }
                        false
                    });
                }
            }
            for (to, tid) in told {
                forward(&to, Self::farewell(rx.pack_id, rx.id_bytes, rx.tid_bytes, tid)); // not under the lock: the wake may run a route here
            }
            c.hit = None;
        }
        for (tid, out) in rx.fed.drain(..) {
            forward(&out, Self::farewell(rx.pack_id, rx.id_bytes, rx.tid_bytes, tid));
        }
        rx.last = None;
        rx.cur = None;
    }

    /// The transmit side (a pack of the outbound transmitter): `[pack id][tunnel_id]`, every region
    /// as a chunk of its own, then `[0]` — `ABORT` when the source ended it so or vanished. `false`
    /// — the outbound buffer is full (called again), or nothing to send now (the transmitter holds
    /// until the receive wakes it); `true` — the end is written.
    pub fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
        // SAFETY: the transmit side.
        let tx = unsafe { &mut *self.tx.get() };
        loop {
            match tx.state {
                TX_BETWEEN => {
                    // no region in hand, no chunk open: the one place a vanished source is acted on
                    // - a chunk once begun is finished first, its bytes are all here, so the peer
                    // is never left inside one
                    if self.lost_seq.load(Ordering::Acquire) == self.seq.load(Ordering::Acquire) {
                        self.lost_seq.store(NO_SEQ, Ordering::Relaxed);
                        self.discard_slot(); // a region of the lost frame: the frame ends here
                        if !tx.prefix_done {
                            return self.retire(tx); // nothing of it is out: it vanishes
                        }
                        tx.end_mark = crate::lib::adhoc::connection::transmitter::chunked::ABORT as u16;
                        tx.state = TX_END;
                        continue;
                    }
                    if !self.take(tx) {
                        // nothing to take: the receive wakes the transmit when it publishes
                        self.tx_held.store(true, Ordering::Relaxed);
                        std::sync::atomic::fence(Ordering::SeqCst); // the flag first, the slot second: the receive fills first
                        if !self.take(tx) {
                            self.wake_in_if_waiting(); // it may have lost the try-lock to us: it retries
                            dst.hold();
                            return false;
                        }
                        self.tx_held.store(false, Ordering::Relaxed); // filled meanwhile
                    }
                    self.wake_in_if_waiting(); // the slot is empty: the receive can go on
                    tx.state = if tx.prefix_done { TX_REGION } else { TX_PREFIX };
                }
                TX_PREFIX => {
                    let w = tx.tid_bytes;
                    let prefix = tx.id_bytes + if tx.open.is_some() { w * 3 } else { w };
                    if dst.remaining() < prefix {
                        dst.retry_at(0);
                        return false;
                    }
                    let mut at = dst.byte_;
                    dst.buffer[at..at + tx.id_bytes].copy_from_slice(&tx.pack_id.to_le_bytes()[..tx.id_bytes]);
                    at += tx.id_bytes;
                    if let Some(far) = tx.open {
                        // the first frame of a circuit: [mark][label][tunnel id on the far relay]
                        dst.buffer[at..at + w].copy_from_slice(&open_mark(w).to_le_bytes()[..w]);
                        at += w;
                        dst.buffer[at..at + w].copy_from_slice(&tx.tunnel_id.to_le_bytes()[..w]);
                        at += w;
                        dst.buffer[at..at + w].copy_from_slice(&far.to_le_bytes()[..w]);
                    } else {
                        dst.buffer[at..at + w].copy_from_slice(&tx.tunnel_id.to_le_bytes()[..w]);
                    }
                    dst.byte_ += prefix;
                    tx.prefix_done = true;
                    tx.state = TX_REGION;
                }
                TX_REGION => {
                    if tx.chunk_left == 0 {
                        if tx.r.payload == 0 {
                            // the region is out (a region of framing only writes nothing: an empty
                            // chunk would read as the end): its bytes are copied, the buffer they
                            // lay in goes back at the next take - a region that was copied has none,
                            // the spare waiting to go back stays
                            if let Some(own) = tx.r.own.take() {
                                tx.drained = Some(own);
                            }
                            if tx.r.last {
                                tx.end_mark = if tx.r.abort { crate::lib::adhoc::connection::transmitter::chunked::ABORT as u16 } else { 0 };
                                tx.state = TX_END;
                            } else {
                                tx.state = TX_BETWEEN;
                            }
                            continue;
                        }
                        if dst.remaining() < PREFIX + 1 {
                            dst.retry_at(0);
                            return false; // room for the chunk header, and for a byte of it
                        }
                        tx.chunk_left = tx.r.payload.min(crate::lib::adhoc::connection::transmitter::chunked::MAX);
                        let at = dst.byte_;
                        dst.buffer[at..at + PREFIX].copy_from_slice(&(tx.chunk_left as u16).to_le_bytes());
                        dst.byte_ += PREFIX;
                    }
                    // the bytes of the body the receiver read with the id: a few, written
                    while 0 < tx.chunk_left && tx.lead_at < tx.r.lead_n {
                        if dst.remaining() == 0 {
                            dst.retry_at(0);
                            return false;
                        }
                        dst.buffer[dst.byte_] = tx.r.lead[tx.lead_at];
                        dst.byte_ += 1;
                        tx.lead_at += 1;
                        tx.chunk_left -= 1;
                        tx.r.payload -= 1;
                    }
                    while 0 < tx.chunk_left {
                        let h = &tx.r.holes;
                        while tx.hole_at * 2 < h.len() && h[tx.hole_at * 2] as usize == tx.rel {
                            tx.rel += h[tx.hole_at * 2 + 1] as usize;
                            tx.hole_at += 1;
                        }
                        let stop = if tx.hole_at * 2 < h.len() { h[tx.hole_at * 2] as usize } else { tx.r.len };
                        let mut n = (stop - tx.rel).min(tx.chunk_left);
                        // SAFETY: `rel + n <= len`: inside the region's bytes
                        let from = unsafe { tx.r.ptr.add(tx.rel) };
                        // into the one buffer of the outbound: a send is one buffer, its size is
                        // the user's to set
                        let room = dst.remaining();
                        if room == 0 {
                            dst.retry_at(0);
                            return false;
                        }
                        n = n.min(room);
                        // SAFETY: `from + n` inside the region,
                        // `byte_ + n <= byte_max <= buffer.len()`, distinct memory
                        unsafe { std::ptr::copy_nonoverlapping(from, dst.buffer.as_mut_ptr().add(dst.byte_), n) };
                        dst.byte_ += n;
                        tx.rel += n;
                        tx.chunk_left -= n;
                        tx.r.payload -= n;
                    }
                }
                TX_END => {
                    if dst.remaining() < PREFIX {
                        dst.retry_at(0);
                        return false;
                    }
                    let at = dst.byte_;
                    dst.buffer[at..at + PREFIX].copy_from_slice(&tx.end_mark.to_le_bytes());
                    dst.byte_ += PREFIX;
                    return self.retire(tx);
                }
                _ => {
                    // TX_FAREWELL: the whole frame at once
                    let prefix = tx.id_bytes + tx.tid_bytes;
                    if dst.remaining() < prefix + PREFIX {
                        dst.retry_at(0);
                        return false;
                    }
                    let at = dst.byte_;
                    dst.buffer[at..at + tx.id_bytes].copy_from_slice(&tx.pack_id.to_le_bytes()[..tx.id_bytes]);
                    let tid = self.out_tid.load(Ordering::Relaxed);
                    dst.buffer[at + tx.id_bytes..at + prefix].copy_from_slice(&tid.to_le_bytes()[..tx.tid_bytes]);
                    dst.buffer[at + prefix..at + prefix + PREFIX].copy_from_slice(&(crate::lib::adhoc::connection::transmitter::chunked::ABORT as u16).to_le_bytes());
                    dst.byte_ += prefix + PREFIX;
                    return true;
                }
            }
        }
    }

    /// The frame is out: the gate opens and the receive, parked on it, is woken.
    fn retire(&self, tx: &mut TxSide) -> bool {
        tx.state = TX_BETWEEN;
        tx.prefix_done = false;
        self.enqueued.store(false, Ordering::Release);
        self.wake_in_if_waiting(); // a receive parked on the gate goes on
        true
    }
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
