// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Receiving side of a tunnel carried inside protocol packets.
//!
//! [`In`] reads a frame from the carrier receiver and supplies its chunk payloads to the virtual
//! connection as a continuous byte stream.


use crate::lib::adhoc::connection::transmitter::chunked::ABORT;
use crate::lib::adhoc::connection::receiver::chunked::{push, RxChain};
use crate::lib::adhoc::chain::Stage;
use crate::lib::adhoc::connection::receiver::Rx;
use crate::lib::adhoc::connection::transmitter::TxHost;
use crate::lib::stages::Keys;

/// The frames of the tunnels of one kind arriving on a carrier: one frame at a time, its target the
/// virtual connection of its `tunnel_id`.
pub struct In<T> {
    tid: u32,
    tid_got: usize,
    routed: bool,
    /// The `tunnel_id` is read, the frame is not routed yet: that waits for its first length.
    fresh: bool,
    len: u32,
    len_got: u8,
    rem: u32,
    target: Option<T>,
    /// The stages of the chain of the tunnel ([`put_bytes_chain`](Self::put_bytes_chain)): made
    /// once, armed every frame.
    chain: RxChain,
    /// What the stages gave and the virtual connection refused (a relay of it is behind): it goes
    /// first, no byte of the carrier is taken before - the stages cannot be rewound. Empty, and
    /// with no memory, until a refusal.
    pending: Vec<u8>,
    /// The terminator of the frame is read but its last byte, left with the carrier's transport:
    /// `pending` is all that is left of the frame.
    ending: bool,
}

impl<T> Default for In<T> {
    fn default() -> Self {
        Self { tid: 0, tid_got: 0, routed: false, fresh: false, len: 0, len_got: 0, rem: 0, target: None, chain: RxChain::default(), pending: Vec::new(), ending: false }
    }
}

/// The id a receiver read, `rx_id_bytes` wide, of a frame whose pack id is `id_bytes` wide and
/// whose `tunnel_id` is `tid_bytes`: (pack id, the bytes of the `tunnel_id` the receiver read with
/// it, how many). A receiver of a connection of actors reads a 2-byte id - the pack id and, for a
/// tunnel frame, the first byte of its `tunnel_id` (the frame's id is one byte).
#[inline]
pub fn split_id(id: u32, id_bytes: usize, tid_bytes: usize, rx_id_bytes: usize) -> (u32, u32, usize) {
    // the widths are the generated code's constants: inlined, the check folds away
    assert!(id_bytes <= rx_id_bytes && rx_id_bytes <= 4 && tid_bytes <= 4, "the receiver reads the whole pack id: {id_bytes} of {rx_id_bytes}, tunnel_id {tid_bytes}");
    let taken = rx_id_bytes.saturating_sub(id_bytes).min(tid_bytes);
    let mask = |n: usize| if 4 <= n { u32::MAX } else { (1u32 << (8 * n)) - 1 };
    (id & mask(id_bytes), (id >> (8 * id_bytes)) & mask(taken), taken)
}

impl<T> In<T> {
    /// The frame being received will not be completed - the carrier's receiver dropped it: the next
    /// frame starts clean. Returns the virtual connection it was feeding: what that one received
    /// half goes too.
    pub fn abandon(&mut self) -> Option<T> {
        self.routed = false;
        self.fresh = false;
        self.tid = 0;
        self.tid_got = 0;
        self.rem = 0;
        self.len = 0;
        self.len_got = 0;
        self.pending.clear();
        self.ending = false;
        self.target.take()
    }

    /// A frame arrives: `id` is what the carrier's receiver read, `rx_id_bytes` wide - the bytes of
    /// the `tunnel_id` it read with the pack id are taken from it ([`split_id`]), the rest comes
    /// from the wire.
    #[inline]
    pub fn arm(&mut self, id: u32, id_bytes: usize, rx_id_bytes: usize, tid_bytes: usize) {
        if self.routed {
            return; // the frame in progress goes on
        }
        // Tunnel dispatch must begin before body bytes are consumed. Relay dispatch instead uses
        // `Relay::arm` to preserve bytes already read with the identifier.
        assert!(rx_id_bytes <= id_bytes + tid_bytes, "Tunnel arming mismatch: the receiver reads {rx_id_bytes} bytes of id, the pack id and the tunnel_id are {id_bytes} + {tid_bytes}: bytes of the body would be lost");
        let (_, tid, taken) = split_id(id, id_bytes, tid_bytes, rx_id_bytes);
        self.tid = tid;
        self.tid_got = taken;
    }

    /// The `tunnel_id` of the frame is read already - by the relay of this host that found the
    /// tunnel ends here ([`Relay::here`](crate::lib::adhoc::connection::tunnel::relay::Relay::here)):
    /// its chunks come next. In place of [`arm`](Self::arm).
    #[inline]
    pub fn routed(&mut self, tunnel_id: u32, tid_bytes: usize) {
        self.tid = tunnel_id;
        self.tid_got = tid_bytes;
    }

    /// The body of a frame from the window of the carrier's receiver, its `tunnel_id` `tid_bytes`
    /// wide. `route(tunnel_id, make)` — the virtual connection of the tunnel, at the first length
    /// of the frame: `make` — made when there is none; not — a frame that is only `ABORT` ends a
    /// tunnel that is there and makes none. `None` drops the frame. `feed` gives it the bytes of a
    /// chunk; `ended(target, tunnel_id)` — the frame ended with `ABORT`: the tunnel ended on the
    /// far side. `true` — the frame is through; `false` — the window ran out.
    pub fn put_bytes(
        &mut self,
        src: &mut Rx<'_>,
        tid_bytes: usize,
        route: impl FnOnce(u32, bool) -> Option<T>,
        mut feed: impl FnMut(&T, &[u8]) -> usize,
        ended: impl FnOnce(T, u32),
    ) -> bool {
        if !self.routed {
            while self.tid_got < tid_bytes {
                if src.byte_max <= src.byte_ {
                    return false;
                }
                self.tid |= (src.buffer[src.byte_] as u32) << (8 * self.tid_got);
                src.byte_ += 1;
                self.tid_got += 1;
            }
            self.routed = true;
            self.fresh = true;
            self.rem = 0;
            self.len = 0;
            self.len_got = 0;
        }
        let mut route = Some(route);
        loop {
            if self.rem == 0 {
                while self.len_got < 2 {
                    if src.byte_max <= src.byte_ {
                        return false;
                    }
                    self.len |= (src.buffer[src.byte_] as u32) << (8 * self.len_got);
                    src.byte_ += 1;
                    self.len_got += 1;
                }
                let len = self.len;
                self.len = 0;
                self.len_got = 0;
                if self.fresh {
                    self.fresh = false;
                    self.target = route.take().and_then(|r| r(self.tid, len != ABORT));
                }
                if len == 0 || len == ABORT {
                    let tid = self.tid;
                    self.routed = false;
                    self.tid = 0;
                    self.tid_got = 0;
                    let t = self.target.take();
                    if len == ABORT {
                        if let Some(t) = t {
                            ended(t, tid);
                        }
                    }
                    return true;
                }
                self.rem = len;
            }
            let avail = src.byte_max - src.byte_;
            if avail == 0 {
                return false;
            }
            let n = avail.min(self.rem as usize);
            let took = match &self.target {
                Some(t) => feed(t, &src.buffer[src.byte_..src.byte_ + n]),
                None => n,
            };
            src.byte_ += took;
            self.rem -= took as u32;
            if took < n {
                return false; // refused (a relay of the virtual connection is behind): the rest stays with the carrier's
                // transport
            }
        }
    }

    /// Receives a tunnel burst through the inverse of the stages in `spec`, then sends decoded
    /// bytes to `feed`. Complements
    /// [`Out::get_bytes_chain`](crate::lib::adhoc::connection::transmitter::tunnel::Out::get_bytes_chain).
    /// `keys` — the keys of the chain of the tunnel the frame is routed to: the stages are keyed by
    /// them, never by the carrier's ( `src` ).
    pub fn put_bytes_chain(
        &mut self,
        src: &mut Rx<'_>,
        tid_bytes: usize,
        spec: &'static [Stage],
        route: impl FnOnce(u32, bool) -> Option<T>,
        keys: impl FnOnce(&T) -> Keys,
        mut feed: impl FnMut(&T, &[u8]) -> usize,
        ended: impl FnOnce(T, u32),
    ) -> bool {
        if !self.routed {
            while self.tid_got < tid_bytes {
                if src.byte_max <= src.byte_ {
                    return false;
                }
                self.tid |= (src.buffer[src.byte_] as u32) << (8 * self.tid_got);
                src.byte_ += 1;
                self.tid_got += 1;
            }
            self.routed = true;
            self.fresh = true;
            self.rem = 0;
            self.len = 0;
            self.len_got = 0;
            self.chain.arm(spec);
        }
        if !self.pending.is_empty() {
            // what the virtual connection refused goes first (the carrier's receive was woken: it
            // has room again)
            let took = match &self.target {
                Some(t) => feed(t, &self.pending),
                None => self.pending.len(),
            };
            self.pending.drain(..took);
            if !self.pending.is_empty() {
                return false;
            }
            if self.ending {
                if src.byte_max <= src.byte_ {
                    return false; // the last byte of the terminator is offered again with the wake
                }
                src.byte_ += 1; // that byte: the frame ends with it
                self.ending = false;
                self.end_frame();
                return true;
            }
        }
        let mut route = Some(route);
        let mut keys = Some(keys);
        let (target, chain, fresh, tid, pending) = (&mut self.target, &mut self.chain, &mut self.fresh, self.tid, &mut self.pending);
        // the leaf: the bytes the stages give, to the virtual connection of the frame; the stages
        // are not rewound - what it refuses is kept, and once anything is kept the rest of what the
        // stages give is kept behind it
        let mut leaf = |target: &Option<T>, pending: &mut Vec<u8>, r: &mut Rx<'_>| {
            if let Some(t) = target.as_ref() {
                let b = &r.buffer[r.byte_..r.byte_max];
                if pending.is_empty() {
                    let took = feed(t, b);
                    if took < b.len() {
                        pending.extend_from_slice(&b[took..]);
                    }
                } else {
                    pending.extend_from_slice(b);
                }
            }
            r.byte_ = r.byte_max;
        };
        loop {
            if self.rem == 0 {
                while self.len_got < 2 {
                    if src.byte_max <= src.byte_ {
                        return false;
                    }
                    self.len |= (src.buffer[src.byte_] as u32) << (8 * self.len_got);
                    src.byte_ += 1;
                    self.len_got += 1;
                }
                let len = self.len;
                self.len = 0;
                self.len_got = 0;
                if *fresh {
                    *fresh = false;
                    *target = route.take().and_then(|r| r(tid, len != ABORT));
                    if let Some(k) = keys.take() {
                        // the tunnel's: the stages look at nothing of the carrier; a frame of no tunnel - none
                        chain.keys(&target.as_ref().map_or_else(Keys::none, k));
                    }
                }
                if len == ABORT {
                    let t = target.take();
                    let tid = self.tid;
                    self.end_frame();
                    if let Some(t) = t {
                        ended(t, tid);
                    }
                    return true;
                }
                if len == 0 {
                    // the end of the burst: what the stages still hold goes to the leaf
                    let max = src.byte_max;
                    src.byte_max = src.byte_;
                    push(src, &mut chain.stages, &mut |r: &mut Rx<'_>| leaf(target, pending, r));
                    src.byte_max = max;
                    if !pending.is_empty() {
                        // the frame ends when what is kept is taken: the last byte of the
                        // terminator, read in this window, is left to the carrier's transport - it
                        // has something to offer again when the receive is woken
                        src.byte_ -= 1;
                        self.ending = true;
                        return false;
                    }
                    self.end_frame();
                    return true;
                }
                self.rem = len;
            }
            let n = (src.byte_max - src.byte_).min(self.rem as usize);
            if n == 0 {
                return false;
            }
            let (max, from) = (src.byte_max, src.byte_);
            src.byte_max = from + n;
            push(src, &mut chain.stages, &mut |r: &mut Rx<'_>| leaf(target, pending, r));
            src.byte_max = max;
            let taken = src.byte_ - from;
            self.rem -= taken as u32;
            if !pending.is_empty() {
                return false; // refused: the rest stays with the carrier's transport until its receive is woken
            }
            if taken == 0 {
                return false; // the stages take no more bytes of the burst: the next window
            }
        }
    }

    /// The frame is through: the next one starts with its `tunnel_id`.
    #[inline]
    fn end_frame(&mut self) {
        self.routed = false;
        self.tid = 0;
        self.tid_got = 0;
        self.target = None;
    }
}
