// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Select a protocol connection on a shared listening port.
//!
//! The dialing side sends its host identifier as the first byte; the listening side selects the
//! corresponding connection factory. The byte goes in the clear: the Noise of the socket is of the
//! connection it names, declared as that one is seated ( `[Noise]` of its description ), so the
//! handshake and the records come right behind the byte. TLS, chosen by the host, is under both.

use std::sync::Arc;

use parking_lot::Mutex;

use super::TCP::external_connection::Event;
use crate::lib::adhoc::{BytesDst, BytesSrc};
use super::TCP::external_connection::{ExternalConnection, Incumbent};
use super::TCP::ConnectionFactory;

/// The listening side: the factory of the connection a caller names by the uid of its host; `None`
/// — an unknown one: the socket closes.
pub type MuxPick = Arc<dyn Fn(u8) -> Option<ConnectionFactory> + Send + Sync>;

/// The listening side of a multiplexed port, as a [`ConnectionFactory`] of the server: every
/// accepted socket gets a stand-in ([`MuxSink`] / [`MuxSource`]) until its first byte names the
/// connection, then the connection `pick` gives. `connect` — the event the seated connection hears:
/// `Event::REMOTE_CONNECT` on TCP, `Event::WEBSOCKET_REMOTE_CONNECT` on WebSocket (the server fired
/// its own before anyone was seated).
pub fn factory(pick: MuxPick, connect: Event) -> ConnectionFactory {
    Arc::new(move |state: &Arc<ExternalConnection>| {
        let source = Arc::new(Mutex::new(None));
        let sink = MuxSink {
            state: state.clone(),
            pick: pick.clone(),
            connect,
            seated: None,
            uid: None,
            source: source.clone(),
            refused: false,
        };
        (Box::new(sink) as Box<dyn BytesDst>, Box::new(MuxSource { source, own: None }) as Box<dyn BytesSrc>)
    })
}

/// Stands in for the connection of a socket of a multiplexed port until the caller names it: the
/// first byte picks the connection, the rest goes to it.
pub struct MuxSink {
    state: Arc<ExternalConnection>,
    pick: MuxPick,
    connect: Event,
    seated: Option<Box<dyn BytesDst>>,
    /// The byte the caller named the seated connection with.
    uid: Option<u8>,
    source: Arc<Mutex<Option<Box<dyn BytesSrc>>>>,
    /// The caller named no connection of the port: the stream is broken for good, the socket is
    /// ours to close.
    refused: bool,
}

impl MuxSink {
    /// The connection the byte `uid` names, seated on the transport of the socket, in the Noise it
    /// declares: false — none, or it refused the socket.
    fn seat(&mut self, uid: u8) -> bool {
        // the connection this slot served last, when it is the one named, is taken back as it is
        let kept = self.state.incumbent.lock().take();
        let (kept, again) = match kept {
            Some(k) if k.uid == uid => (None, (k.seat)(&self.state)), // `k.keep` goes: what is seated holds the connection now
            k => (k, None),
        };
        let (sink, source) = match again {
            Some(s) => s,
            None => {
                let Some(factory) = (self.pick)(uid) else {
                    *self.state.incumbent.lock() = kept; // an unknown caller: the slot keeps what it had
                    self.state.failure(&super::failure::protocol(format!("A multiplexed port has no connection for the host uid {uid}")));
                    self.refused = true;
                    return false;
                };
                drop(kept); // another connection named: the one kept goes back to the application
                factory(&self.state)
            }
        };
        // the bytes behind this one are of the Noise the connection seated declares: this end answers
        let Some((sink, source)) = super::TCP::noised(&self.state, false, sink, source) else {
            self.state.failure(&super::TCP::refused());
            self.refused = true;
            return false;
        };
        self.uid = Some(uid);
        *self.source.lock() = Some(source);
        self.seated = Some(sink);
        // the connection seated hears what the socket heard before it, then OnOpen
        if self.connect != Event::REMOTE_CONNECT {
            self.state.notify(Event::REMOTE_CONNECT);
        }
        self.state.fire(self.connect);
        self.state.wake_transmit(); // what the connection has to send goes now
        true
    }
}

/// The socket is over: the connection it served stays with the slot for the next caller that names
/// it - unless its session parks, which holds it itself, or its protocol layer did not say how to
/// hold it and seat it again.
impl Drop for MuxSink {
    fn drop(&mut self) {
        let Some(uid) = self.uid else { return };
        if self.seated.is_none() || self.state.resumable.load(std::sync::atomic::Ordering::Acquire) != 0 {
            return;
        }
        let (Some(keeper), Some(seat)) = (self.state.session_keeper(), self.state.seat_hook()) else { return };
        let Some(keep) = keeper() else { return }; // the connection is still alive: what is seated holds it yet
        *self.state.incumbent.lock() = Some(Incumbent { uid, keep, seat });
    }
}

impl BytesDst for MuxSink {
    fn on_bytes(&mut self, bytes: &[u8]) -> bool {
        if let Some(s) = &mut self.seated {
            return s.on_bytes(bytes);
        }
        if bytes.is_empty() {
            return true;
        }
        if !self.seat(bytes[0]) {
            return false;
        }
        bytes.len() == 1 || self.seated.as_mut().unwrap().on_bytes(&bytes[1..])
    }

    fn on_bytes_mut(&mut self, bytes: &mut [u8]) -> bool {
        if let Some(s) = &mut self.seated {
            return s.on_bytes_mut(bytes);
        }
        if bytes.is_empty() {
            return true;
        }
        if !self.seat(bytes[0]) {
            return false;
        }
        bytes.len() == 1 || self.seated.as_mut().unwrap().on_bytes_mut(&mut bytes[1..])
    }

    fn on_block(&mut self, block: &super::buf::RxBuf, at: usize, end: usize) -> Option<usize> {
        if let Some(s) = &mut self.seated {
            return s.on_block(block, at, end);
        }
        if at == end {
            return Some(0);
        }
        // SAFETY: the connection's own byte
        if !self.seat(unsafe { block.slice(at, at + 1) }[0]) {
            return None;
        }
        if at + 1 == end {
            return Some(1);
        }
        Some(1 + self.seated.as_mut().unwrap().on_block(block, at + 1, end)?)
    }

    fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
        if let Some(s) = &mut self.seated {
            return s.on_part(bytes);
        }
        if bytes.is_empty() {
            return Some(0);
        }
        if !self.seat(bytes[0]) {
            return None;
        }
        if bytes.len() == 1 {
            return Some(1);
        }
        Some(1 + self.seated.as_mut().unwrap().on_part(&bytes[1..])?)
    }

    fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
        if let Some(s) = &mut self.seated {
            return s.on_slice(bytes);
        }
        if bytes.is_empty() {
            return Some(0);
        }
        if !self.seat(bytes[0]) {
            return None;
        }
        if bytes.len() == 1 {
            return Some(1);
        }
        Some(1 + self.seated.as_mut().unwrap().on_slice(&mut bytes[1..])?)
    }

    fn graceful_close(&self) -> bool {
        self.seated.as_ref().is_some_and(|s| s.graceful_close())
    }

    fn broken(&self) -> bool {
        self.refused || self.seated.as_ref().is_some_and(|s| s.broken())
    }
    #[inline]
    fn lost(&mut self, position: i64) {
        if let Some(s) = &mut self.seated {
            s.lost(position)
        }
    }
    #[inline]
    fn abandon(&mut self) {
        if let Some(s) = &mut self.seated {
            s.abandon()
        }
    }
    #[inline]
    fn continue_(&mut self) {
        if let Some(s) = &mut self.seated {
            s.continue_()
        }
    }
    #[inline]
    fn received(&self) -> i64 {
        self.seated.as_ref().map_or(-1, |s| s.received())
    }
    #[inline]
    fn idle(&self) -> bool {
        self.seated.as_ref().is_none_or(|s| s.idle())
    }

    fn wants_more(&self) -> bool {
        self.seated.as_ref().is_none_or(|s| s.wants_more())
    }

    /// The seated sink offers the tail it holds again (a relay whose outbound side is behind).
    fn retry(&mut self) {
        if let Some(s) = self.seated.as_mut() {
            s.retry();
        }
    }

    fn suspend(&mut self) {
        if let Some(s) = self.seated.as_mut() {
            s.suspend();
        }
    }
}

/// The source of the stand-in: nothing before the caller is named, then the seated connection's.
pub struct MuxSource {
    source: Arc<Mutex<Option<Box<dyn BytesSrc>>>>,
    /// The seated source, taken out of the shared place once: no lock per read after that.
    own: Option<Box<dyn BytesSrc>>,
}

impl BytesSrc for MuxSource {
    fn read(&mut self, dst: &mut [u8]) -> isize {
        if self.own.is_none() {
            match self.source.lock().take() {
                Some(s) => self.own = Some(s),
                None => return -1,
            }
        }
        self.own.as_mut().unwrap().read(dst)
    }

    #[inline]
    fn journal(&self) -> Option<&crate::lib::adhoc::connection::transmitter::resumable::Journal> {
        self.own.as_ref()?.journal()
    }
    #[inline]
    fn holds_journal(&mut self) -> bool {
        self.own.as_mut().map_or(true, |s| s.holds_journal())
    }
    #[inline]
    fn with_journal(&mut self, f: &mut dyn FnMut(&mut crate::lib::adhoc::connection::transmitter::resumable::Resumable)) -> bool {
        self.own.as_mut().map_or(false, |s| s.with_journal(f))
    }
    #[inline]
    fn online(&self) {
        if let Some(s) = self.own.as_ref() {
            s.online()
        }
    }
    #[inline]
    fn turn(&mut self, position: i64) -> std::io::Result<i64> {
        self.own.as_mut().map_or(Ok(-1), |s| s.turn(position))
    }
    #[inline]
    fn abandon(&mut self) {
        if let Some(s) = self.own.as_mut() {
            s.abandon()
        }
    }
}

/// The dialing side: the protocol layer `seat` gives, with the byte `uid` naming the connection put
/// before everything it sends. The write half is woken at once: the byte goes even when the
/// connection has nothing to send yet. The Noise the protocol layer declares goes inside, behind
/// the byte; a refusal is left for the transport, which does not dial then.
pub fn dialing(
    uid: u8,
    seat: impl Fn(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>),
) -> impl Fn(&Arc<ExternalConnection>) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) {
    move |state| {
        let (sink, source) = noised_inside(state, seat(state));
        named_reseat(state, uid);
        (sink, Box::new(MuxNamed { uid: Some(uid), inner: source }) as Box<dyn BytesSrc>)
    }
}

/// The Noise the protocol layer seated on `state` declared, around `seated`: this end starts the
/// handshake.
fn noised_inside(state: &Arc<ExternalConnection>, seated: (Box<dyn BytesDst>, Box<dyn BytesSrc>)) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>) {
    match state.take_secured_chain() {
        Some(c) if 0 <= c.pattern => super::TCP::noise(state, &c, true, seated.0, seated.1),
        _ => seated,
    }
}

/// A reconnect of the session seated on `state` opens with the byte naming the connection too: the
/// seat hook the protocol layer set is put inside one that names the source it gives on the new
/// connection - and so the hook it sets there.
fn named_reseat(state: &ExternalConnection, uid: u8) {
    let Some(seat) = state.seat_hook() else { return };
    state.set_seat_hook(Arc::new(move |to| {
        let (sink, source) = noised_inside(to, seat(to)?);
        named_reseat(to, uid);
        Some((sink, Box::new(MuxNamed { uid: Some(uid), inner: source }) as Box<dyn BytesSrc>))
    }));
}

/// The source of a dialing socket: the byte naming the connection first, then the connection's
/// bytes.
pub struct MuxNamed {
    uid: Option<u8>,
    inner: Box<dyn BytesSrc>,
}

impl BytesSrc for MuxNamed {
    #[inline]
    fn journal(&self) -> Option<&crate::lib::adhoc::connection::transmitter::resumable::Journal> {
        self.inner.journal()
    }
    #[inline]
    fn holds_journal(&mut self) -> bool {
        self.inner.holds_journal()
    }
    #[inline]
    fn with_journal(&mut self, f: &mut dyn FnMut(&mut crate::lib::adhoc::connection::transmitter::resumable::Resumable)) -> bool {
        self.inner.with_journal(f)
    }
    #[inline]
    fn online(&self) {
        self.inner.online()
    }
    #[inline]
    fn turn(&mut self, position: i64) -> std::io::Result<i64> {
        self.inner.turn(position)
    }
    #[inline]
    fn abandon(&mut self) {
        self.inner.abandon()
    }

    fn read(&mut self, dst: &mut [u8]) -> isize {
        let Some(uid) = self.uid.take() else { return self.inner.read(dst) };
        dst[0] = uid;
        1 + self.inner.read(&mut dst[1..]).max(0)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::network::TCP::external_connection::ConnId;

    struct Collect(Arc<Mutex<Vec<u8>>>);

    impl BytesDst for Collect {
        fn on_bytes(&mut self, bytes: &[u8]) -> bool {
            self.0.lock().extend_from_slice(bytes);
            true
        }
    }

    struct Nothing;

    impl BytesSrc for Nothing {
        fn read(&mut self, _dst: &mut [u8]) -> isize {
            -1
        }
    }

    #[test]
    fn the_first_byte_names_the_connection() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let seated: ConnectionFactory = Arc::new(move |_| {
            (Box::new(Collect(g.clone())) as Box<dyn BytesDst>, Box::new(Nothing) as Box<dyn BytesSrc>)
        });
        let pick: MuxPick = Arc::new(move |uid| (uid == 7).then(|| seated.clone()));
        let state = ExternalConnection::new(ConnId(1));
        let (mut sink, _) = factory(pick.clone(), Event::REMOTE_CONNECT)(&state);
        assert!(sink.on_bytes(&[7, 1, 2]));
        assert!(sink.on_bytes(&[3]));
        assert_eq!(*got.lock(), vec![1, 2, 3]);

        let unknown = ExternalConnection::new(ConnId(2));
        let failures = Arc::new(Mutex::new(Vec::<String>::new()));
        let fl = failures.clone();
        unknown.set_on_failure(Arc::new(move |_, e| fl.lock().push(e.to_string())));
        let (mut other, _) = factory(pick, Event::REMOTE_CONNECT)(&unknown);
        assert!(!other.on_bytes(&[9, 1]), "an unknown uid closes the socket");
        assert!(other.broken(), "broken for good: the socket is ours to close, the session does not park ( Java abort )");
        assert!(!sink.broken());
        assert_eq!(*failures.lock(), ["A multiplexed port has no connection for the host uid 9"], "and goes to the handler");
    }

    #[test]
    fn the_dialing_socket_opens_with_the_byte() {
        let state = ExternalConnection::new(ConnId(3));
        let (_, mut source) = dialing(5, |_| {
            (Box::new(Collect(Arc::new(Mutex::new(Vec::new())))) as Box<dyn BytesDst>, Box::new(Nothing) as Box<dyn BytesSrc>)
        })(&state);
        let mut out = Vec::new();
        source.try_drain(&mut out);
        source.try_drain(&mut out);
        assert_eq!(out, vec![5]);
    }
}
