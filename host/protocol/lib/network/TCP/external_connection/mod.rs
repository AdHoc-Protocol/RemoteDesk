// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Thread-safe handles for transport connections and their lifecycle requests.
//!
//! [`ExternalConnection`] exposes slot identity, timeouts, close operations, and wakeups. Atomic
//! requests are consumed on the owning I/O loop. Protocol sources, sinks, and socket state remain
//! on that loop; application hooks receive lifecycle and timeout notifications.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use super::pool::{Pool, ACTIVE, CLEANING, FREE, PARKED};

pub mod event;
pub use event::Event;

/// Stable index of a connection slot within its host registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ConnId(pub u32);

impl std::fmt::Display for ConnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConnId({})", self.0)
    }
}



// Timeout question + deadline

/// Which side's wait expired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    Receive,
    Transmit,
}

/// Application response to one receive or transmit timeout. An extension applies only to the
/// current wait; it does not change the configured default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutAnswer {
    /// Extend this one wait by this many milliseconds. The next wait asks again — an answer never
    /// becomes a new default.
    Extend(i64),
    /// Close the connection (gracefully).
    Close,
    /// Stop checking this side's timeout until the timeout is set again.
    Disarm,
}

/// Callback invoked when a receive or transmit wait expires.
pub type TimeoutHandler = Arc<dyn Fn(TimeoutKind) -> TimeoutAnswer + Send + Sync>;

/// Callback receiving the current time in milliseconds and returning the nearest remaining absolute
/// deadline, or zero when none remain. The connection timer is lowered accordingly.
pub type DeadlineHook = Arc<dyn Fn(i64) -> i64 + Send + Sync>;

/// Servicing of a connection on its own loop's tick: idle checks, statistics, rekeying - whatever
/// looks at or changes the state of the connection, never racing its I/O, which runs on that loop
/// too. Returns the milliseconds until it wants the next call: `i32::MAX` - nothing more to do, 0 -
/// the next tick.
pub type MaintenanceHook = Arc<dyn Fn(&ExternalConnection) -> i32 + Send + Sync>;

// Parked sessions

/// Decides how many seconds a disconnected session remains parked for its peer. Zero or a negative
/// value ends the session immediately.
pub type ResumableSecondsHook = Arc<dyn Fn(Event) -> i64 + Send + Sync>;

/// What the other threads write to a connection ([`ExternalConnection::asks`]): the requests, the
/// link of the inbox.
pub(crate) struct Asks {
    pub(crate) requests: std::sync::atomic::AtomicU32,
    pub(crate) next_task: std::sync::atomic::AtomicPtr<ExternalConnection>,
}

/// Reference-counted thread-safe per-connection state shared between the read pump, write pump,
/// timeout watchdog, and any external holder (the registry, user `on_event` callback, …).
///
/// The fields here are the lock-free skeleton. Any heavy state (the `BytesSrc` / `BytesDst` from
/// the protocol layer, framing decoder, etc.) lives in the loop of the transport.
pub struct ExternalConnection {
    /// The position of this connection's slot in the registry of its host: stamped once, when it is
    /// made.
    pub index: ConnId,
    /// The lifecycle of the slot: [`FREE`] on the free stack, [`ACTIVE`] a session runs on it,
    /// [`CLEANING`] torn down, its transport not done with it yet, [`PARKED`] its session waits for
    /// the peer.
    pub(crate) conn_state: AtomicU8,
    /// The next slot on the list this one is on: the free stack of its host or its parked list -
    /// one of the two at most. Its index + 1; 0 - none.
    pub(crate) next: AtomicU32,
    /// The host whose slot this is: nothing - a connection of no pool (UDP, a test).
    host: std::sync::Weak<Pool>,
    /// Strong host reference retained by a client slot, till the client stops ( `client::Core::give_up` )
    /// or its session is over ( [`reset`](Self::reset) ): the client holds its slot, the two would keep
    /// each other alive for good. Server and engine slots are retained by their registries and do
    /// not need this reference.
    pub(crate) owner: parking_lot::Mutex<Option<Arc<dyn std::any::Any + Send + Sync>>>,
    /// The loop the slot lives on, for good: its number among the loops of the engine, 1 and up; 0
    /// - none. Every session of the slot runs there, its free list is that loop's, its parked
    /// session is reaped there.
    pub(crate) home: u8,
    /// Its place among the slots of its loop ([`crate::lib::network::engine`]), taken by its first
    /// socket and kept: the waits of the slot on the deadline lists of the loop go on from one
    /// session to the next. `u32::MAX` - none yet.
    pub(crate) slot: AtomicU32,
    /// A reconnect of the parked session of this slot is under way: until it connects, a close
    /// parks the session again, for what is left of its wait, with no close event.
    pub(crate) redialing: AtomicBool,
    /// This side made the connection (a client's slot): on a new socket it names itself first and
    /// names its position in the peer's journal only once the peer named its own
    /// ([`crate::lib::adhoc::connection::control`]).
    pub(crate) dialing: AtomicBool,
    /// A WebSocket whose upgrade is not done yet: its session opens with the upgrade, not with the
    /// socket - a port scanner or a refused upgrade opens and closes nothing.
    pub(crate) upgrading: AtomicBool,
    /// The loops of the engine of the host: [`link`](Self::link) names one of them.
    loops: std::sync::OnceLock<Arc<[Arc<crate::lib::network::engine::Loop>]>>,
    pub receive_timeout_ms: AtomicI64, // the magnitude: a graceful close is `closing_gracefully`
    pub transmit_timeout_ms: AtomicI64,
    pub is_open: AtomicBool,
    /// A loss undid the graceful close under way: the teardown parks the session when it may.
    pub(crate) grace_undone: AtomicBool,
    /// Last successful receive, in [`now_ms`] milliseconds. `0` = never.
    pub last_rx_ms: AtomicI64,
    /// Last successful transmit byte. `0` = never.
    pub last_tx_ms: AtomicI64,
    /// Lock-free fast-path flag. Read on the hot event-firing path so we can skip the `event_sink`
    /// mutex entirely when no sink is installed (the common case for non-observed connections).
    event_sink_set: AtomicBool,
    /// Event sink — the lifecycle callback the user wired in via
    /// [`crate::lib::network::TCP::server::Builder::on_event`]. The sink is installed at most once
    /// (registration time, before the pump is spawned), but is held in a Mutex so the type
    /// signature can stay `&self`. Callbacks fire OUTSIDE the lock (see [`Self::fire`]) so a slow
    /// user callback never blocks event delivery.
    pub event_sink: parking_lot::Mutex<Option<Arc<dyn Fn(ConnId, Event) + Send + Sync>>>,

    /// The timeout question hook (`None` = the watchdog's default behavior — see
    /// [`Self::set_timeout_handler`]).
    timeout_handler: parking_lot::Mutex<Option<TimeoutHandler>>,
    /// The deadline: absolute ms when the deadline hook fires; `0` = none. One entry per connection
    /// — the latest [`Self::set_deadline`] wins.
    pub deadline_at: AtomicI64,
    deadline_hook: parking_lot::Mutex<Option<DeadlineHook>>,
    /// The servicing of the connection on the tick of its loop (see [`MaintenanceHook`]);
    /// `maintained` - one is set, so a connection with none costs the tick one load.
    maintenance: parking_lot::Mutex<Option<MaintenanceHook>>,
    maintained: AtomicBool,
    /// When the servicing wants to run next, in the loop's nanoseconds: 0 - the next tick.
    pub(crate) maintain_at: AtomicI64,
    /// How long this side may say nothing, in milliseconds ([`pulse`](Self::pulse)); 0 - no pulse.
    pub(crate) pulse_ms: AtomicI64,
    /// This side hears no `Pulse_` of its peer: it says its own by itself, whichever side dialed.
    pub(crate) pulse_alone: AtomicBool,
    /// The line has been silent: the transmitter says `Pulse_` once its queue is empty
    /// ([`take_pulse`](Self::take_pulse)).
    pulsed: AtomicBool,

    /// A graceful close is in progress: the write half drains and half-closes (`shutdown(Send)`),
    /// the read half lingers for the peer's FIN.
    pub closing_gracefully: AtomicBool,
    /// How long the read half waits for the peer's FIN after the local graceful close before
    /// declaring the close abrupt.
    pub linger_timeout_ms: AtomicI64,
    /// Absolute ms the linger expires. Set by [`Self::close`] / [`Self::close_and_dispose`].
    pub linger_deadline: AtomicI64,
    /// Where the socket of the session runs ([`crate::lib::network::engine`]): the key of its slot
    /// on its loop, the number of that loop of [`loops`](Self::loops) in the low byte; set when the
    /// socket is adopted, kept while the session is parked (its loop reaps it). 0 - no socket (a
    /// test): the wakes have nothing to wake.
    link: AtomicU64,
    /// What other threads asked of the loop of the socket, bits `1 << kind` (`engine::WAKE` …): the
    /// one that raises them from zero puts the connection on the loop's inbox, the others only add
    /// theirs - a thousand wakes are one push. The loop takes them all at once. With the link of
    /// the loop's inbox on a cache line of their own: the threads that ask write them, and the loop
    /// writes the fields of the connection on every buffer - one line shared would go to and fro
    /// between the cores at every wake (B7).
    pub(crate) asks: crossbeam_utils::CachePadded<Asks>,
    /// A connect the connection carries to its loop (`engine::CONNECT`): no closure for it.
    pub(crate) dial: parking_lot::Mutex<Option<crate::lib::network::TCP::Pending>>,
    /// The event the teardown carries, stamped by whoever won the close.
    close_ev: std::sync::atomic::AtomicU32,
    /// A WebSocket control frame from the application (a PING, a CLOSE): it goes ahead of the data.
    pub(crate) ws_urgent: parking_lot::Mutex<Option<crate::lib::network::TCP::web_socket::Control>>,
    pub(crate) ws_urgent_set: AtomicBool,
    /// Our WebSocket CLOSE is staged or out: another is not sent.
    pub(crate) ws_closing: AtomicBool,
    /// Our WebSocket CLOSE, asked for and waiting for the send queue to drain
    /// ([`crate::lib::network::TCP::web_socket::WebSocket::close_graceful`]).
    pub(crate) ws_close: parking_lot::Mutex<Option<crate::lib::network::TCP::web_socket::Control>>,
    /// The Noise stream of an encrypted session: its handshake, its failure, its count of forged
    /// records.
    noise: parking_lot::Mutex<Option<crate::lib::network::noise_adapter::NoiseHandle>>,
    /// The size of the receive and of the send buffer of this connection: set by its server /
    /// client.
    pub buffer_size: std::sync::atomic::AtomicUsize,

    // --- parked sessions ---------------------------------------------------

    /// The session identity: `index << 32 | id`, `0` = unidentified. An unidentified connection is
    /// never parked.
    session_id: AtomicU64,
    /// The protocol state the session survives with — the journal, the keyring, whatever the
    /// application needs to resume.
    session_save: parking_lot::Mutex<Option<Arc<dyn std::any::Any + Send + Sync>>>,
    /// The teardown path's question: seconds a lost session waits.
    resumable_seconds_hook: parking_lot::Mutex<Option<ResumableSecondsHook>>,
    /// Parked-session deadline in Unix seconds, or zero when not parked.
    pub resumable: AtomicI64,
    /// The keys of the session on this connection: the ChaCha20 stage of a pack chain takes them. The
    /// session's: a session that moves to another slot takes them along, the slot gets new ones
    /// ([`renew_keys`](Self::renew_keys)).
    keys: parking_lot::Mutex<Arc<crate::lib::adhoc::connection::keyring::Keyring>>,
    /// A session that moves onto this transport (`reclaim`): its receiver and source take over at a
    /// pack boundary.
    pub reseat: Arc<super::super::reseat::Reseat>,
    /// What gives the session to keep when the connection is lost and parks: the protocol layer
    /// holds it weakly, the transport holds nothing of it while it lives.
    session_keeper: parking_lot::Mutex<Option<SessionKeeper>>,
    /// Who hears that the session can be used on this connection, and that it is over here.
    session_hook: parking_lot::Mutex<Option<SessionHook>>,
    /// The session heard it is open here and not yet that it is over.
    session_open: AtomicBool,
    /// This side's address and the peer's, of the socket the connection runs on: taken when it is
    /// seated.
    addresses: parking_lot::Mutex<(Option<std::net::SocketAddr>, Option<std::net::SocketAddr>)>,
    /// A connection of the UDP transport: its slot, for the requests ([`link`](Self::link) is the
    /// TCP one's).
    pub(crate) udp: std::sync::OnceLock<crate::lib::network::UDP::Link>,
    /// How the protocol layer seated here moves onto another connection: the transport calls it
    /// when the session moves (a UDP `reclaim`).
    seat_hook: parking_lot::Mutex<Option<SeatHook>>,
    /// The connection of a multiplexed port this slot served last: the slot keeps it from one
    /// session to the next - [`reset`](Self::reset) does not take it.
    pub(crate) incumbent: parking_lot::Mutex<Option<Incumbent>>,
    /// How the session seated here goes on after a reconnect (see [`ResumeHook`]).
    resume_hook: parking_lot::Mutex<Option<ResumeHook>>,
    /// How a pack queued while the session is parked gets into its journal (see [`OfflineHook`]).
    offline_hook: parking_lot::Mutex<Option<OfflineHook>>,
    /// The Noise the protocol layer declared as it was seated, not yet applied to its socket ([`secure`](Self::secure)): `None` -
    /// none declared, `Some(None)` - the connection is refused, `Some(Some(chain))` - its socket is encrypted by `chain`.
    secured: parking_lot::Mutex<Option<Option<Arc<crate::lib::chain::Chain>>>>,
    /// What this connection's bytes go through, as chosen, with [`noise`](Self::noise) ([`Chain`](crate::lib::chain::Chain)).
    chain: parking_lot::Mutex<Option<Arc<crate::lib::chain::Chain>>>,
    /// What the session has on its way (see [`ProgressHook`]).
    progress_hook: parking_lot::Mutex<Option<ProgressHook>>,
    /// How the session seated here is closed cleanly for the next process (see [`StopHook`]).
    stop_hook: parking_lot::Mutex<Option<StopHook>>,
    /// The bytes the socket carried, both ways: what `TCP::Redial::stalled` measures the speed by.
    /// The loop of the connection alone writes it.
    pub carried: AtomicU64,
    /// The peer said something on this socket once its session started (a WebSocket's 101 is not
    /// that): what proves a dial reached a peer that serves, not a door that takes the socket and
    /// shuts it - the backoff of the client starts anew only then ([`TCP::Redial`]). The loop of the
    /// connection sets it, the socket of the next session finds it clear.
    ///
    /// [`TCP::Redial`]: crate::lib::network::TCP::client::Redial
    pub(crate) heard: AtomicBool,
    /// A write is parked: the socket does not take what this side writes - the session needs the
    /// link (`TCP::Redial::stalled`). The loop of the connection alone writes it, as the write
    /// parks and as it ends.
    pub(crate) writing: AtomicBool,
    /// Where the failures of this connection go: the handler of its host, set when it is made.
    on_failure: std::sync::OnceLock<crate::lib::network::failure::OnFailure>,
}

/// Seats the protocol layer of a session, with the session, on the connection given (the generated
/// `seat_on`): what it sends goes through that one from now on; the sink and the source the
/// transport of that one drives. `None` - the protocol layer is gone. The connection a multiplexed
/// port seated on a slot last: kept with the slot once its session is over, for the next caller of
/// the slot that names the same one - it takes it back as it is, the factory of the port is not
/// asked.
pub(crate) struct Incumbent {
    /// The byte the caller named it with.
    pub(crate) uid: u8,
    /// The connection itself, held: its hooks hold it weakly.
    pub(crate) keep: Arc<dyn std::any::Any + Send + Sync>,
    /// Seats it on the next socket of the slot (its `seat_on`).
    pub(crate) seat: SeatHook,
}

pub type SeatHook = Arc<dyn Fn(&Arc<ExternalConnection>) -> Option<(Box<dyn crate::lib::adhoc::BytesDst>, Box<dyn crate::lib::adhoc::BytesSrc>)> + Send + Sync>;

/// The session goes on on this connection (the generated `resume`): its peer is asked to go on from
/// the position of its receiver, its journal flows again.
pub type ResumeHook = Arc<dyn Fn() + Send + Sync>;

/// A pack is queued while the session is parked (the generated `journal_queue`): no transmit runs
/// to take it, the thread that queued it journals the queue.
pub type OfflineHook = Arc<dyn Fn() + Send + Sync>;

/// What the session has on its way and what the peer confirmed of it (the generated `attach`): what
/// a client dials again for and judges the speed of its link by (`TCP::Redial`).
pub type ProgressHook = Arc<dyn Fn() -> Progress + Send + Sync>;

/// See [`ProgressHook`].
#[derive(Clone, Copy, Default)]
pub struct Progress {
    /// What of the journal of this side the peer has not confirmed: whether it holds any - a
    /// parked session dials again for it -, whether the peer owes a confirmation.
    pub unconfirmed: crate::lib::adhoc::connection::transmitter::resumable::Unconfirmed,
    /// A guaranteed pack is half received: the peer is sending its rest.
    pub guarding: bool,
    /// The position the peer confirmed of the journal of this side, as each confirmation is heard.
    pub confirmed: i64,
}

/// Hears `true` once the session can be used on a connection, `false` once it is over there.
pub type SessionHook = Arc<dyn Fn(bool) + Send + Sync>;

/// Gives the session a lost connection parks with (the protocol layer's connection), `None` — none
/// to keep.
pub type SessionKeeper = Arc<dyn Fn() -> Option<Arc<dyn std::any::Any + Send + Sync>> + Send + Sync>;

/// The process ends, and the session seated here goes on in the next one: it lives in its files as it goes
/// ( `Control` ), what it holds goes into them now and they are closed cleanly. `false` — no session to close.
pub type StopHook = Arc<dyn Fn() -> bool + Send + Sync>;

impl ExternalConnection {
    /// A connection of no pool, open (a UDP slot, a test).
    pub fn new(id: ConnId) -> Arc<Self> {
        Arc::new(Self::make(id, std::sync::Weak::new(), None, 0, true))
    }

    /// A connection of no pool, closed until its session starts.
    pub(crate) fn closed(id: ConnId) -> Arc<Self> {
        Arc::new(Self::make(id, std::sync::Weak::new(), None, 0, false))
    }

    /// A slot of the pool `host` on its loop `home`, free: [`activate`](Self::activate) opens it
    /// for a session.
    pub(crate) fn pooled(id: ConnId, host: std::sync::Weak<Pool>, loops: Option<Arc<[Arc<crate::lib::network::engine::Loop>]>>, home: u8) -> Arc<Self> {
        Arc::new(Self::make(id, host, loops, home, false))
    }

    fn make(id: ConnId, host: std::sync::Weak<Pool>, loops: Option<Arc<[Arc<crate::lib::network::engine::Loop>]>>, home: u8, open: bool) -> Self {
        Self {
            index: id,
            conn_state: AtomicU8::new(FREE),
            next: AtomicU32::new(0),
            host,
            owner: parking_lot::Mutex::new(None),
            home,
            slot: AtomicU32::new(u32::MAX),
            redialing: AtomicBool::new(false),
            dialing: AtomicBool::new(false),
            upgrading: AtomicBool::new(false),
            loops: loops.map_or_else(std::sync::OnceLock::new, std::sync::OnceLock::from),
            receive_timeout_ms: AtomicI64::new(i32::MAX as i64),
            transmit_timeout_ms: AtomicI64::new(i32::MAX as i64), // No timeout is armed.
            is_open: AtomicBool::new(open),
            grace_undone: AtomicBool::new(false),
            // The timeouts measure silence since connect.
            last_rx_ms: AtomicI64::new(now_ms()),
            last_tx_ms: AtomicI64::new(now_ms()),
            event_sink_set: AtomicBool::new(false),
            event_sink: parking_lot::Mutex::new(None),
            timeout_handler: parking_lot::Mutex::new(None),
            deadline_at: AtomicI64::new(0),
            deadline_hook: parking_lot::Mutex::new(None),
            maintenance: parking_lot::Mutex::new(None),
            maintained: AtomicBool::new(false),
            maintain_at: AtomicI64::new(0),
            pulse_ms: AtomicI64::new(0),
            pulse_alone: AtomicBool::new(false),
            pulsed: AtomicBool::new(false),
            closing_gracefully: AtomicBool::new(false),
            linger_timeout_ms: AtomicI64::new(5_000),
            linger_deadline: AtomicI64::new(0),
            link: AtomicU64::new(0),
            asks: crossbeam_utils::CachePadded::new(Asks { requests: std::sync::atomic::AtomicU32::new(0), next_task: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }),
            dial: parking_lot::Mutex::new(None),
            close_ev: std::sync::atomic::AtomicU32::new(Event::THIS_CLOSE_ABRUPTLY.0),
            ws_urgent: parking_lot::Mutex::new(None),
            ws_urgent_set: AtomicBool::new(false),
            ws_closing: AtomicBool::new(false),
            ws_close: parking_lot::Mutex::new(None),
            noise: parking_lot::Mutex::new(None),
            buffer_size: std::sync::atomic::AtomicUsize::new(crate::lib::network::buf::BUFFER_SIZE),
            session_id: AtomicU64::new(0),
            session_save: parking_lot::Mutex::new(None),
            resumable_seconds_hook: parking_lot::Mutex::new(None),
            resumable: AtomicI64::new(0),
            keys: parking_lot::Mutex::new(Default::default()),
            reseat: Default::default(),
            session_keeper: parking_lot::Mutex::new(None),
            session_hook: parking_lot::Mutex::new(None),
            session_open: AtomicBool::new(false),
            addresses: parking_lot::Mutex::new((None, None)),
            udp: std::sync::OnceLock::new(),
            seat_hook: parking_lot::Mutex::new(None),
            incumbent: parking_lot::Mutex::new(None),
            resume_hook: parking_lot::Mutex::new(None),
            offline_hook: parking_lot::Mutex::new(None),
            secured: parking_lot::Mutex::new(None),
            chain: parking_lot::Mutex::new(None),
            progress_hook: parking_lot::Mutex::new(None),
            stop_hook: parking_lot::Mutex::new(None),
            carried: AtomicU64::new(0),
            heard: AtomicBool::new(false),
            writing: AtomicBool::new(false),
            on_failure: std::sync::OnceLock::new(),
        }
    }

    // The slot of the pool

    /// A slot of a host that is still there: its place on its loop is kept for its next session.
    #[inline]
    pub(crate) fn of_host(&self) -> bool {
        self.host.strong_count() != 0
    }

    /// The host whose slot this is: the pool its sessions park in.
    #[inline]
    pub(crate) fn host(&self) -> Option<Arc<Pool>> {
        self.host.upgrade()
    }

    /// A session runs on this connection.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.conn_state.load(Ordering::Acquire) == ACTIVE
    }

    /// The Noise stream of the session, encrypted: its handshake, its failure, its count of forged
    /// records. `None` - not encrypted.
    pub fn noise(&self) -> Option<crate::lib::network::noise_adapter::NoiseHandle> {
        self.noise.lock().clone()
    }

    pub(crate) fn set_noise(&self, h: crate::lib::network::noise_adapter::NoiseHandle) {
        *self.noise.lock() = Some(h);
    }

    /// The Noise of the socket this connection is seated on, as `[Noise]` of the description of its protocol layer declares it:
    /// for the generated code, which calls it from its `seat_on` every time the protocol layer is seated, before the handshake.
    /// The transport applies it right after the seat - this end starts the handshake when it made the connection. `None` - the
    /// connection is refused: there are no keys for it, and the generated code has told the failure already.
    pub fn secure(&self, chain: Option<crate::lib::chain::Chain>) {
        if let Some(c) = &chain {
            assert!(!c.has_tls_for(true) && !c.has_tls_for(false), "TLS of a connection is chosen by its host or client, not by [Noise]");
            assert!(c.pattern != crate::lib::noise::PATTERN_XX || self.udp.get().is_none(), "Noise XX takes three messages, a UDP handshake two: [Noise] of a UDP connection is NK, IK or KK");
        }
        *self.secured.lock() = Some(chain.map(Arc::new));
    }

    /// What the protocol layer seated last declared of Noise ([`secure`](Self::secure)), taken: `None` - nothing.
    pub(crate) fn take_secured(&self) -> Option<Option<Arc<crate::lib::chain::Chain>>> {
        self.secured.lock().take()
    }

    /// The chain the protocol layer seated last declared, taken; a refusal stays for the transport to hear.
    pub(crate) fn take_secured_chain(&self) -> Option<Arc<crate::lib::chain::Chain>> {
        let mut s = self.secured.lock();
        if matches!(*s, Some(Some(_))) { s.take().flatten() } else { None }
    }

    /// A protocol layer is about to be seated for a new socket: nothing of the Noise of the last one is left.
    pub(crate) fn forget_noise(&self) {
        let gone = (self.secured.lock().take(), self.noise.lock().take());
        drop(gone); // outside the locks
    }

    /// The session is over and the transport is letting go of the slot.
    #[inline]
    pub fn is_cleaning(&self) -> bool {
        self.conn_state.load(Ordering::Acquire) == CLEANING
    }

    /// The session waits here for its peer to come back.
    #[inline]
    pub fn is_parked(&self) -> bool {
        self.conn_state.load(Ordering::Acquire) == PARKED
    }

    /// Takes the slot for a new session: FREE becomes ACTIVE, open, its clocks stamped. `false` -
    /// another caller won it.
    pub(crate) fn activate(&self) -> bool {
        let s = self.conn_state.load(Ordering::Acquire);
        if s != FREE && s != PARKED || self.conn_state.compare_exchange(s, ACTIVE, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        if let Some(h) = self.host.upgrade() {
            h.connections_live.fetch_add(1, Ordering::AcqRel);
            self.linger_timeout_ms.store(h.linger_timeout_ms.load(Ordering::Relaxed), Ordering::Relaxed); // the host's
        }
        let now = now_ms();
        self.last_rx_ms.store(now, Ordering::Relaxed);
        self.last_tx_ms.store(now, Ordering::Relaxed);
        self.heard.store(false, Ordering::Relaxed);
        self.is_open.store(true, Ordering::Release);
        true
    }

    /// Everything the session before left on the slot goes: it is free, the next session finds it
    /// new. Nothing of the transport holds it any more.
    pub(crate) fn reset(&self) {
        self.is_open.store(false, Ordering::Release);
        self.link.store(0, Ordering::Release);
        self.receive_timeout_ms.store(i32::MAX as i64, Ordering::Relaxed);
        self.transmit_timeout_ms.store(i32::MAX as i64, Ordering::Relaxed);
        self.pulse_ms.store(0, Ordering::Relaxed);
        self.pulsed.store(false, Ordering::Relaxed);
        self.closing_gracefully.store(false, Ordering::Relaxed);
        self.grace_undone.store(false, Ordering::Relaxed);
        self.linger_deadline.store(0, Ordering::Relaxed);
        self.linger_timeout_ms.store(5_000, Ordering::Relaxed); // the host's: a session that changed it changed its own
        self.deadline_at.store(0, Ordering::Relaxed);
        self.close_ev.store(Event::THIS_CLOSE_ABRUPTLY.0, Ordering::Relaxed);
        *self.ws_urgent.lock() = None;
        self.ws_urgent_set.store(false, Ordering::Relaxed);
        self.ws_closing.store(false, Ordering::Relaxed);
        *self.ws_close.lock() = None;
        *self.noise.lock() = None;
        self.session_id.store(0, Ordering::Relaxed);
        self.resumable.store(0, Ordering::Relaxed);
        self.session_open.store(false, Ordering::Relaxed);
        self.event_sink_set.store(false, Ordering::Release);
        // the hooks of the session hold it weakly, what it saved holds it strongly: both go - and
        // what they captured with them
        let gone = (
            self.event_sink.lock().take(),
            self.timeout_handler.lock().take(),
            self.deadline_hook.lock().take(),
            self.maintenance.lock().take(),
            self.session_save.lock().take(),
            self.resumable_seconds_hook.lock().take(),
            self.session_keeper.lock().take(),
            self.session_hook.lock().take(),
            self.seat_hook.lock().take(),
            self.resume_hook.lock().take(),
            self.offline_hook.lock().take(),
            self.secured.lock().take(),
            self.stop_hook.lock().take(),
            self.owner.lock().take(), // the client's: a client dropped without a shutdown goes once its session is over
        );
        self.redialing.store(false, Ordering::Relaxed);
        self.upgrading.store(false, Ordering::Relaxed);
        self.maintained.store(false, Ordering::Relaxed);
        self.maintain_at.store(0, Ordering::Relaxed);
        drop(gone); // outside the locks: what they held may run code as it goes
        *self.addresses.lock() = (None, None);
        self.keys().clear();
        self.reseat.reset();
    }

    /// Wins the teardown for `event`: false - someone else did. The socket of the session goes: the
    /// slot is CLEANING until its transport is done with it
    /// ([`finish_dispose`](Self::finish_dispose)).
    #[inline]
    pub(crate) fn claim_close(&self, event: Event) -> bool {
        if !self.is_open.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.close_ev.store(event.0, Ordering::Release);
        if self.conn_state.compare_exchange(ACTIVE, CLEANING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            if let Some(h) = self.host.upgrade() {
                h.connections_live.fetch_sub(1, Ordering::AcqRel);
            }
        }
        true
    }

    /// The transport is done with the slot: a lost session waits on it, parked, for its peer; any
    /// other is over, and the slot goes back to the free stack.
    pub(crate) fn finish_dispose(&self) {
        if self.conn_state.load(Ordering::Acquire) != CLEANING {
            return;
        }
        let Some(h) = self.host.upgrade() else { return };
        if self.resumable.load(Ordering::Acquire) != 0 {
            self.link.store(0, Ordering::Release); // no socket while it waits: a close of it now is a close of no transport
            h.park_connection(self);
            super::client::Core::parked(self); // a client's session dials again, by the backoff, on this loop
        } else {
            h.return_to_free_pool(self);
        }
    }

    /// A slot taken for a session that never got a transport (a factory that panicked, a socket
    /// that could not be adopted): it goes back with no event.
    pub(crate) fn abandon(&self) {
        self.claim_close(Event::THIS_CLOSE_ABRUPTLY);
        self.finish_dispose();
    }

    /// The parked session of this slot, claimed for a reconnect after its wait ran out, ends here:
    /// its close is heard, its keys go, its identity with it - the slot starts a session anew with
    /// the protocol layer it has.
    pub(crate) fn end_stale(&self) {
        self.reap();
        self.session_id.store(0, Ordering::Release);
        self.session_save.lock().take();
    }

    /// Ends the parked session for good: the peer did not return in time - the close the loss
    /// withheld is heard now, the keys of the session go. Its host takes the slot back.
    pub(crate) fn reap(&self) {
        self.resumable.store(0, Ordering::Release);
        // code of the application on the tick of the loop: a panic of it goes to the handler, the
        // reap of the others goes on
        if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.fire(Event::REMOTE_CLOSE_ABRUPTLY))) {
            self.failure(&crate::lib::network::failure::panicked(&*why));
        }
        self.keys().clear();
    }

    /// The keys of the session on this connection (see the field): what a stage takes.
    #[inline]
    pub fn keys(&self) -> Arc<crate::lib::adhoc::connection::keyring::Keyring> {
        self.keys.lock().clone()
    }

    /// The keys of the session that moved onto this connection: they are the ones its stages hold.
    pub(crate) fn set_keys(&self, keys: Arc<crate::lib::adhoc::connection::keyring::Keyring>) {
        *self.keys.lock() = keys;
    }

    /// The session on this slot moved to another one with its keys: the slot gets keys of its own
    /// for its next session.
    pub(crate) fn renew_keys(&self) {
        *self.keys.lock() = Default::default();
    }

    /// This thread runs the loop the socket of the session is on: the code of the connection runs here, one thing at a
    /// time - what only it touches needs no lock. No socket ( parked, closed, not yet seated ) - false.
    #[inline]
    pub fn on_its_thread(&self) -> bool {
        match self.link() {
            Some((lp, _)) => lp.in_loop(),
            None => self.udp.get().is_some_and(|u| u.on_loop_thread()),
        }
    }

    /// Where the socket of the session runs: its loop, the key of its slot there.
    #[inline(always)]
    pub(crate) fn link(&self) -> Option<(&Arc<crate::lib::network::engine::Loop>, u64)> {
        let v = self.link.load(Ordering::Acquire);
        if v == 0 {
            return None;
        }
        Some((self.loops.get()?.get((v & 0xFF) as usize - 1)?, v & !0xFF))
    }

    /// The socket of the session is adopted on the loop `number` of `loops`, its slot there is
    /// `key`.
    pub(crate) fn link_to(&self, loops: &Arc<[Arc<crate::lib::network::engine::Loop>]>, number: u8, key: u64) {
        if self.loops.get().is_none() {
            let _ = self.loops.set(loops.clone());
        }
        debug_assert!(Arc::ptr_eq(self.loops.get().unwrap(), loops), "a slot serves the loops of one engine");
        self.link.store(key | number as u64, Ordering::Release);
    }

    /// The handler the failures of this connection go to: its host's, set once, when it is made.
    pub fn set_on_failure(&self, f: crate::lib::network::failure::OnFailure) {
        let _ = self.on_failure.set(f);
    }

    /// A failure of this connection, to the handler of its host.
    pub(crate) fn failure(&self, e: &(dyn std::error::Error + 'static)) {
        match self.on_failure.get() {
            // a handler that panics does not take the loop with it
            Some(f) => {
                if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self, e))) {
                    (crate::lib::network::failure::print_console())(self, &crate::lib::network::failure::panicked(&*why));
                }
            }
            None => (crate::lib::network::failure::print_console())(self, e),
        }
    }

    /// What the bytes of the next socket go through.
    pub(crate) fn set_chain(&self, chain: Option<Arc<crate::lib::chain::Chain>>) {
        *self.chain.lock() = chain;
    }

    pub(crate) fn chain(&self) -> Option<Arc<crate::lib::chain::Chain>> {
        self.chain.lock().clone()
    }

    /// The reconnect connected: the session is live on this slot again, a close from now on is a
    /// close of the session here.
    pub(crate) fn redialed(&self) {
        self.redialing.store(false, Ordering::Release);
        self.resumable.store(0, Ordering::Release);
    }

    /// A reconnect that did not connect: the session stays parked, for what is left of its wait.
    /// `true` - it was one.
    #[inline]
    fn failed_redial(&self) -> bool {
        self.redialing.swap(false, Ordering::AcqRel)
    }

    /// How the session seated here goes on after a reconnect (see [`ResumeHook`]).
    pub fn set_resume_hook(&self, hook: ResumeHook) {
        *self.resume_hook.lock() = Some(hook);
    }

    /// How a pack queued while the session is parked gets into its journal (see [`OfflineHook`]).
    pub fn set_offline_hook(&self, hook: OfflineHook) {
        *self.offline_hook.lock() = Some(hook);
    }

    /// What the session seated here has on its way (see [`ProgressHook`]).
    pub fn set_progress_hook(&self, hook: ProgressHook) {
        *self.progress_hook.lock() = Some(hook);
    }

    /// What the session seated here has on its way (see [`ProgressHook`]): nothing, when it says
    /// nothing of it.
    pub fn progress(&self) -> Progress {
        let hook = self.progress_hook.lock().clone();
        hook.map_or(Progress::default(), |h| h())
    }

    /// The loop of the connection: the socket carried `n` more bytes. Its thread alone writes: no
    /// locked step.
    #[inline]
    pub(crate) fn carry(&self, n: usize) {
        self.carried.store(self.carried.load(Ordering::Relaxed).wrapping_add(n as u64), Ordering::Relaxed);
    }

    /// The session seated here goes on.
    pub(crate) fn resume(&self) {
        let hook = self.resume_hook.lock().clone();
        if let Some(h) = hook {
            h();
        }
    }

    /// The protocol layer of the session seated here, seated with it on `to` (see [`SeatHook`]).
    pub(crate) fn seat_on(&self, to: &Arc<ExternalConnection>) -> Option<(Box<dyn crate::lib::adhoc::BytesDst>, Box<dyn crate::lib::adhoc::BytesSrc>)> {
        let hook = self.seat_hook.lock().clone();
        hook.and_then(|h| h(to))
    }

    /// How the protocol layer seated here moves onto another connection, if it said (see
    /// [`SeatHook`]).
    pub(crate) fn seat_hook(&self) -> Option<SeatHook> {
        self.seat_hook.lock().clone()
    }

    /// How the protocol layer seated here moves onto another connection (see [`SeatHook`]).
    pub fn set_seat_hook(&self, hook: SeatHook) {
        *self.seat_hook.lock() = Some(hook);
    }

    /// The session of this connection moves onto `to`: its protocol layer is seated there and what
    /// it hooked here - its events, `OnOpen` / `OnClose`, its seconds, its keeper, its seat - goes
    /// there, in place of what the protocol layer seated there hooked.
    pub(crate) fn hand_over(&self, to: &Arc<ExternalConnection>) {
        let seat = self.seat_hook.lock().take();
        if let Some(h) = &seat {
            // the protocol layer seats itself there, its hooks too; the sink and the source it
            // gives are not wanted - the transport moves the ones that hold the state of the
            // session. Nor the Noise it declares: `to` runs the stream of its own socket
            let noise = to.noise();
            let seated = h(to).is_some();
            let gone = (to.take_secured(), std::mem::replace(&mut *to.noise.lock(), noise));
            drop(gone);
            if seated {
                return;
            }
        }
        if let Some(e) = self.event_sink.lock().take() {
            self.event_sink_set.store(false, Ordering::Release);
            to.set_event_sink(e);
        }
        *to.session_hook.lock() = self.session_hook.lock().take();
        *to.resumable_seconds_hook.lock() = self.resumable_seconds_hook.lock().take();
        *to.session_keeper.lock() = self.session_keeper.lock().take();
        *to.stop_hook.lock() = self.stop_hook.lock().take();
        *to.seat_hook.lock() = seat;
    }

    /// How the session seated here is closed cleanly for the next process (see [`StopHook`]).
    pub fn set_stop_hook(&self, hook: StopHook) {
        *self.stop_hook.lock() = Some(hook);
    }

    /// The session seated here is closed cleanly for the next process (see [`StopHook`]): `false` — none to close.
    pub(crate) fn stop_session(&self) -> bool {
        let hook = self.stop_hook.lock().clone(); // code of the session: not under the lock
        hook.is_some_and(|h| h())
    }

    /// What gives the session to keep when this connection is lost and parks (see
    /// [`SessionKeeper`]).
    pub fn set_session_keeper(&self, keeper: SessionKeeper) {
        *self.session_keeper.lock() = Some(keeper);
    }

    /// What holds the session of this connection, set by its protocol layer.
    pub(crate) fn session_keeper(&self) -> Option<SessionKeeper> {
        self.session_keeper.lock().clone()
    }

    /// The peer hands back the token of a former connection of this host: when its session is still
    /// parked, it is claimed — exactly one winner against the reaper — and given back, for the
    /// protocol layer to move onto this transport. Either way this connection takes the identity of
    /// the token.
    pub fn reclaim_session(&self, session_id: u64) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        if let Some(u) = self.udp.get() {
            u.reclaim(session_id); // The transport moves the retained session directly; protocol reseating is unnecessary.
            return None;
        }
        let saved = self.claim(session_id);
        if saved.is_some() {
            self.unidentify(); // the identity is applied again below: the login may have identified this connection already
        }
        ExternalConnection::identify(self, session_id as u32);
        saved
    }

    /// The session parked under `session_id` on a slot of this connection's host, claimed for this
    /// connection: its keys come here - a pack its journal replays was sealed with one of them -,
    /// what it saved is returned for the protocol layer to move here, and its slot goes back to the
    /// host once the call that claimed it is over - the receiver of the session that moved here may
    /// still be on the stack.
    fn claim(&self, session_id: u64) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        if (session_id >> 32) as u32 == self.index.0 {
            return None; // this very slot
        }
        let host = self.host.upgrade()?;
        let parked = host.parked(session_id)?;
        self.keys().adopt(&parked.keys());
        let saved = parked.session_save().or_else(|| {
            let keeper = parked.session_keeper.lock().clone();
            keeper.and_then(|k| k())
        });
        let depart = move || {
            parked.keys().clear(); // its session ended there: the key of that connection goes
            host.return_to_free_pool(&parked);
        };
        match self.link() {
            Some((lp, _)) if lp.in_loop() => lp.submit_fn(Box::new(move |_| depart())),
            _ => depart(),
        }
        saved
    }

    // The doors in from other threads: on the loop of the connection the work runs at once,
    // elsewhere it is a request - the connection itself on the loop's inbox, once for any number of
    // requests.

    #[inline]
    fn on_loop(&self, kind: u64, f: impl FnOnce(*mut crate::lib::network::engine::Slot)) {
        let Some((lp, key)) = self.link() else { return self.on_udp(kind) };
        if lp.in_loop() {
            unsafe { crate::lib::network::engine::on_slot_kind(lp, key, kind, f) }; // as the request from another thread: the same at any thread
        } else {
            self.request(lp, kind);
        }
    }

    /// `kind` of the loop `lp` of the socket, on its next pass: the connection goes on its inbox
    /// when it is not there yet.
    #[inline]
    pub(crate) fn request(&self, lp: &crate::lib::network::engine::Loop, kind: u64) {
        if self.asks.requests.fetch_or(1 << kind, Ordering::AcqRel) == 0 {
            // SAFETY: a connection with a socket is in an `Arc` (its pool's, its slot's): the inbox
            // holds a count of it until the loop has served it
            unsafe { Arc::increment_strong_count(self as *const Self) };
            lp.push(self);
        }
    }

    /// The same door, for a connection of the UDP transport: a request its loop serves on its next
    /// pass.
    #[cold]
    fn on_udp(&self, kind: u64) {
        use crate::lib::network::{engine, UDP};
        if let Some(u) = self.udp.get() {
            u.request(match kind {
                engine::WAKE => UDP::TX_,
                engine::DRAIN => UDP::RX_,
                engine::DEADLINE => UDP::DEADLINE_,
                engine::GRACE => UDP::CLOSE_,
                _ => return,
            });
        }
    }

    /// There is something to send.
    #[inline]
    pub fn wake_transmit(&self) {
        if self.is_open.load(Ordering::Acquire) {
            self.on_loop(crate::lib::network::engine::WAKE, |p| unsafe { crate::lib::network::engine::wake_on_loop(p) });
        } else if self.resumable.load(Ordering::Acquire) != 0 {
            self.offline();
        }
    }

    /// Parked: no transmit runs to wake - the thread that queued the pack journals the queue
    /// ([`OfflineHook`]).
    #[cold]
    fn offline(&self) {
        let hook = self.offline_hook.lock().clone(); // code of the session: not under the lock
        if let Some(h) = hook {
            h();
        }
    }

    /// As [`wake_transmit`](Self::wake_transmit), never run in place: the transmit runs on the next
    /// pass of the loop, even when asked on the loop's thread. For a caller that holds what the
    /// transmit takes - a record layer that wakes the transmit from inside its own lock (Noise: the
    /// handshake moved on) would lock itself out.
    pub fn wake_transmit_deferred(&self) {
        if !self.is_open.load(Ordering::Acquire) {
            return;
        }
        let Some((lp, _)) = self.link() else { return self.on_udp(crate::lib::network::engine::WAKE) };
        self.request(lp, crate::lib::network::engine::WAKE); // through the inbox from the loop's own thread too: the next pass
    }

    /// The consumer that refused the tail has room again.
    #[inline]
    pub fn wake_receive(&self) {
        if self.is_open.load(Ordering::Acquire) {
            self.on_loop(crate::lib::network::engine::DRAIN, |p| unsafe { crate::lib::network::engine::drain_on_loop(p) });
        }
    }

    /// The teardown: exactly one caller wins, whichever thread; it runs on the loop. A connection
    /// of no transport only hears the event.
    pub fn disconnect(&self, event: Event) {
        if !self.claim_close(event) {
            return;
        }
        match self.link() {
            Some((lp, key)) if lp.in_loop() => unsafe { crate::lib::network::engine::close_slot(lp, key) },
            Some((lp, _)) => self.request(lp, crate::lib::network::engine::CLOSE),
            None if self.udp.get().is_some() => self.udp.get().unwrap().request(crate::lib::network::UDP::ABORT_),
            None => {
                if self.failed_redial() {
                    self.finish_dispose(); // a reconnect that never got a socket: the session parks again
                    return;
                }
                // code of the application: a panic of it goes to the handler, the slot goes back all the same
                let quietly = |f: &dyn Fn()| {
                    if let Err(why) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                        self.failure(&crate::lib::network::failure::panicked(&*why));
                    }
                };
                quietly(&|| self.fire(event));
                quietly(&|| self.session_closed());
                self.resumable.store(0, Ordering::Release);
                self.finish_dispose(); // no transport took it: nothing else gives the slot back
            }
        }
    }

    /// This side's address of the socket: `None` before it is seated, or when the system did not
    /// tell.
    pub fn local(&self) -> Option<std::net::SocketAddr> {
        self.addresses.lock().0
    }

    /// The peer's address: `None` before the socket is seated, or when the system did not tell.
    pub fn remote(&self) -> Option<std::net::SocketAddr> {
        self.addresses.lock().1
    }

    /// The socket is seated: its addresses are taken.
    pub(crate) fn set_addresses(&self, local: Option<std::net::SocketAddr>, remote: Option<std::net::SocketAddr>) {
        *self.addresses.lock() = (local, remote);
    }

    /// Who hears `OnOpen` / `OnClose` of the session on this connection: the protocol layer seated
    /// on it.
    pub fn set_session_hook(&self, hook: SessionHook) {
        *self.session_hook.lock() = Some(hook);
    }

    /// The session can be used here: `OnOpen`, once. With nobody seated yet - the mux before its
    /// pick - nothing is opened. The session heard `OnOpen` here and not yet `OnClose`.
    pub(crate) fn is_session_open(&self) -> bool {
        self.session_open.load(Ordering::Acquire)
    }

    pub(crate) fn session_opened(&self) {
        let hook = self.session_hook.lock().clone();
        if let Some(h) = hook {
            if !self.session_open.swap(true, Ordering::AcqRel) {
                h(true);
            }
        }
    }

    /// The connection is over: `OnClose` for a session that heard `OnOpen`, once, after the closing
    /// event - whatever ended it, a session that parks too: the peer comes back on another
    /// connection.
    pub(crate) fn session_closed(&self) {
        if self.session_open.swap(false, Ordering::AcqRel) {
            let hook = self.session_hook.lock().clone();
            if let Some(h) = hook {
                h(false);
            }
        }
    }

    #[inline]
    pub(crate) fn close_event(&self) -> Event {
        Event(self.close_ev.load(Ordering::Acquire))
    }

    /// A WebSocket control frame from the application: ahead of the data. False - not a WebSocket,
    /// or closed.
    pub(crate) fn ws_control(&self, c: crate::lib::network::TCP::web_socket::Control) -> bool {
        if !self.is_open.load(Ordering::Acquire) {
            return false;
        }
        if c.is_close() && self.ws_closing.swap(true, Ordering::AcqRel) {
            return true; // closing already
        }
        {
            let mut u = self.ws_urgent.lock();
            if !c.is_close() && u.as_ref().is_some_and(|staged| staged.is_close()) {
                return true; // a CLOSE staged and not out yet goes: the PING yields to it
            }
            *u = Some(c);
        }
        self.ws_urgent_set.store(true, Ordering::Release);
        self.wake_transmit();
        true
    }

    // Session identity and parking

    /// The session token: `index << 32 | id`: the slot and the identity - 0 in the low half while
    /// the session is not identified.
    #[inline]
    pub fn session(&self) -> u64 {
        ((self.index.0 as u64) << 32) | (self.session_id.load(Ordering::Acquire) & 0xFFFF_FFFF)
    }

    /// Sets this connection's identity to `id` and returns the packed token.
    pub fn identify(&self, id: u32) -> u64 {
        let s = ((self.index.0 as u64) << 32) | (id as u64);
        if self.session_id.swap(s, Ordering::AcqRel) != s {
            if let Some(u) = self.udp.get() {
                u.request(crate::lib::network::UDP::CONTINUE_ | crate::lib::network::UDP::TX_); // a UDP session resumes: the peer is asked to go on from this side's position
            }
        }
        s
    }

    /// The protocol state to keep across a socket loss (the journal, the keyring — opaque to the
    /// network layer).
    pub fn save_session(&self, state: Arc<dyn std::any::Any + Send + Sync>) {
        *self.session_save.lock() = Some(state);
    }

    /// The saved protocol state, if any.
    pub fn session_save(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.session_save.lock().clone()
    }

    /// The teardown path's question: how many seconds a lost session waits for its peer; the hook
    /// also sees the loss event.
    pub fn set_resumable_seconds_hook(&self, hook: ResumableSecondsHook) {
        *self.resumable_seconds_hook.lock() = Some(hook);
    }

    /// The protocol layer's answer: how many seconds the session lost with `event` waits for its
    /// peer; 0 - none.
    pub(crate) fn resumable_seconds(&self, event: Event) -> i64 {
        let hook = self.resumable_seconds_hook.lock().clone();
        hook.map_or(0, |h| h(event))
    }

    /// The identity is gone with the session (a UDP slot that takes the next one), or is to be
    /// applied anew (a session that moved here).
    pub(crate) fn unidentify(&self) {
        self.session_id.store(0, Ordering::Release);
    }

    /// The seconds the parked session still waits for its peer, 0 once its deadline has passed but
    /// it has not been reaped yet, `-1` when there is no parked session.
    #[inline]
    pub fn parked(&self) -> i64 {
        let d = self.resumable.load(Ordering::Acquire);
        if d == 0 {
            -1
        } else {
            (d - now_sec()).max(0)
        }
    }

    /// A loss that is not our own doing: when the connection is identified, is a slot of a host,
    /// and the application answers `resumable_seconds > 0`, the session waits on this slot for its
    /// peer instead of ending, and `event` is WITHHELD - heard at the reap. Returns `true` when it
    /// was withheld: the caller must not fire it. The teardown of the socket goes on either way,
    /// and the slot parks once its transport is done with it
    /// ([`finish_dispose`](Self::finish_dispose)).
    pub(crate) fn loss(&self, event: Event, closing_gracefully: bool) -> bool {
        if self.failed_redial() {
            return true; // not the end of the session: a reconnect of it that failed
        }
        if closing_gracefully || !(event.is_remote() || event.is_timeout()) {
            return false;
        }
        if self.session() as u32 == 0 || self.host.strong_count() == 0 {
            return false;
        }
        let hook = self.resumable_seconds_hook.lock().clone(); // code of the application: not under the lock
        let seconds = hook.map_or(0, |h| h(event));
        if seconds <= 0 {
            return false;
        }
        if self.session_save().is_none() {
            let keeper = self.session_keeper.lock().clone();
            if let Some(s) = keeper.and_then(|k| k()) {
                self.save_session(s); // the session the slot waits with
            }
        }
        self.resumable.store(now_sec() + seconds, Ordering::Release);
        true
    }

    /// Install (or replace) the event-sink callback. Records the existence of a sink in
    /// [`Self::event_sink_set`] so [`Self::fire`] can skip the mutex acquisition on the hot path.
    pub fn set_event_sink(&self, sink: Arc<dyn Fn(ConnId, Event) + Send + Sync>) {
        *self.event_sink.lock() = Some(sink);
        self.event_sink_set.store(true, Ordering::Release);
    }

    /// The timeout question: when a wait expires the watchdog asks this hook and acts on the answer
    /// — extend this one wait, close, or disarm. Unset: the watchdog's default behavior (fire the
    /// timeout event; close only under the negative-graceful convention).
    pub fn set_timeout_handler(&self, handler: TimeoutHandler) {
        *self.timeout_handler.lock() = Some(handler);
    }

    /// The timeout handler, for the watchdog.
    pub fn timeout_handler(&self) -> Option<TimeoutHandler> {
        self.timeout_handler.lock().clone()
    }

    /// Schedules the deadline callback `ms` milliseconds from now. A later call replaces the
    /// previous deadline; `ms <= 0` disarms it.
    pub fn set_deadline(&self, ms: i64) {
        let at = if ms <= 0 { 0 } else { now_ms() + ms };
        self.deadline_at.store(at, Ordering::Release);
    }

    /// Lowers the connection timer to the actor deadline `ms` milliseconds from now when earlier.
    /// Returns the absolute due time in the [`now_ms`] clock. Nonpositive delays are due now; zero
    /// as a return value means the connection is closed and cannot service the deadline.
    pub fn deadline(&self, ms: i64) -> i64 {
        if !self.is_open.load(Ordering::Acquire) {
            return 0;
        }
        let due = now_ms() + ms.max(0);
        if self.lower_deadline(due) {
            // nearer than the one the loop waits for: it files the connection under it. A later one
            // (a lease renewed) costs nothing here - the timer fires at the old one and the hook
            // answers with the nearest
            self.on_loop(crate::lib::network::engine::DEADLINE, |p| unsafe { crate::lib::network::engine::arm_deadline_from_state(p) });
        }
        due
    }

    /// The timer of the connection, lowered to `at` when that is nearer (0 — nothing): true - it
    /// was.
    pub fn lower_deadline(&self, at: i64) -> bool {
        if at <= 0 {
            return false;
        }
        let mut cur = self.deadline_at.load(Ordering::Acquire);
        loop {
            if cur != 0 && cur <= at {
                return false;
            }
            match self.deadline_at.compare_exchange(cur, at, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(c) => cur = c,
            }
        }
    }

    /// How long this side may say nothing, in milliseconds (`External::pulse`); 0 - no pulse. `hears`
    /// - this side hears the `Pulse_` of its peer: the side that dialed answers it rather than saying
    /// its own. A UDP connection says no `Pulse_`: `ms` is the interval of its keepalive, filed anew
    /// on its loop.
    pub fn pulse(&self, ms: i64, hears: bool) {
        self.pulse_alone.store(!hears, Ordering::Relaxed);
        self.pulse_ms.store(ms.max(0), Ordering::Release);
        if let Some(u) = self.udp.get() {
            u.request(crate::lib::network::UDP::PULSE_);
        }
    }

    /// The peer said `Pulse_`: the side that dialed answers it, unless it said something in the last
    /// half of the pulse - the peer heard that.
    pub fn heard_pulse(&self) {
        let pulse = self.pulse_ms.load(Ordering::Acquire);
        if 0 < pulse && self.dialing.load(Ordering::Acquire) {
            self.say_pulse(pulse / 2);
        }
    }

    /// Says `Pulse_` if this side said nothing for `silent` milliseconds: the transmitter is woken
    /// to say it once its queue is empty. Neither while the session has not started (a WebSocket's
    /// upgrade) nor in a graceful close: the line is not silent there, it is ending.
    pub(crate) fn say_pulse(&self, silent: i64) {
        if self.is_active() && !self.upgrading.load(Ordering::Acquire) && !self.closing_gracefully.load(Ordering::Acquire) && silent <= now_ms() - self.last_tx_ms.load(Ordering::Relaxed) {
            self.pulsed.store(true, Ordering::Release);
            self.wake_transmit();
        }
    }

    /// The transmitter, its queue empty: `Pulse_` is to be said - once for each time
    /// [`say_pulse`](Self::say_pulse) asked it.
    #[inline]
    pub fn take_pulse(&self) -> bool {
        self.pulsed.load(Ordering::Acquire) && self.pulsed.swap(false, Ordering::AcqRel)
    }

    /// The wait of the stage ran out: no pack of the peer came in it. The timeout handler is asked
    /// as for the receive timeout ([`TimeoutKind::Receive`]): the milliseconds it still waits
    /// (`i64::MAX` - it stops waiting), or 0 - the connection is closed with the receive timeout
    /// event: a loss, an identified session is parked.
    pub fn receive_timed_out(&self) -> i64 {
        if !self.is_active() || self.closing_gracefully.load(Ordering::Acquire) {
            return 0;
        }
        match self.timeout_handler().map(|h| h(TimeoutKind::Receive)) {
            Some(TimeoutAnswer::Extend(ms)) if 0 < ms => return ms,
            Some(TimeoutAnswer::Disarm) => return i64::MAX,
            _ => {}
        }
        if !self.is_active() || self.closing_gracefully.load(Ordering::Acquire) {
            return 0; // the handler ended it, or began a graceful close
        }
        match self.link() {
            // SAFETY: on the loop of the socket, its slot is there
            Some((lp, key)) if lp.in_loop() => unsafe {
                crate::lib::network::engine::on_slot_kind(lp, key, crate::lib::network::engine::CLOSE, |p| {
                    crate::lib::network::engine::disconnect(p, if (*p).ws.is_some() { Event::WEBSOCKET_RECEIVE_TIMEOUT } else { Event::RECEIVE_TIMEOUT })
                })
            },
            // a UDP connection on its loop ends at once, as its receive timeout ends it: a request served later would find the
            // session parked meanwhile - by the CLOSE of its peer - and end the parking
            None if self.udp.get().is_some_and(|u| u.on_loop_thread()) => self.udp.get().unwrap().receive_timed_out(),
            _ => self.disconnect(Event::RECEIVE_TIMEOUT),
        }
        0
    }

    /// The socket of the session is gone: what was the socket's goes with it - the servicing starts
    /// from the next tick of the socket that follows, the WebSocket control frames and flags of the
    /// old socket, its Noise stream, its addresses. What is the session's stays: a parked one goes
    /// on with it on the next socket, the deadline of its actors included (armed again when that
    /// socket starts). On the loop of the connection.
    pub(crate) fn socket_gone(&self) {
        self.maintain_at.store(0, Ordering::Relaxed);
        *self.ws_urgent.lock() = None;
        self.ws_urgent_set.store(false, Ordering::Relaxed);
        self.ws_closing.store(false, Ordering::Relaxed);
        *self.ws_close.lock() = None;
        *self.noise.lock() = None;
        *self.addresses.lock() = (None, None);
    }

    /// The servicing of this connection on the tick of its loop (see [`MaintenanceHook`]): from the
    /// next tick on.
    pub fn set_maintenance(&self, hook: MaintenanceHook) {
        *self.maintenance.lock() = Some(hook);
        self.maintain_at.store(0, Ordering::Relaxed);
        self.maintained.store(true, Ordering::Release);
    }

    /// The servicing, when its time has come by `now` (the loop's nanoseconds): run, and its next
    /// time set. On the loop of the connection.
    #[inline]
    pub(crate) fn maintain(&self, now: i64) {
        if !self.maintained.load(Ordering::Acquire) || now < self.maintain_at.load(Ordering::Relaxed) {
            return;
        }
        let hook = self.maintenance.lock().clone(); // code of the application: not under the lock
        let Some(h) = hook else { return };
        let ms = h(self);
        self.maintain_at.store(if ms == i32::MAX { i64::MAX } else { now + ms.max(0) as i64 * 1_000_000 }, Ordering::Relaxed);
    }

    pub fn set_deadline_hook(&self, hook: DeadlineHook) {
        *self.deadline_hook.lock() = Some(hook);
    }

    /// Fire the deadline hook once (the watchdog's check). Returns false when there is no hook or
    /// no armed deadline.
    pub fn fire_deadline(&self, now: i64) -> bool {
        let at = self.deadline_at.load(Ordering::Acquire);
        if at == 0 || now < at {
            return false;
        }
        // Claim the firing: exactly one caller gets the hook.
        if self
            .deadline_at
            .compare_exchange(at, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let hook = self.deadline_hook.lock().clone();
        if let Some(h) = hook {
            let next = h(now);
            if self.is_open() {
                self.lower_deadline(next);
            }
            return true;
        }
        false
    }

    /// A session that came from another connection files its deadlines here: the hook is asked
    /// with `i64::MIN` - nothing fires, the nearest comes back - and the timer is lowered to it.
    pub(crate) fn refile_deadline(&self) {
        let hook = self.deadline_hook.lock().clone(); // code of the session: not under the lock
        if let Some(h) = hook {
            self.lower_deadline(h(i64::MIN));
        }
    }

    /// Fire a lifecycle event. Lock-free fast-path when no sink is installed; on the slow path we
    /// clone the `Arc` out, drop the guard, and only THEN invoke the user callback so a slow user
    /// closure cannot block other firings.
    #[inline]
    pub fn fire(&self, event: Event) {
        self.notify(event);
        // the session can be used - the socket, the upgrade, the seat behind a mux: OnOpen after
        // the event, while the connection is still there; a WebSocket opens with its upgrade, not
        // with the socket
        if (event.is_connect() || event.is_ready()) && self.is_open() && !self.upgrading.load(Ordering::Acquire) {
            self.session_opened();
        }
    }

    /// The event to whoever hears the events of this connection, and nothing else: no session is
    /// opened by it.
    #[inline]
    pub(crate) fn notify(&self, event: Event) {
        // Fast path: no sink installed → skip the mutex entirely.
        if self.event_sink_set.load(Ordering::Acquire) {
            let cb = {
                let guard = self.event_sink.lock();
                guard.clone()
            };
            if let Some(f) = cb {
                f(self.index, event);
            }
        }
    }

    /// Closes the connection at once with `event` - [`Self::disconnect`].
    pub fn mark_closed(&self, event: Event) {
        self.disconnect(event);
    }

    /// A timeout set: negative - the graceful close begins, the magnitude is the deadline.
    #[inline]
    fn timeout(&self, at: &AtomicI64, ms: i32) {
        at.store((ms as i64).abs().min(i32::MAX as i64), Ordering::Release); // i32::MIN too: its magnitude reads back as an i32
        if ms < 0 {
            self.closing_gracefully.store(true, Ordering::Release);
        }
    }

    /// The graceful close: what is queued drains, then the FIN (a WebSocket's CLOSE frame), then
    /// the wait for the peer's, bounded by the linger.
    pub fn close_gracefully(&self) {
        if !self.is_open.load(Ordering::Acquire) {
            return;
        }
        self.closing_gracefully.store(true, Ordering::Release);
        self.linger_deadline.store(now_ms() + self.linger_timeout_ms.load(Ordering::Acquire), Ordering::Release);
        self.on_loop(crate::lib::network::engine::GRACE, |p| unsafe { crate::lib::network::engine::grace(p) });
    }
}

/// The clock of the timeouts, deadlines and the linger: a monotonic instant and the epoch
/// milliseconds it was, both taken once. Times are epoch milliseconds that run on the monotonic
/// clock - a step of the wall clock back moves no deadline - with the sleep of the machine taken in
/// ([`clock`](crate::lib::network::engine::clock)).
pub(crate) fn clock_base() -> &'static (std::time::Instant, i64) {
    static BASE: std::sync::OnceLock<(std::time::Instant, i64)> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        (std::time::Instant::now(), epoch)
    })
}

/// Current time in milliseconds — shared clock of the timeouts, deadlines and the linger
/// ([`clock_base`]).
pub(crate) fn now_ms() -> i64 {
    clock_base().1 + crate::lib::network::engine::clock() / 1_000_000
}

/// The clock of the parked sessions: unix SECONDS, off the same monotonic base as [`now_ms`].
#[inline]
pub(crate) fn now_sec() -> i64 {
    now_ms() / 1000
}

impl std::fmt::Display for ExternalConnection {
    /// Formats the session identifier, host name, and transport location or closed state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let addresses = *self.addresses.lock();
        let host = self.host.upgrade();
        let name = host.as_ref().map_or("", |h| h.name.as_str());
        let id = self.session_id.load(Ordering::Acquire) as u32; // the identity of the session
        match (self.is_open.load(Ordering::Acquire), addresses) {
            (true, (Some(local), Some(remote))) => write!(f, "[{}] {name} {local}:{remote}", id),
            (true, _) => write!(f, "[{}] {name}", id),
            (false, _) => write!(f, "[{}] {name} : closed", id),
        }
    }
}

impl ExternalConnection {

    #[inline] pub fn receive_timeout_ms(&self) -> i32 {
        if !self.is_open.load(Ordering::Acquire) {
            return i32::MIN;
        }
        let ms = self.receive_timeout_ms.load(Ordering::Acquire) as i32;
        if self.closing_gracefully.load(Ordering::Acquire) { -ms } else { ms }
    }
    #[inline] pub fn set_receive_timeout_ms(&self, ms: i32) {
        self.timeout(&self.receive_timeout_ms, ms);
        if let Some(u) = self.udp.get() {
            // negative: the graceful close, asked at once; else the wait is filed anew on the loop
            u.request(if ms < 0 { crate::lib::network::UDP::CLOSE_ } else { crate::lib::network::UDP::TIMEOUT_ });
        }
    }

    #[inline] pub fn transmit_timeout_ms(&self) -> i32 {
        let ms = self.transmit_timeout_ms.load(Ordering::Acquire) as i32;
        if self.closing_gracefully.load(Ordering::Acquire) { -ms } else { ms }
    }
    #[inline] pub fn set_transmit_timeout_ms(&self, ms: i32) {
        self.timeout(&self.transmit_timeout_ms, ms);
        if ms < 0 {
            if let Some(u) = self.udp.get() {
                u.request(crate::lib::network::UDP::CLOSE_); // the graceful close, asked at once
            }
        }
    }

    #[inline] pub fn is_open(&self) -> bool {
        self.is_open.load(Ordering::Acquire)
    }

    /// Graceful close: drain the write half, then `shutdown(Send)`, then linger for the peer's FIN.
    pub fn close(&self) {
        self.close_gracefully();
    }

    /// Terminal close — equivalent to [`Self::close`] in the Rust port: the registry slot is
    /// released by the pump's teardown path.
    pub fn close_and_dispose(&self) {
        self.close_gracefully();
    }

    /// Abort — drops pending I/O immediately: the loop tears the connection down where it stands, a
    /// FIN goes out (there is no portable "abortive close / RST on demand").
    pub fn abort(&self) {
        if let Some(u) = self.udp.get() {
            // a UDP slot: whatever its state - connecting, closing, parked - its loop ends it
            return u.request(crate::lib::network::UDP::ABORT_);
        }
        self.disconnect(Event::THIS_CLOSE_ABRUPTLY);
    }

    /// A loss, not our close: an identified session parks when it may.
    pub fn lost(&self) {
        // a loss, not our close: a graceful close under way is undone, the session parks when it
        // may
        self.closing_gracefully.store(false, Ordering::Relaxed);
        self.grace_undone.store(true, Ordering::Relaxed);
        self.disconnect(Event::REMOTE_CLOSE_ABRUPTLY);
    }

    /// Attempts to claim the parked session identified by `session_id` and stages it for this
    /// connection. Falls back to identifying the current connection when no session is available.
    /// Datagram connections delegate the entire move to their transport.
    pub fn reclaim(&self, session_id: i64) -> i64 {
        if let Some(u) = self.udp.get() {
            u.reclaim(session_id as u64); // the transport moves the parked session here
            return self.session() as i64;
        }
        if let Some(saved) = self.claim(session_id as u64) {
            self.save_session(saved);
            self.unidentify(); // the identity is applied again below: the login may have identified this connection already
        }
        ExternalConnection::identify(self, session_id as u32) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_fires_and_rearms_with_the_nearest() {
        let state = ExternalConnection::new(ConnId(5));
        let fired = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let f = fired.clone();
        state.set_deadline_hook(Arc::new(move |now| {
            f.store(now, Ordering::Relaxed);
            now + 500
        }));
        state.is_open.store(false, Ordering::Release);
        assert_eq!(state.deadline(1000), 0, "closed: nothing times it");
        state.is_open.store(true, Ordering::Release);
        let due = state.deadline(1000);
        assert!(0 < due);
        state.lower_deadline(due + 100); // later: the nearer stands
        assert!(!state.fire_deadline(due - 1));
        assert!(state.fire_deadline(due));
        assert_eq!(fired.load(Ordering::Relaxed), due);
        assert!(!state.fire_deadline(due + 499));
        assert!(state.fire_deadline(due + 500)); // re-armed by the hook's answer
        let now = now_ms();
        assert!(now <= state.deadline(0) && state.deadline(0) <= now_ms(), "0 is due at once ( Java )");
    }

    #[test]
    fn close_is_idempotent_and_fires_once() {
        let state = ExternalConnection::new(ConnId(7));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = count.clone();
        state.set_event_sink(Arc::new(move |_id, _ev| {
            c.fetch_add(1, Ordering::Relaxed);
        }));
        state.close();
        state.close();
        state.abort();
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(!state.is_open());
    }

    #[test]
    fn fire_without_sink_is_lock_free() {
        // No sink installed → `fire` must not touch the mutex; we detect that indirectly by holding
        // the mutex on a separate thread for the duration of the fire call. If `fire` took the
        // lock, this would deadlock.
        let state = ExternalConnection::new(ConnId(0));
        let guard = state.event_sink.lock();
        state.fire(Event::REMOTE_CONNECT); // would deadlock if it locked
        drop(guard);
    }

    #[test]
    fn timeouts_are_atomic_round_trip() {
        let state = ExternalConnection::new(ConnId(0));
        state.is_open.store(true, Ordering::Release);
        state.set_receive_timeout_ms(1234);
        state.set_transmit_timeout_ms(5678);
        assert_eq!((state.receive_timeout_ms(), state.transmit_timeout_ms()), (1234, 5678));
        state.is_open.store(false, Ordering::Release);
        assert_eq!(state.receive_timeout_ms(), i32::MIN, "closed");
    }

    /// The sign says a graceful close is under way, whoever began it - a negative timeout of either
    /// side, or the close itself - and a timeout set after keeps it: the sender reads it to refuse
    /// new packs.
    #[test]
    fn a_graceful_close_reads_negative_on_both_sides_and_stays() {
        let state = ExternalConnection::new(ConnId(0));
        state.is_open.store(true, Ordering::Release);
        state.set_receive_timeout_ms(1234);
        state.set_transmit_timeout_ms(-9999);
        assert_eq!((state.receive_timeout_ms(), state.transmit_timeout_ms()), (-1234, -9999));
        state.set_transmit_timeout_ms(500);
        assert_eq!((state.receive_timeout_ms(), state.transmit_timeout_ms()), (-1234, -500), "a timeout set does not end the close");
        assert_eq!(state.receive_timeout_ms.load(Ordering::Relaxed), 1234, "the deadline is the magnitude");

        let state = ExternalConnection::new(ConnId(1));
        state.is_open.store(true, Ordering::Release);
        state.set_receive_timeout_ms(1000);
        state.closing_gracefully.store(true, Ordering::Release); // as close_gracefully does
        assert_eq!(state.receive_timeout_ms(), -1000);

        let state = ExternalConnection::new(ConnId(2));
        state.is_open.store(true, Ordering::Release);
        state.set_transmit_timeout_ms(i32::MIN); // the least: its magnitude does not fit, it reads as the largest
        assert_eq!(state.transmit_timeout_ms(), -i32::MAX);
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;

    #[test]
    fn connection_state_overrides() {
        let state = ExternalConnection::new(ConnId(3));
        let s = state.identify(9);
        assert_eq!(s, (3u64 << 32) | 9);
        assert_eq!(state.session(), (3u64 << 32) | 9);
        // Deadline returns the ABSOLUTE firing time.
        let d = state.deadline(60_000);
        assert!(now_ms() < d && d <= now_ms() + 60_000, "absolute epoch-ms: {d}");
        assert!(state.deadline(0) <= now_ms(), "0 is due at once ( Java )");
        state.wake_transmit(); // wakes the write half, no panic
    }
}
