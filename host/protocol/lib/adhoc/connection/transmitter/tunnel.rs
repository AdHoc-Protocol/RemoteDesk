// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! The sending end of a tunnel: [`Out`] writes a burst of the transmitter of the virtual connection
//! into the window of the carrier's transmitter.

use crate::lib::adhoc::chain::Stage;
use crate::lib::adhoc::connection::transmitter::chunked::{pull, TxChain, ABORT, MAX};
use crate::lib::adhoc::connection::transmitter::{Tx, TxHost, TxState};
use crate::lib::adhoc::connection::tunnel::end_point::EndPoint;
use crate::lib::noise::{ByteSource, NoiseStream, Pull};
use crate::lib::stages::Keys;

/// What the transmitter of the virtual connection has now, into `buf`: `share` - its share carried,
/// the rest of the pack in progress and no other. Through the Noise stream of the tunnel when it has
/// one ([`EndPoint::secure`]): records of those bytes. (the bytes, a pack ended the share).
#[inline]
fn read<H: TxHost>(tx: &mut TxState, host: &mut H, noise: Option<&mut NoiseStream>, buf: &mut [u8], share: bool) -> (isize, bool) {
    let Some(ns) = noise else {
        return if share {
            let (r, _, done) = tx.read_one(buf, host);
            (r, done)
        } else {
            (tx.read(buf, host), false)
        };
    };
    tx.held = false; // the stream may give what it has with no read of the packs: a hold is of this call
    let r = pulled(ns.pull_from(&mut (&mut *tx, &mut *host, share), buf));
    (r, share && 0 < r && !tx.is_open())
}

/// The bytes a Noise stream gave as a read of a byte source gives them: the stream broken - nothing more.
#[inline]
fn pulled(p: std::io::Result<Pull>) -> isize {
    match p {
        Ok(Pull::Bytes(n)) => n as isize,
        Ok(Pull::Pending) => 0,
        Ok(Pull::Eof) | Err(_) => -1,
    }
}

/// The plaintext the Noise stream of a tunnel seals: the transmitter of its virtual connection, its
/// host, its share carried ([`read`]) - then the stream stops at the end of the pack in progress.
impl<H: TxHost> ByteSource for (&mut TxState, &mut H, bool) {
    #[inline]
    fn pull(&mut self, buf: &mut [u8]) -> std::io::Result<Pull> {
        let (tx, host, share) = self;
        let n = if *share {
            if !tx.is_open() {
                return Ok(Pull::Eof); // the pack of the share is out
            }
            tx.read_one(buf, *host).0
        } else {
            tx.read(buf, *host)
        };
        Ok(if 0 < n {
            Pull::Bytes(n as usize)
        } else if n == 0 {
            Pull::Pending
        } else {
            Pull::Eof
        })
    }
}

/// A burst of a tunnel on the carrier: the state of the transmitter of the virtual connection
/// between the bursts, and where the burst is.
#[derive(Default)]
pub struct Out {
    /// The transmitter of the virtual connection: a pack cut by the end of a chunk goes on in the
    /// next one.
    pub tx: TxState,
    head: bool,
    ended: bool,
    farewell: bool,
    /// The bytes of the virtual connection this burst carried: past [`MAX`] it ends at the end of
    /// the pack in progress.
    burst: usize,
    /// The stages of the chain of the tunnel ([`get_bytes_chain`](Self::get_bytes_chain)): made
    /// once, armed every burst.
    chain: TxChain,
}

impl Out {
    /// The burst queued or in flight is dropped with its carrier's socket: the pack it cut on the
    /// transmitter of the virtual connection is dropped too - the peer threw away what it got of it
    /// -, and the next burst starts afresh, its `[pack id][tunnel_id]` first. Between bursts
    /// nothing to drop.
    pub fn abandon(&mut self) {
        self.tx.abandon();
        self.head = false;
        self.ended = false;
        self.burst = 0;
    }

    /// The frame on the carrier (a pack of its transmitter): `[pack id][tunnel_id]`, what the
    /// transmitter of the virtual connection `host` has now as chunks, then `[0]` — `ABORT` when
    /// the end closes. `true` — the burst is out: the end opens its gate. A tunnel with a Noise
    /// stream ([`EndPoint::secure`]) carries its records.
    pub fn get_bytes<H: TxHost>(&mut self, dst: &mut Tx<'_>, host: &mut H, end: &EndPoint, pack_id: u32, id_bytes: usize, tid_bytes: usize) -> bool {
        // SAFETY: the transmit of the carrier, on its loop - the one carrier of the end
        let mut noise = end.noise().map(|n| unsafe { n.0.stream() });
        if !self.head {
            let prefix = id_bytes + tid_bytes;
            if dst.remaining() < prefix {
                dst.retry_at(0);
                return false;
            }
            let at = dst.byte_;
            dst.buffer[at..at + id_bytes].copy_from_slice(&pack_id.to_le_bytes()[..id_bytes]);
            dst.buffer[at + id_bytes..at + prefix].copy_from_slice(&end.tunnel_id().to_le_bytes()[..tid_bytes]);
            dst.byte_ += prefix;
            self.head = true;
            self.ended = false;
            self.farewell = end.armed();
            self.burst = 0;
        }
        let mut parked = false;
        while !self.ended {
            let room = dst.remaining();
            if room < 3 {
                dst.retry_at(0);
                return false;
            }
            let n = (room - 2).min(MAX);
            let at = dst.byte_;
            // its share carried, the burst ends at the end of a pack: the rest of the pack in
            // progress, no other; the last burst of a tunnel takes all that is queued
            let share = MAX <= self.burst && !self.farewell;
            if share && !self.tx.is_open() {
                end.more(); // packs may be left: the next burst, behind the other tunnels
                self.ended = true;
                continue;
            }
            let (r, last) = read(&mut self.tx, host, noise.as_deref_mut(), &mut dst.buffer[at + 2..at + 2 + n], share);
            if 0 < r {
                if parked {
                    parked = false;
                    end.unpark();
                }
                dst.buffer[at..at + 2].copy_from_slice(&(r as u16).to_le_bytes());
                dst.byte_ += 2 + r as usize;
                self.burst += r as usize;
                if last {
                    end.more(); // packs may be left: the next burst, behind the other tunnels
                    self.ended = true;
                }
            } else if r < 0 {
                self.ended = true; // nothing more now: the burst ends between packs
            } else if self.tx.held && !parked {
                parked = true; // the pack waits for its producer: its wake is to wake the carrier - a wake before this is
                end.park(); // seen by one more look
            } else {
                dst.retry_at(0); // the pack waits for a larger window, or for its producer
                if self.tx.held {
                    dst.hold(); // the carrier sleeps until the producer's wake (`EndPoint::park`)
                }
                return false;
            }
        }
        if dst.remaining() < 2 {
            dst.retry_at(0);
            return false;
        }
        let mark = if self.farewell { ABORT as u16 } else { 0 };
        let at = dst.byte_;
        dst.buffer[at..at + 2].copy_from_slice(&mark.to_le_bytes());
        dst.byte_ += 2;
        self.head = false;
        drop(noise); // let go before the end hears it: what that wakes, or hears the close, may take the stream
        end.sent(self.farewell);
        true
    }

    /// The frame on the carrier from a byte source of the virtual connection - its journal, when the connection guarantees
    /// packs ( C# `resumable.Init_( transmitter )` ): the bytes go as they come, the burst is shared with the other tunnels
    /// only at a pack boundary. Otherwise as [`get_bytes`](Self::get_bytes).
    pub fn get_bytes_src(&mut self, dst: &mut Tx<'_>, src: &mut dyn crate::lib::adhoc::BytesSrc, end: &EndPoint, pack_id: u32, id_bytes: usize, tid_bytes: usize) -> bool {
        // SAFETY: the transmit of the carrier, on its loop - the one carrier of the end
        let mut noise = end.noise().map(|n| unsafe { n.0.stream() });
        if !self.head {
            let prefix = id_bytes + tid_bytes;
            if dst.remaining() < prefix {
                dst.retry_at(0);
                return false;
            }
            let at = dst.byte_;
            dst.buffer[at..at + id_bytes].copy_from_slice(&pack_id.to_le_bytes()[..id_bytes]);
            dst.buffer[at + id_bytes..at + prefix].copy_from_slice(&end.tunnel_id().to_le_bytes()[..tid_bytes]);
            dst.byte_ += prefix;
            self.head = true;
            self.ended = false;
            self.farewell = end.armed();
            self.burst = 0;
        }
        while !self.ended {
            let room = dst.remaining();
            if room < 3 {
                dst.retry_at(0);
                return false;
            }
            if MAX <= self.burst && !self.farewell && src.at_boundary() {
                end.more(); // its share carried: the next burst, behind the other tunnels
                self.ended = true;
                continue;
            }
            let n = (room - 2).min(MAX);
            let at = dst.byte_;
            let buf = &mut dst.buffer[at + 2..at + 2 + n];
            let r = match noise.as_deref_mut() {
                None => src.read(buf),
                Some(ns) => pulled(ns.pull_from(&mut *src, buf)), // the journal under the records of the tunnel
            };
            if 0 < r {
                dst.buffer[at..at + 2].copy_from_slice(&(r as u16).to_le_bytes());
                dst.byte_ += 2 + r as usize;
                self.burst += r as usize;
            } else if r < 0 {
                self.ended = true; // nothing more now
            } else {
                dst.retry_at(0); // a pack waits for its producer, or for a larger window
                return false;
            }
        }
        if dst.remaining() < 2 {
            dst.retry_at(0);
            return false;
        }
        let mark = if self.farewell { ABORT as u16 } else { 0 };
        let at = dst.byte_;
        dst.buffer[at..at + 2].copy_from_slice(&mark.to_le_bytes());
        dst.byte_ += 2;
        self.head = false;
        drop(noise); // let go before the end hears it: what that wakes, or hears the close, may take the stream
        end.sent(self.farewell);
        true
    }

    /// Encodes a tunnel burst through the stages in `spec`, from leaf to wire, and emits the result
    /// as length-prefixed chunks. The leaf of a tunnel with a Noise stream gives its records. The
    /// stages are keyed by `keys` - the keys of the chain of the tunnel -, never by the carrier's
    /// ( `dst` ): every relay on the way knows those.
    pub fn get_bytes_chain<H: TxHost>(&mut self, dst: &mut Tx<'_>, host: &mut H, end: &EndPoint, pack_id: u32, id_bytes: usize, tid_bytes: usize, spec: &'static [Stage], keys: &Keys) -> bool {
        // SAFETY: the transmit of the carrier, on its loop - the one carrier of the end
        let mut noise = end.noise().map(|n| unsafe { n.0.stream() });
        if !self.head {
            let prefix = id_bytes + tid_bytes;
            if dst.remaining() < prefix {
                dst.retry_at(0);
                return false;
            }
            let at = dst.byte_;
            dst.buffer[at..at + id_bytes].copy_from_slice(&pack_id.to_le_bytes()[..id_bytes]);
            dst.buffer[at + id_bytes..at + prefix].copy_from_slice(&end.tunnel_id().to_le_bytes()[..tid_bytes]);
            dst.byte_ += prefix;
            self.head = true;
            self.ended = false;
            self.farewell = end.armed();
            self.burst = 0;
            self.chain.arm(spec);
            self.chain.keys(keys); // the tunnel's: the stages look at nothing of the carrier
        }
        let farewell = self.farewell;
        let mut cut = false;
        let mut parked = false;
        let (tx, chain, ended, burst) = (&mut self.tx, &mut self.chain, &mut self.ended, &mut self.burst);
        while !*ended {
            if dst.remaining() < 3 {
                dst.retry_at(0);
                return false; // the length and at least one byte
            }
            let len_at = dst.byte_;
            dst.byte_ = len_at + 2;
            let max = dst.byte_max;
            if len_at + 2 + MAX < max {
                dst.byte_max = len_at + 2 + MAX;
            }
            // the leaf: what the transmitter of the virtual connection has now; `true` — nothing
            // more, the burst ends
            let mut leaf = |t: &mut Tx<'_>| -> bool {
                while t.byte_ < t.byte_max {
                    let (at, m) = (t.byte_, t.byte_max);
                    if MAX <= *burst && !farewell && !tx.is_open() {
                        cut = true;
                        return true; // its share carried at a pack boundary: the packs left go in the next burst
                    }
                    let (r, last) = read(tx, host, noise.as_deref_mut(), &mut t.buffer[at..m], MAX <= *burst && !farewell); // its share carried: up to the end of a pack
                    if r <= 0 {
                        return r < 0; // 0 — the pack waits for a larger window
                    }
                    t.byte_ += r as usize;
                    *burst += r as usize;
                    if last {
                        cut = true;
                        return true; // the burst ends here: the packs left go in the next one
                    }
                }
                false
            };
            let done = pull(dst, &mut chain.stages, &mut leaf);
            dst.byte_max = max;
            let n = dst.byte_ - (len_at + 2);
            if n == 0 {
                dst.byte_ = len_at; // nothing of the burst in this window
                if !done {
                    if tx.held && !parked {
                        parked = true; // the pack waits for its producer: one more look after the park (as `get_bytes`)
                        end.park();
                        continue;
                    }
                    dst.retry_at(0);
                    if tx.held {
                        dst.hold(); // the carrier sleeps until the producer's wake
                    }
                    return false;
                }
            } else {
                if parked {
                    parked = false;
                    end.unpark();
                }
                dst.buffer[len_at..len_at + 2].copy_from_slice(&(n as u16).to_le_bytes());
            }
            if done {
                *ended = true;
                if cut {
                    end.more(); // packs may be left: the next burst, behind the other tunnels
                }
            }
        }
        if dst.remaining() < 2 {
            dst.retry_at(0);
            return false;
        }
        let mark = if self.farewell { ABORT as u16 } else { 0 };
        let at = dst.byte_;
        dst.buffer[at..at + 2].copy_from_slice(&mark.to_le_bytes());
        dst.byte_ += 2;
        self.head = false;
        drop(noise); // let go before the end hears it: what that wakes, or hears the close, may take the stream
        end.sent(self.farewell);
        true
    }
}
