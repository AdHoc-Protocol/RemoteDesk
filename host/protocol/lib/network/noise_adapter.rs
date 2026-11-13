// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Adapters between transport byte traits and encrypted Noise records.
//!
//! [`SourceBridge`] maps a [`BytesSrc`] to [`ByteSource`], including temporary lack of output as
//! [`Pull::Pending`]. [`SinkBridge`] maps [`BytesDst`] to [`ByteSink`]. [`NoiseSource`] emits
//! records and [`NoiseSink`] opens received records, retaining partial-consumption state across
//! retries.
//!
//! [`wrap_chain`] shares one [`NoiseStream`] between directions, on the loop of the connection: the Noise the protocol layer
//! declared as it was seated ( [`ExternalConnection::secure`] ). Handshake, authentication, and record-order errors terminate the
//! connection.
//!
//! [`ExternalConnection::secure`]: super::TCP::external_connection::ExternalConnection::secure

use std::io;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use crate::lib::adhoc::{BytesDst, BytesSrc};
use crate::lib::noise::{ByteSink, ByteSource, NoiseStream, Pull, KEY};

// Bridges between transport byte traits and Noise source and sink traits.

/// Maps positive [`BytesSrc::read`] counts to [`Pull::Bytes`], zero to [`Pull::Pending`], and
/// negative counts to [`Pull::Eof`].
struct SourceBridge {
    inner: Box<dyn BytesSrc>,
}

impl SourceBridge {
    fn new(inner: Box<dyn BytesSrc>) -> Self {
        Self { inner }
    }
}

impl ByteSource for SourceBridge {
    #[inline]
    fn pull(&mut self, buf: &mut [u8]) -> io::Result<Pull> {
        (*self.inner).pull(buf)
    }

    #[inline]
    fn clear(&self) -> bool {
        self.inner.clear()
    }
}

/// Any byte source under a Noise stream - the journal of a tunnel too
/// ([`Out::get_bytes_src`](crate::lib::adhoc::connection::transmitter::tunnel::Out::get_bytes_src)).
impl<'a> ByteSource for dyn BytesSrc + 'a {
    /// The plaintext is serialized straight into the record's room.
    fn pull(&mut self, buf: &mut [u8]) -> io::Result<Pull> {
        let n = self.read(buf);
        // 0 - a pack waits; -1 - nothing to send now: the stream is drained, a graceful close goes
        // on (FIN, CLOSE frame) - as "a pack waits" it would hold the transport and the close would
        // never come
        Ok(if 0 < n {
            Pull::Bytes(n as usize)
        } else if n == 0 {
            Pull::Pending
        } else {
            Pull::Eof
        })
    }

    #[inline]
    fn clear(&self) -> bool {
        BytesSrc::clear(self)
    }
}

/// Adapts [`BytesDst::on_part`] to [`ByteSink::write`], preserving partial consumption. A `None`
/// result becomes an invalid-data error.
struct SinkBridge(Box<dyn BytesDst>);

impl ByteSink for SinkBridge {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (*self.0).write(bytes)
    }

    #[inline]
    fn write_clear(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (*self.0).write_clear(bytes)
    }

    fn broken(&self) -> bool {
        self.0.broken()
    }

    fn suspend(&mut self) {
        self.0.suspend()
    }
}

/// Any protocol sink under a Noise stream - the receiver of a tunnel's virtual connection too
/// ([`EndPoint::on_part`](crate::lib::adhoc::connection::tunnel::end_point::EndPoint::on_part)).
impl<'a> ByteSink for dyn BytesDst + 'a {
    /// What the protocol sink takes: a refused rest is the record layer's to offer again.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.on_part(bytes).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the protocol sink broke the stream"))
    }

    /// The bytes of a record in the clear: the protocol sink takes apart what may be there.
    fn write_clear(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.on_clear(bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a record in the clear carried what may not be there"))
    }

    fn broken(&self) -> bool {
        BytesDst::broken(self)
    }

    fn suspend(&mut self) {
        BytesDst::suspend(self)
    }
}

// The shared stream

/// The [`NoiseStream`] of a connection. Its receive, its transmit and its chain all run on the loop of the connection, one after
/// the other - never one inside another: the stream is touched there with no lock, and a sink that sends meets no wait. Other
/// threads read only what is published here.
pub(crate) struct Shared {
    ns: std::cell::UnsafeCell<NoiseStream>,
    ready: AtomicBool,
    failed: AtomicBool,
    forged: AtomicI64,
}

// SAFETY: the stream moves between threads only before its connection is seated; it is touched only on the loop of the
// connection ( `stream` ); the rest is atomic
unsafe impl Sync for Shared {}

impl Shared {
    fn new(ns: NoiseStream) -> Arc<Self> {
        Arc::new(Self {
            ns: std::cell::UnsafeCell::new(ns),
            ready: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            forged: AtomicI64::new(0),
        })
    }

    /// The stream.
    ///
    /// # Safety
    ///
    /// On the loop of the connection, or where nothing else runs it ( before it is seated, after it is gone ); the reference is
    /// let go before anything else of the connection runs there.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn stream(&self) -> &mut NoiseStream {
        &mut *self.ns.get()
    }

    /// What the last offer changed, for the other threads. On the loop of the connection.
    #[inline(always)]
    pub(crate) fn publish(&self, ns: &NoiseStream) {
        if ns.failed() {
            if !self.failed.load(Ordering::Relaxed) {
                self.forged.store(ns.forged, Ordering::Relaxed);
                self.failed.store(true, Ordering::Release);
            }
        } else if !self.ready.load(Ordering::Relaxed) && ns.ready() {
            self.ready.store(true, Ordering::Release);
        }
    }
}

// NoiseHandle — observability for the shared stream

/// A handle to the shared [`NoiseStream`] of a wrapped connection: handshake progress, failure,
/// forgery counter, the key of the peer.
#[derive(Clone)]
pub struct NoiseHandle(pub(crate) Arc<Shared>, Arc<std::sync::OnceLock<[u8; KEY]>>);

impl NoiseHandle {
    /// The handle of `ns`, a stream nothing runs yet.
    pub(crate) fn of(ns: NoiseStream) -> Self {
        let peer = ns.peer.clone();
        Self(Shared::new(ns), peer)
    }
    /// Both directions have their keys.
    pub fn ready(&self) -> bool {
        self.0.ready.load(Ordering::Acquire)
    }
    /// The stream broke (a forged record, a failed handshake).
    pub fn failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    /// Records that did not open: the stream ended at the first.
    pub fn forged(&self) -> i64 {
        if self.failed() {
            self.0.forged.load(Ordering::Relaxed)
        } else {
            0
        }
    }
    /// The key of the other side, proved by the handshake: for a pattern that brings it.
    pub fn peer_key(&self) -> Option<[u8; KEY]> {
        self.1.get().copied()
    }
}

// Pump adapters

/// [`BytesSrc`] over a shared [`NoiseStream`]: the handshake
/// message first, then records of what the inner source gives.
pub struct NoiseSource {
    ns: Arc<Shared>,
    /// The bridge itself, not boxed: one call less a read, one allocation less a connection.
    inner: SourceBridge,
}

impl BytesSrc for NoiseSource {
    /// The records are sealed straight into the transport's buffer.
    fn read(&mut self, dst: &mut [u8]) -> isize {
        // SAFETY: the transmit of the connection, on its loop
        let ns = unsafe { self.ns.stream() };
        let r = ns.pull_from(&mut self.inner, dst);
        self.ns.publish(ns);
        match r {
            Ok(Pull::Bytes(n)) => n as isize,
            // Nothing now: the stream's `wake` hook wakes the transmit when the handshake advances.
            Ok(Pull::Pending) => 0,
            Ok(Pull::Eof) | Err(_) => -1,
        }
    }
}

/// [`BytesDst`] over a shared [`NoiseStream`]: opens inbound
/// records where the transport read them and hands the plaintext to the inner sink. What it does
/// not take stays with the transport: no tail of its own.
pub struct NoiseSink {
    ns: Arc<Shared>,
    /// The bridge itself, not boxed: one call less a record, one allocation less a connection.
    inner: SinkBridge,
}

impl BytesDst for NoiseSink {
    fn on_bytes(&mut self, bytes: &[u8]) -> bool {
        self.on_part(bytes) == Some(bytes.len())
    }

    fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
        // SAFETY: the receive of the connection, on its loop
        let ns = unsafe { self.ns.stream() };
        let r = ns.push_slice(&mut self.inner, bytes).ok();
        self.ns.publish(ns);
        r
    }

    fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
        // SAFETY: the receive of the connection, on its loop
        let ns = unsafe { self.ns.stream() };
        let r = ns.push_to(&mut self.inner, bytes).ok();
        self.ns.publish(ns);
        r
    }

    /// A forged record broke the stream for good, as a refusal of the protocol sink: the connection
    /// is aborted, its session does not park.
    fn broken(&self) -> bool {
        self.inner.broken() || self.ns.failed.load(Ordering::Relaxed)
    }

    /// The records of this socket go with it; the protocol sink under them keeps what survives a
    /// reconnect.
    fn suspend(&mut self) {
        self.inner.suspend()
    }
}

// The seat

/// Seat of a chain with a Noise pattern ([`Chain`](crate::lib::chain::Chain)): `initiator` writes the first message. `wake`
/// wakes the transmit of the connection: the handshake calls it when its next message (or more records) can go out.
pub(crate) fn wrap_chain(
    chain: &crate::lib::chain::Chain,
    initiator: bool,
    sink: Box<dyn BytesDst>,
    source: Box<dyn BytesSrc>,
    wake: Box<dyn FnMut() + Send>,
) -> (Box<dyn BytesDst>, Box<dyn BytesSrc>, NoiseHandle) {
    let handle = NoiseHandle::of(chain.noise_stream(initiator, Some(wake)));
    let ns = handle.0.clone();
    (
        Box::new(NoiseSink {
            ns: ns.clone(),
            inner: SinkBridge(sink),
        }),
        Box::new(NoiseSource { ns, inner: SourceBridge::new(source) }),
        handle,
    )
}
