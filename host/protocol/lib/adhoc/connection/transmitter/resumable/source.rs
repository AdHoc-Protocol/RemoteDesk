// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! The journal seated on a stream: the sending side of a socket pulls the packs of the transmitter,
//! copies the bytes of a guaranteed one into the journal as they are produced, and at a pack
//! boundary gives the replay that is due and the packs the two sides agree by. The session - the
//! journal, the control, the position - lives in the generated connection and outlives a socket;
//! the source of a socket is made again for the next one.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::lib::adhoc::connection::control::Control;
use crate::lib::adhoc::connection::transmitter::resumable::{Journal, Resumable, FREE, PULL};
use crate::lib::adhoc::connection::transmitter::{TxHost, TxState};

/// The session state the generated connection keeps for its guaranteed packs: what
/// [`ResumableSource`] works with.
pub trait Session {
    /// The journal of what this side sends guaranteed; `None` — it guarantees nothing.
    fn journal(&self) -> Option<&Journal>;
    fn control(&self) -> &Control;
    /// The transport connection of the session: a failed journal is reported to it and aborts it.
    /// Default: none.
    fn ext_connection(&self) -> Option<Arc<dyn crate::lib::adhoc::connection::external::External>> {
        None
    }
    /// The pack the two sides agree by of the kind `kind` ([`Control::next`]), carrying `value`,
    /// serialized into `out` by the generated code: its bytes. `out` holds
    /// [`Session::service_room`] bytes at least.
    fn service(&self, _kind: u32, _value: u64, _out: &mut [u8]) -> usize {
        0
    }
    /// The bytes of the largest of the packs the two sides agree by.
    fn service_room(&self) -> usize {
        0
    }
    /// The session lives in files of its own ([`Control`]): the name of the connection they are named by. `None` - in memory
    /// only: a datagram transport, a virtual connection. Default: none.
    fn files(&self) -> Option<&'static str> {
        None
    }
    /// The connection the session is of: by which `Pool::restore` makes it.
    fn kind(&self) -> u32 {
        0
    }
    /// What of the id bytes of a pack is the id of the pack: the width of a cell of the trace of the queue.
    fn id_mask(&self) -> u32 {
        u32::MAX
    }
    /// The generated `save`: what the session is, appended to `out`; `false` - the next process does not take it up.
    fn save(&self, _out: &mut Vec<u8>) -> bool {
        false
    }
    /// The generated `load`: what `save` wrote, read back.
    fn load(&self, _from: &mut &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    /// The position of the receiver in the peer's journal, when this side receives guaranteed packs: touched on the thread
    /// that runs the session, or before it carries anything.
    fn position(&self) -> Option<&crate::lib::adhoc::utils::OwnCell<crate::lib::adhoc::connection::receiver::Position>> {
        None
    }
    /// Where the transmitter traces its queue: the header of the files of the session, published.
    fn trace(&self) -> Option<&std::sync::atomic::AtomicPtr<u8>> {
        None
    }
}

/// What of the journal of the session the peer has not confirmed ([`unconfirmed`]): what a client
/// dials again for and judges the speed of its link by (`Network::TCP::Redial`).
#[derive(Clone, Copy, Default)]
pub struct Unconfirmed {
    /// The journal holds bytes the peer has not confirmed: a session that lost its socket has them
    /// to send again on the next one - a dialing side dials again for them.
    pub held: bool,
    /// The peer owes a confirmation: a step's worth of the journal ([`Control::step`]) has been
    /// sent since the last one. Fewer unconfirmed bytes say nothing - the peer confirms once a
    /// step, so bytes it has long received stay unconfirmed until the step fills.
    pub owed: bool,
}

/// What of the journal of the session the peer has not confirmed ([`Unconfirmed`]). From any
/// thread: the positions the journal published.
pub fn unconfirmed<T: Session>(conn: &T) -> Unconfirmed {
    conn.journal().map_or(Unconfirmed::default(), |j| Unconfirmed {
        held: j.unconfirmed(),
        owed: conn.control().step() <= j.owed(),
    })
}

/// The room a call of the source gets at least.
const ROOM: usize = 8192;

/// The packs of a connection that guarantees packs, straight into the pump's output buffer: the
/// journal keeps the bytes of the guaranteed ones, the replay and the packs of the agreement go at
/// pack boundaries.
pub struct ResumableSource<T>
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    conn: Arc<T>,
    tx: TxState,
    /// Its number among the sources of the session ([`Control`] `sources`).
    number: u32,
    /// The number of the session it is of ([`Resumable`] `session`): one over keeps no pack cut for
    /// the next.
    session: u32,
    /// The last fill stopped at the cap of the journal: packs wait in the queue for a report of the
    /// peer, which wakes the transmit.
    capped: bool,
    /// It holds the journal of the session ([`Journal`]): from the first fill that finds it free -
    /// the source of the socket before, or a pull of one pack, lets it go and wakes the transmit -
    /// until it is dropped.
    holds: bool,
    /// The bytes of the last fill are frames of tunnels with a Noise of their own ([`TxState::clear`]).
    clear: bool,
}

impl<T> ResumableSource<T>
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    /// The source of a socket of the session of `conn`: a guaranteed pack the socket before cut
    /// goes on first.
    pub fn new(conn: Arc<T>) -> Self {
        conn.control().silent(); // what was to be said on the socket before is not said on this one
        let control = conn.control();
        let number = match conn.journal() {
            Some(j) => {
                let mut n = control.sources.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
                while n == FREE || n == PULL {
                    n = control.sources.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
                    // the numbers of a source only
                }
                j.set_offline(false); // the queue is this source's: an offline pull in progress ends its pack
                n
            }
            None => 0,
        };
        let mut s = Self {
            conn,
            tx: TxState::default(),
            number,
            session: 0,
            capped: false,
            holds: false,
            clear: false,
        };
        s.hold();
        s
    }

    /// Takes the journal of the session, if nobody holds it, and sets it up for this socket: nothing
    /// goes out guaranteed until the peer names its position, a guaranteed pack the socket before
    /// cut goes on first. `false` - another holds it: the source of the socket before, or a pull of
    /// one pack, which wakes the transmit as it lets it go. No journal: `true`.
    fn hold(&mut self) -> bool {
        if self.holds {
            return true;
        }
        let Some(j) = self.conn.journal() else { return true };
        if !j.take(self.number) {
            return false;
        }
        if self.conn.control().sources.load(Ordering::Acquire) != self.number {
            j.give(); // a newer source has the session: the journal is its
            return false;
        }
        self.holds = true;
        // SAFETY: taken
        let r = unsafe { j.get() };
        r.rewind();
        if let Some(mut kept) = r.kept() {
            kept.keys = std::mem::take(&mut self.tx.keys); // the keys of this socket
            kept.envelope = self.tx.envelope.take();
            self.tx = kept;
        }
        self.session = r.session();
        true
    }

    /// The keys of the session: a ChaCha20 stage of a pack chain takes the newest there.
    pub fn keys(mut self, keys: Arc<crate::lib::adhoc::connection::keyring::Keyring>) -> Self {
        self.tx.keys = keys;
        self
    }

    /// The keys of the Envelope stages of the connection ( [`crate::lib::chain::Envelope`] ).
    pub fn envelope(mut self, envelope: Arc<crate::lib::chain::Envelope>) -> Self {
        self.tx.envelope = Some(envelope);
        self
    }

    /// Fills `out[from..]`: returns the end of what is written; `None` — the journal failed, the
    /// session ends.
    fn fill(&mut self, out: &mut [u8], from: usize) -> Option<usize> {
        if !self.hold() {
            self.capped = true; // the holder lets the journal go and wakes the transmit
            return Some(from);
        }
        let conn = &*self.conn;
        // SAFETY: this source holds the journal
        let mut journal: Option<&mut Resumable> = conn.journal().map(|j| unsafe { j.get() });
        let control = conn.control();
        if let Some(j) = journal.as_deref_mut() {
            if j.is_parked() && control.take_online() {
                j.online(); // the generated resume(connection): the session is identified on this socket
                if let Some(m) = conn.journal() {
                    m.attached(j); // the session got its files meanwhile ( Control::live ): the journal moves into them
                }
            }
        }
        let mut host: &T = conn;
        let mut at = from;
        let end = out.len();
        let mut failed = None;
        self.capped = false;
        // the kind of the bytes of this fill: -1 none yet, 0 sealed, 1 frames in the clear - a fill gives bytes of one kind
        let mut kind: i8 = -1;
        while at < end {
            if !self.tx.is_open() {
                if let Some(j) = journal.as_deref_mut() {
                    j.hold_at_next();
                    control.apply(j);
                }
                // no pack of the agreement between the bytes of a replay: it may stop in the middle
                // of a pack; while one is due a confirmation waits - the answer goes right before
                // the bytes, they may go on with a pack the peer holds in part
                let (begun, due) = journal.as_deref().map_or((false, false), |j| (j.replay_begun(), j.replay_due()));
                if !begun && control.pending(due) {
                    if end - at < conn.service_room() || kind == 1 {
                        break; // no room for the pack, or frames in the clear before it: it goes first in the next buffer
                    }
                    if let Some((service, value)) = control.next(due) {
                        kind = 0;
                        at += conn.service(service, value, &mut out[at..]);
                        control.said(service, journal.as_deref_mut());
                        continue;
                    }
                }
                if let Some(j) = journal.as_deref_mut() {
                    if control.agreeing() {
                        break; // a new socket: nothing but the packs of the agreement until the position the peer names is
                               // answered
                    }
                    // Between the identification and the agreement nothing leaves - see the order in the
                    // description of the module resumable:
                    //   is_awaiting - the position the peer names is not applied yet;
                    //   !is_parked  - the session is identified: the generated resume(connection) put the stage online;
                    //   !dialing    - the side that took the connection: the side that made it sends its login now.
                    // An unguaranteed pack let out here would reach a decoder that holds a pack in progress and be
                    // glued into it.
                    if j.is_awaiting() && !j.is_parked() && conn.ext_connection().is_some_and(|x| !x.dialing()) {
                        break; // the position the peer names wakes the transmit
                    }
                    if kind == 1 && j.sent_ < j.next_ && !j.is_holding() {
                        break; // the replay is sealed: not behind frames in the clear
                    }
                    match j.replay_into(&mut out[at..]) {
                        Ok(0) => {}
                        Ok(n) => {
                            at += n;
                            kind = 0;
                            continue;
                        }
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                    if !j.has_room() {
                        self.capped = true;
                        break; // the cap: no pack leaves the queue until the peer reports
                    }
                }
            }
            // the cap holds a pack in the middle too: no more than is left under it, none below
            // ROOM - a report of the peer wakes the transmit
            let stop = match journal.as_deref_mut() {
                Some(j) => {
                    control.acks(j);
                    let free = j.free();
                    if free < crate::lib::adhoc::connection::transmitter::resumable::ROOM {
                        self.capped = true;
                        break;
                    }
                    end.min(at.saturating_add(free))
                }
                None => end,
            };
            self.tx.before = kind;
            let (n, _, _) = self.tx.read_one(&mut out[at..stop], &mut host);
            if n <= 0 {
                // a pack that waits behind the bytes of this fill - for room, or of the other kind - is no empty queue
                if let (Some(j), true) = (journal.as_deref_mut(), n < 0 || kind < 0) {
                    if let Err(e) = j.drained() {
                        failed = Some(e); // the queue is empty: what the last syncs passed by went to the disk - or did not
                    }
                }
                break; // nothing to send, or the pack waits for room
            }
            let n = n as usize;
            kind = self.tx.clear as i8;
            if self.tx.guaranteed {
                if let Some(j) = journal.as_deref_mut() {
                    match j.journal(&out[at..at + n]) {
                        Ok(true) => at += n,
                        Ok(false) => {} // journalled only: they go out with the replay
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                    if !self.tx.is_open() {
                        if let Err(e) = j.pack_ended() {
                            failed = Some(e);
                            break;
                        }
                    }
                    continue;
                }
            }
            at += n;
        }
        drop(journal);
        self.tx.before = -1;
        self.clear = kind == 1;
        if let Some(j) = conn.journal() {
            j.publish(); // what the other threads read of the journal
        }
        if let Some(e) = failed {
            // the close of the session asks the holder - this source - to empty the journal: it does
            // as it lets it go
            if let Some(x) = conn.ext_connection() {
                x.failure(&e); // Journal storage has already been invalidated; abort the transport session.
                x.abort();
            }
            return None;
        }
        Some(at)
    }
}

/// The socket is gone: a guaranteed pack it cut in the middle is kept for the next socket of the
/// session, which finishes it into the journal - the peer's position is behind it, the replay
/// brings it whole. A pack of no guarantee is lost with the socket: the next one gives its place
/// back (the generated `on_serializing`); so is any pack of a session over - the next session
/// starts on a pack boundary.
impl<T> Drop for ResumableSource<T>
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    fn drop(&mut self) {
        let parked = self.conn.ext_connection().is_some_and(|x| x.is_parked());
        let Some(j) = self.conn.journal() else {
            if parked {
                self.conn.control().parked(&*self.conn); // the session waits for its next socket: since when, to its header
            }
            return;
        };
        let control = self.conn.control();
        // a newer source has the session (a reclaim moved it): the journal is its, this one only lets
        // it go
        let current = control.sources.load(Ordering::Acquire) == self.number;
        if current {
            control.forget(); // a CONTINUE of this socket not applied: the next socket waits for its own
            if self.holds {
                // SAFETY: this source holds the journal
                let r = unsafe { j.get() };
                if r.session() == self.session && self.tx.is_open() && self.tx.guaranteed {
                    r.keep(std::mem::take(&mut self.tx));
                }
                if parked {
                    // the session waits for its next socket: the journal goes to the disk and grows
                    // no more in memory
                    if let Err(e) = r.close(true) {
                        crate::lib::adhoc::connection::transmitter::on_error::tx_at(
                            crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error,
                            &"the journal",
                            &"could not be parked",
                            Some(&e),
                        );
                    }
                }
            } else if parked {
                j.park(); // never held: whoever holds the journal parks it as it lets it go
            }
            j.set_offline(true); // until the next socket, the thread that queues a pack journals it
        }
        if self.holds {
            j.give();
            if !current {
                if let Some(x) = self.conn.ext_connection() {
                    x.wake_transmit(); // the newer source waits for the journal
                }
            }
        }
        if current && parked {
            control.parked(&*self.conn); // the session waits for its next socket: since when, and what it is, to its header
            journal_queue(&*self.conn); // what the socket left queued: no put wakes for it
        }
    }
}

impl<T: Send + Sync> crate::lib::adhoc::BytesSrc for ResumableSource<T>
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    #[inline]
    fn at_boundary(&self) -> bool {
        !self.tx.is_open()
    }

    #[inline]
    fn journal(&self) -> Option<&Journal> {
        self.conn.journal()
    }

    #[inline]
    fn holds_journal(&mut self) -> bool {
        self.hold()
    }

    fn with_journal(&mut self, f: &mut dyn FnMut(&mut Resumable)) -> bool {
        if !self.hold() {
            return false;
        }
        let Some(j) = self.conn.journal() else { return false };
        // SAFETY: this source holds the journal
        f(unsafe { j.get() });
        j.publish();
        true
    }

    #[inline]
    fn online(&self) {
        self.conn.control().online();
    }

    /// The peer asks the stream to go on from `position`: a guaranteed pack in the middle of
    /// serialization is finished into the journal and not sent - what the peer has of it is in the
    /// journal, the rest follows from there; one of no guarantee is dropped - the peer dropped its
    /// head. Then the journal turns.
    fn turn(&mut self, position: i64) -> std::io::Result<i64> {
        if !self.hold() {
            return Ok(-1); // a transport asks once the journal is held ( holds_journal )
        }
        let Some(m) = self.conn.journal() else {
            self.tx.abandon();
            return Ok(-1);
        };
        // SAFETY: this source holds the journal
        let j = unsafe { m.get() };
        if self.tx.is_open() {
            if self.tx.guaranteed {
                let mut host: &T = &self.conn;
                let mut buf = [0u8; ROOM];
                while self.tx.is_open() {
                    let (n, _, _) = self.tx.read_one(&mut buf, &mut host);
                    if n <= 0 {
                        break; // the transmitter made no step: the rest of the pack waits
                    }
                    if let Err(e) = j.journal(&buf[..n as usize]) {
                        return Err(e); // the journal failed: the session ends
                    }
                }
                if !self.tx.is_open() {
                    j.pack_ended()?;
                }
            } else {
                self.tx.abandon();
            }
        }
        let at = j.turn(position);
        m.publish();
        Ok(at)
    }

    /// No journal to send it again: the pack in flight is dropped.
    #[inline]
    fn abandon(&mut self) {
        self.tx.abandon();
    }

    #[inline]
    fn clear(&self) -> bool {
        self.clear
    }

    /// Straight into the transport's buffer: the packs of the agreement, the replay, then new packs
    /// - journalled as they go.
    fn read(&mut self, dst: &mut [u8]) -> isize {
        match self.fill(dst, 0) {
            None => -1, // the journal failed
            // a pack in the middle - waiting for room, or for its producer - or the cap of the
            // journal: the source is not empty, a graceful close waits for it
            Some(0) if self.tx.is_open() || self.capped => 0,
            Some(0) => -1,
            Some(n) => {
                let host: &T = &self.conn;
                self.tx.gave(n as isize, host.speed())
            }
        }
    }
}

/// A pack is queued while the session is parked: no transmit runs, so the thread that queued it
/// pulls the queue into the journal - every guaranteed pack, up to the cap; one of no guarantee has
/// no socket to go on and gives its place back. A pack queued while a pull is in progress makes
/// that pull go on. A pull holds the journal for one pack: the source of the next socket takes over
/// between two pulls - woken as the pulls end -, and a pack the cap cut goes on there.
pub fn journal_queue<T>(conn: &T)
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    let Some(m) = conn.journal() else { return };
    let control = conn.control();
    if control.draining.fetch_add(1, std::sync::atomic::Ordering::AcqRel) != 0 {
        return; // the pull in progress sees the count changed and pulls again
    }
    pull_queued(conn, m);
}

/// `f` with no pull of the parked session in flight: a pull takes its packs through the actors of the session on the thread
/// that queued them ( [`journal_queue`] ), so the code that touches those actors with no lock from the thread of the connection
/// - its events, its deadlines - waits for the pull in flight ( a pack at most ) and holds the next ones off; what was queued
/// meanwhile it pulls itself, as the pull would have. A session with no journal has no pulls: `f` at once.
pub fn excluding_pulls<T, R>(conn: &T, f: impl FnOnce() -> R) -> R
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    let Some(m) = conn.journal() else { return f() };
    let control = conn.control();
    while control
        .draining
        .compare_exchange_weak(0, 1, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Relaxed)
        .is_err()
    {
        std::hint::spin_loop(); // a pull in flight: one pack
    }
    let r = f();
    if control
        .draining
        .compare_exchange(1, 0, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire)
        .is_err()
    {
        pull_queued(conn, m); // packs queued meanwhile: their threads left them to the holder
    }
    r
}

/// The pulls of [`journal_queue`] for the count `draining` holds, until no pack queued meanwhile is left.
fn pull_queued<T>(conn: &T, m: &Journal)
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    let control = conn.control();
    loop {
        let seen = control.draining.load(std::sync::atomic::Ordering::Acquire); // what was queued before is served by this pass
        loop {
            match pull_offline(conn, m) {
                Ok(true) => {} // a pack is in: the journal was let go, the source of a new socket may take over now
                Ok(false) => break,
                Err(e) => {
                    // the journal is emptied already
                    crate::lib::adhoc::connection::transmitter::on_error::tx_at(
                        crate::lib::adhoc::connection::transmitter::on_error::TxOnError::Error,
                        &"the journal of a parked session",
                        &"failed",
                        Some(&e),
                    );
                    break;
                }
            }
        }
        if control
            .draining
            .compare_exchange(seen, 0, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire)
            .is_ok()
        {
            break;
        }
    }
    if !m.is_offline() {
        if let Some(x) = conn.ext_connection() {
            x.wake_transmit(); // a socket took the session meanwhile: its source waits for the journal
        }
    }
}

/// The generated `save`: the journal of the session of `conn` is in the files of the session ([`Control`]), nothing of it goes
/// to `out` - what the transmitter holds goes into it when the process ends ([`Control::stop`]). `false` — no journal.
pub fn save<T>(conn: &T, _out: &mut Vec<u8>) -> std::io::Result<bool>
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    Ok(conn.journal().is_some())
}

/// One offline pull of [`journal_queue`]: one pack of the queue into the journal while the session
/// has no socket, holding the journal - the source of a new socket waits for one pack at most.
/// `true` — a pack is in, more may wait; `false` — the queue is empty, the cap is met, or a socket
/// took the queue.
pub(crate) fn pull_offline<T>(conn: &T, m: &Journal) -> std::io::Result<bool>
where
    for<'a> &'a T: TxHost,
    T: Session,
{
    if !m.is_offline() || conn.ext_connection().is_some_and(|x| x.is_open()) {
        return Ok(false); // a socket took the queue
    }
    if !m.take(PULL) {
        return Ok(false); // a source holds the journal: the queue is its
    }
    if !m.is_offline() {
        m.give();
        return Ok(false); // a socket took the queue meanwhile
    }
    // SAFETY: taken
    let j = unsafe { m.get() };
    let control = conn.control();
    let mut tx = j.kept().unwrap_or_else(|| {
        let mut tx = TxState::default();
        if let Some(s) = conn.ext_connection().and_then(|x| x.state()) {
            tx.keys = s.keys(); // a ChaCha20 stage of a pack chain takes the keys of the session
        }
        tx.envelope = conn.envelope(); // and an Envelope stage, those of the connection
        tx
    });
    tx.journal_only = true;
    let mut host: &T = conn;
    // SAFETY: the journal is taken ( `PULL` ): nothing else pulls into the scratch meanwhile
    let r = unsafe {
        control.scratch.lend(|scratch| {
            if scratch.is_empty() {
                scratch.resize(ROOM, 0);
            }
            loop {
                let free = j.free();
                if free < crate::lib::adhoc::connection::transmitter::resumable::ROOM {
                    break Ok(false); // the cap: the rest waits in the queue
                }
                let room = free.min(scratch.len());
                let (n, _, _) = tx.read_one(&mut scratch[..room], &mut host);
                if n <= 0 {
                    break j.drained().map(|_| false); // the queue is empty: what the last syncs passed by goes to the disk now
                }
                if let Err(e) = j.journal(&scratch[..n as usize]) {
                    break Err(e);
                }
                if !tx.is_open() {
                    if let Err(e) = j.pack_ended() {
                        break Err(e);
                    }
                    break Ok(true); // the pack is whole: the journal is let go between two packs
                }
            }
        })
    };
    tx.journal_only = false;
    j.keep(tx); // for the next pull, offline or of the next socket: a pack the cap cut goes on with it
    m.give();
    r
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
