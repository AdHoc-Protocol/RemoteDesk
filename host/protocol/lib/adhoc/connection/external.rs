// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Transport controls exposed to a protocol connection.
//!
//! Implementations represent TCP, WebSocket, or tunnel connections. Timeouts use milliseconds; a
//! negative transmit timeout requests closing after the pending output has been sent.

use crate::lib::network::TCP::external_connection::ExternalConnection;

/// What happened to the external connection: the event of the transport itself — `is_connect`,
/// `is_close`, `is_timeout`, `is_on_receive` …
pub use crate::lib::network::TCP::external_connection::Event;

pub trait External: Send + Sync {
    fn is_open(&self) -> bool;
    /// A pack is queued while the transmit runner sleeps: it is to wake.
    fn wake_transmit(&self);
    /// The receive is parked on bytes its consumer could not take (a relay whose outbound side is
    /// behind): the consumer has room again. Default: nothing to wake.
    fn wake_receive(&self) {}
    fn receive_timeout(&self) -> i32;
    fn set_receive_timeout(&self, ms: i32);
    fn transmit_timeout(&self) -> i32;
    fn set_transmit_timeout(&self, ms: i32);
    /// Closes at once, whatever is in flight.
    fn abort(&self);
    /// The socket is lost and the session waits, parked, for its peer to come back: packs may be
    /// queued meanwhile, they go with the next socket of the session.
    fn is_parked(&self) -> bool {
        false
    }
    /// This thread runs the connection - its receive, its transmit, its events, one thing at a time: what only its code
    /// touches needs no lock here. Default: no thread of its own.
    fn on_its_thread(&self) -> bool {
        false
    }
    /// Closes once what is in flight is out.
    fn close(&self) {
        self.abort()
    }
    /// The deadline of an actor instance `ms` from now: the connection times it with its own.
    /// Returns the due time; 0 — none (`ms` is none, or the connection times nothing).
    fn deadline(&self, _ms: i64) -> i64 {
        0
    }
    /// How long this side may say nothing: a third of the `[ReceiveTimeout]` of the connection - the
    /// silence of the link the peer waits out -, in milliseconds; 0 - this side has nothing to say it
    /// with. `hears` - this side receives: it hears the `Pulse_` of its peer. A line silent for so
    /// long carries `Pulse_` ([`take_pulse`](Self::take_pulse)): the side that accepted the
    /// connection says it by itself, the side that dialed answers it
    /// ([`heard_pulse`](Self::heard_pulse)) - or says it by itself, when it hears nothing -, so the
    /// receive timeout of a side runs out only when its peer is gone. Default: nothing - the transport
    /// keeps no time.
    fn pulse(&self, _ms: i64, _hears: bool) {}
    /// The peer said `Pulse_`. The side that dialed answers it, unless it said something in the last
    /// half of [`pulse`](Self::pulse): the peer heard that. Default: nothing.
    fn heard_pulse(&self) {}
    /// The line has been silent ([`pulse`](Self::pulse)): the transmitter says `Pulse_` once its queue
    /// is empty - `true` once for each time the transport asks it, and the transport wakes the
    /// transmitter. Default: never.
    fn take_pulse(&self) -> bool {
        false
    }
    /// The wait of the stage ran out - the `[ReceiveTimeout]` of the state: no pack of the peer came
    /// in it. The application is asked as for the receive timeout of the transport: the milliseconds
    /// it still waits; 0 - the connection is closed as its receive timeout closes it: a loss, an
    /// identified session is parked. Default: [`lost`](Self::lost), 0.
    fn receive_timed_out(&self) -> i64 {
        self.lost();
        0
    }
    /// Sets the identity of the session on this connection: the token its peer hands back to
    /// continue it on another connection. By default `(-1 << 32) | id`: no slot of a registry -
    /// only a transport that reclaims makes a real one.
    fn identify(&self, id: u32) -> u64 {
        (u64::MAX << 32) | id as u64
    }
    /// The token of the session on this connection: its slot and its identity; -1 - this transport
    /// keeps no sessions.
    fn session(&self) -> u64 {
        u64::MAX
    }
    /// The peer hands back the token of a former connection: this connection takes it over and
    /// returns its own. By default the identity alone.
    fn reclaim(&self, session_id: u64) -> u64 {
        self.identify(session_id as u32)
    }
    /// [`reclaim`](Self::reclaim) of a token that came from the peer. A token is no secret: it holds
    /// the slot and the identity, and either can be guessed. So `id` is the identity the login of this
    /// connection has proved - by a key, a UUID, whatever the application trusts - and the session
    /// moves only when it is the identity the token names. Any other token leaves this connection
    /// with its own session, [identified](Self::identify) as `id`. Returns the token of this
    /// connection, for the peer's next login.
    fn reclaim_id(&self, session_id: u64, id: u32) -> u64 {
        if session_id as u32 == id {
            self.reclaim(session_id)
        } else {
            self.identify(id)
        }
    }
    /// The connection is lost, not closed by this side: a session that may park, parks. By default
    /// an abort.
    fn lost(&self) {
        self.abort()
    }
    /// The peer may go on sending. By default nothing to do.
    fn continue_(&self) {}
    /// The seconds the parked session still waits for its peer, 0 once its wait ran out, -1 - none
    /// parked.
    fn parked(&self) -> i32 {
        if self.is_parked() {
            0
        } else {
            -1
        }
    }
    /// The identity the session was given; 0 while it is not set: a guaranteed pack is refused
    /// before it.
    fn id(&self) -> u32 {
        0
    }
    /// The identity the session was given ([`identify`](Self::identify)): [`id`](Self::id), but for the end of
    /// a tunnel, whose `id` is the tunnel - the address of the far end.
    fn identity(&self) -> u32 {
        self.id()
    }
    /// The peer hands back the token of a former connection: its parked session, when it is still
    /// parked, for the protocol layer to move onto this transport. This connection takes the
    /// identity of the token.
    fn reclaim_session(&self, _session_id: u64) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
        None
    }
    /// The transport connection itself, when it is the TCP / WebSocket one: a session that moves
    /// here is seated on it.
    fn state(self: std::sync::Arc<Self>) -> Option<std::sync::Arc<ExternalConnection>> {
        None
    }
    /// What failed in the session of this connection, to the handler of its host. Default: nothing.
    fn failure(&self, _e: &(dyn std::error::Error + 'static)) {}
    /// This side made the connection: on a new socket it names itself first (its login), and names
    /// its position in the peer's journal only once the peer named its own
    /// ([`crate::lib::adhoc::connection::control`]). Default: no.
    fn dialing(&self) -> bool {
        false
    }
}

/// A connection of the TCP / WebSocket transport as the external one of a generated connection.
impl External for ExternalConnection {
    #[inline]
    fn on_its_thread(&self) -> bool {
        ExternalConnection::on_its_thread(self)
    }
    #[inline]
    fn session(&self) -> u64 {
        ExternalConnection::session(self)
    }
    #[inline]
    fn reclaim(&self, session_id: u64) -> u64 {
        ExternalConnection::reclaim(self, session_id as i64) as u64
    }
    /// The peer may go on sending: a UDP connection asks it from its receive position.
    #[inline]
    fn continue_(&self) {
        if let Some(u) = self.udp.get() {
            u.request(crate::lib::network::UDP::CONTINUE_);
        }
    }
    #[inline]
    fn lost(&self) {
        ExternalConnection::lost(self)
    }
    #[inline]
    fn dialing(&self) -> bool {
        self.dialing.load(std::sync::atomic::Ordering::Acquire)
    }
    #[inline]
    fn parked(&self) -> i32 {
        ExternalConnection::parked(self) as i32
    }
    #[inline]
    fn is_open(&self) -> bool {
        ExternalConnection::is_open(self)
    }
    #[inline]
    fn wake_transmit(&self) {
        ExternalConnection::wake_transmit(self)
    }
    #[inline]
    fn wake_receive(&self) {
        ExternalConnection::wake_receive(self)
    }
    #[inline]
    fn deadline(&self, ms: i64) -> i64 {
        ExternalConnection::deadline(self, ms)
    }
    #[inline]
    fn pulse(&self, ms: i64, hears: bool) {
        ExternalConnection::pulse(self, ms, hears)
    }
    #[inline]
    fn heard_pulse(&self) {
        ExternalConnection::heard_pulse(self)
    }
    #[inline]
    fn take_pulse(&self) -> bool {
        ExternalConnection::take_pulse(self)
    }
    #[inline]
    fn receive_timed_out(&self) -> i64 {
        ExternalConnection::receive_timed_out(self)
    }
    #[inline]
    fn identify(&self, id: u32) -> u64 {
        ExternalConnection::identify(self, id)
    }
    #[inline]
    fn id(&self) -> u32 {
        ExternalConnection::session(self) as u32
    }
    #[inline]
    fn reclaim_session(&self, session_id: u64) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
        ExternalConnection::reclaim_session(self, session_id)
    }
    #[inline]
    fn state(self: std::sync::Arc<Self>) -> Option<std::sync::Arc<ExternalConnection>> {
        Some(self)
    }
    #[inline]
    fn receive_timeout(&self) -> i32 {
        self.receive_timeout_ms()
    }
    /// Negative: a graceful close, at once when nothing is left to send.
    #[inline]
    fn set_receive_timeout(&self, ms: i32) {
        self.set_receive_timeout_ms(ms);
        if ms < 0 {
            ExternalConnection::wake_transmit(self); // the transmit finds the queue drained: the FIN goes
        }
    }
    #[inline]
    fn transmit_timeout(&self) -> i32 {
        self.transmit_timeout_ms()
    }
    /// Negative: a graceful close once what is queued is out - at once when nothing is.
    #[inline]
    fn set_transmit_timeout(&self, ms: i32) {
        self.set_transmit_timeout_ms(ms);
        if ms < 0 {
            ExternalConnection::wake_transmit(self); // the transmit finds the queue drained: the FIN goes
        }
    }
    #[inline]
    fn abort(&self) {
        ExternalConnection::abort(self)
    }
    #[inline]
    fn is_parked(&self) -> bool {
        0 <= ExternalConnection::parked(self)
    }
    #[inline]
    fn close(&self) {
        ExternalConnection::close(self)
    }
    #[inline]
    fn failure(&self, e: &(dyn std::error::Error + 'static)) {
        ExternalConnection::failure(self, e)
    }
}
