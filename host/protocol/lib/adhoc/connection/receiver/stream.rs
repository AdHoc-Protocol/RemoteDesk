// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Destinations for streamed packet fields and files.
//!
//! Chunked fields pass through their transform chains into an application-provided sink. File
//! fields carry their total length before the contents.

use super::*;

impl<'a> Rx<'a> {
    /// A pack nested in the one being received that passes the chain `chain`: one level down, as
    /// [`try_get_bytes`](Self::try_get_bytes), from chunks through the stages of the chain wire to
    /// leaf, up to the terminator. `this_case` — the case of the pack that calls it.
    #[inline]
    pub fn get_chain<D: BytesDst + ?Sized>(&mut self, chain: u32, dst: &mut D, this_case: u32) -> bool {
        self.get_chain_at(chain, dst, 0, this_case)
    }

    /// [`get_chain`](Self::get_chain) whose nested level starts at `enter`: a pack that is the leaf
    /// of the chain on one of its collection fields reads the field from its own case.
    pub fn get_chain_at<D: BytesDst + ?Sized>(&mut self, chain: u32, dst: &mut D, enter: u32, this_case: u32) -> bool {
        let parent = self.st.level;
        let level = parent + 1;
        if self.st.chains.len() <= level {
            self.st.chains.resize_with(level + 1, RxChain::default);
        }
        let mut ch = std::mem::take(&mut self.st.chains[level]);
        if self.st.depth <= level {
            // a new stream
            if self.st.slots.len() <= level {
                self.st.slots.push(RxSlot::default());
            }
            self.st.slots[level].state = enter;
            self.st.u8_ = 0;
            self.st.depth = level + 1;
            ch.arm((self.st.stages)(chain));
        }
        let mut done = {
            let RxChain { stages, frame, leaf, .. } = &mut ch;
            self.chain_body(stages, frame, &mut |r: &mut Rx<'_>| r.chain_leaf(leaf, dst, level))
        };
        if done && !ch.leaf.done && !self.st.drop_received {
            // a dropped pack (a key this side does not have) took its stream unread: its leaf is
            // not whole, nor is it to be
            self.st.error = Some(RxError::Broken("the stream ended before its pack"));
            done = false;
        }
        self.st.chains[level] = ch;
        self.st.level = parent;
        if done {
            self.st.depth = level;
            return true;
        }
        self.st.slots[parent].state = this_case;
        self.st.mode = mode::RETRY; // a value of the leaf the window cut waits in the chain, not in the receiver
        false
    }

    /// A `Stream` field, or a field trimmed on this host: chunks through the stages of `chain`
    /// (`None` — none) into the sink up to the terminator. `open` gives the sink once, on the first
    /// entry; it is dropped at the end. Above `max` (the `[S(N)]` cap) — an error. The sink is
    /// flushed when the stream is whole, then dropped; a stream torn (the connection lost, the pack
    /// abandoned) drops it unflushed: a sink that keeps a file tells the two apart so, and deletes
    /// the part in its `Drop`.
    #[inline]
    pub fn get_stream<F: FnOnce() -> Box<dyn std::io::Write + Send>>(&mut self, chain: Option<u32>, open: F, max: i64, this_case: u32) -> bool {
        self.stream_(chain, None::<fn() -> crate::lib::stages::Keys>, open, max, this_case, false)
    }

    /// The stream of the end of a plain tunnel ( a tunnel-only virtual connection ): as
    /// [`Rx::get_stream`], but an `ABORT` - its source voided it, or nothing serves the tunnel -
    /// ends the stream torn ( the sink goes unflushed ) and the connection goes on. Its stages are
    /// keyed by `keys` - the keys of the chain of the tunnel, taken as the stream starts -, never by
    /// the carrier's.
    #[inline]
    pub fn get_tunnel_stream<K: FnOnce() -> crate::lib::stages::Keys, F: FnOnce() -> Box<dyn std::io::Write + Send>>(&mut self, chain: Option<u32>, keys: K, open: F, this_case: u32) -> bool {
        self.stream_(chain, Some(keys), open, i64::MAX, this_case, true)
    }

    fn stream_<K: FnOnce() -> crate::lib::stages::Keys, F: FnOnce() -> Box<dyn std::io::Write + Send>>(
        &mut self,
        chain: Option<u32>,
        keys: Option<K>,
        open: F,
        max: i64,
        this_case: u32,
        tunnel: bool,
    ) -> bool {
        let parent = self.st.level;
        let level = parent + 1;
        if self.st.chains.len() <= level {
            self.st.chains.resize_with(level + 1, RxChain::default);
        }
        let mut ch = std::mem::take(&mut self.st.chains[level]);
        if self.st.depth <= level {
            // a new stream
            if self.st.slots.len() <= level {
                self.st.slots.push(RxSlot::default());
            }
            self.st.depth = level + 1;
            ch.arm(match chain {
                Some(id) => (self.st.stages)(id),
                None => &[],
            });
            if let Some(k) = keys {
                ch.keys(&k());
            }
            ch.raw = Some(open());
            ch.max = max as u64;
            ch.frame.tunnel = tunnel;
            ch.complete = self.st.stream_complete.take(); // armed for this stream (`on_stream_complete`)
        } else if self.st.stream_complete.is_some() {
            self.st.stream_complete = None; // armed again as the stream goes on: it is this stream's, armed already
        }
        let done = {
            let RxChain { stages, frame, raw, total, max, .. } = &mut ch;
            let max = *max;
            self.chain_body(stages, frame, &mut |r: &mut Rx<'_>| chunked::raw_sink(r, raw, total, max))
        };
        if done {
            ch.end(!ch.frame.void);
        }
        self.st.chains[level] = ch;
        self.st.level = parent;
        if done {
            self.st.depth = level;
            return true;
        }
        self.st.slots[parent].state = this_case;
        self.st.mode = mode::RETRY;
        false
    }

    /// Chunks of the stream up to its terminator; their bytes go through the stages to `leaf`.
    /// `true` — the terminator is read, and what the stages held is given to the leaf.
    pub(super) fn chain_body<L: FnMut(&mut Rx<'_>)>(&mut self, stages: &mut [chunked::RxStage], frame: &mut chunked::RxFrame, leaf: &mut L) -> bool {
        loop {
            if frame.rem == 0 {
                while frame.len_got < 2 {
                    if self.byte_max <= self.byte_ {
                        return false; // the length goes on in the next window
                    }
                    frame.len |= (self.buffer[self.byte_] as u32) << (8 * frame.len_got);
                    self.byte_ += 1;
                    frame.len_got += 1;
                }
                let len = frame.len;
                frame.len = 0;
                frame.len_got = 0;
                if len == crate::lib::adhoc::connection::transmitter::chunked::ABORT {
                    if frame.tunnel {
                        frame.void = true; // the source of the tunnel voided it: the stream ends, torn
                        return true;
                    }
                    self.st.error = Some(RxError::Broken("the stream was voided by its source"));
                    return false;
                }
                if len == 0 {
                    if !stages.is_empty() {
                        // the end: what the stages still hold goes to the leaf
                        let max = self.byte_max;
                        self.byte_max = self.byte_;
                        chunked::push(self, stages, leaf);
                        self.byte_max = max;
                        if self.st.error.is_some() {
                            return false;
                        }
                    }
                    return true;
                }
                frame.rem = len;
            }
            let feed = (frame.rem as usize).min(self.remaining());
            if feed == 0 {
                return false; // the chunk goes on in the next window
            }
            let (max, from) = (self.byte_max, self.byte_);
            self.byte_max = from + feed;
            chunked::push(self, stages, leaf);
            self.byte_max = max;
            if self.st.error.is_some() {
                return false;
            }
            let taken = self.byte_ - from;
            if taken == 0 {
                self.st.error = Some(RxError::Broken("the stages take no more bytes of the stream"));
                return false;
            }
            frame.rem -= taken as u32;
        }
    }

    /// The pack the chain carries, on the level of the chain, from the window: the value the
    /// previous window cut goes first.
    pub(crate) fn chain_leaf<D: BytesDst + ?Sized>(&mut self, leaf: &mut RxLeaf, dst: &mut D, level: usize) {
        if leaf.done {
            self.byte_ = self.byte_max; // the padding after the pack the stage completed early is drained
            return;
        }
        if leaf.mode != mode::OK {
            self.st.mode = leaf.mode;
            match self.resume() {
                1 => leaf.mode = mode::OK,
                0 => {
                    self.st.mode = mode::OK;
                    return; // this window ran out too
                }
                _ => return,
            }
        }
        self.st.mode = mode::OK;
        self.st.level = level;
        let done = dst.put_bytes(self);
        self.st.level = level - 1;
        if done {
            leaf.done = true;
            self.st.depth = level + 1; // the stream stays open: its end is due
            return;
        }
        let m = self.st.mode;
        if m != mode::OK && m != mode::RETRY {
            leaf.mode = m;
        }
        self.st.mode = mode::OK;
    }

    /// An embedded `File` field: the varint of the length, then that many bytes into the sink
    /// `open(length)` gives, once the length is known. The window ended — false, and the pack
    /// enters `this_case` again. A length above `max` (the `[S(N)]` cap) stops the stream with
    /// `RxError::Overflow`.
    pub fn get_file<F: FnOnce(u64) -> Box<dyn std::io::Write + Send>>(&mut self, open: F, max: i64, this_case: u32) -> bool {
        if self.st.file_phase == 0 {
            self.st.u8 = 0;
            self.st.bytes_left = 0;
            self.st.file_phase = 1;
        }
        if self.st.file_phase == 1 {
            if !self.varint() {
                self.retry_at(this_case);
                return false;
            }
            let len = self.st.u8;
            if max < 0 || (max as u64) < len {
                // unsigned: a length of 2^63 and more is over any cap, not a negative one under it
                self.st.file_phase = 0;
                self.st.error = Some(RxError::Overflow { got: len, max: max as u64 });
                return false;
            }
            self.st.file_rem = len;
            self.st.file_dst = Some(open(len));
            self.st.file_phase = 2;
        }
        while 0 < self.st.file_rem {
            let room = self.byte_max - self.byte_;
            if room == 0 {
                self.retry_at(this_case);
                return false;
            }
            let n = if self.st.file_rem < room as u64 { self.st.file_rem as usize } else { room };
            let dst = self.st.file_dst.as_mut().unwrap();
            if let Err(e) = std::io::Write::write_all(dst, &self.buffer[self.byte_..self.byte_ + n]) {
                // the sink of the application failed: it is told, and the rest of the file is read
                // unseen - the connection goes on
                crate::lib::adhoc::connection::receiver::on_error::rx_at(
                    crate::lib::adhoc::connection::receiver::on_error::RxOnError::Error,
                    &"the sink of a file",
                    &"failed: the rest of the file is read unseen",
                    Some(&e),
                );
                self.st.file_dst = Some(Box::new(std::io::sink()));
            }
            self.byte_ += n;
            self.st.file_rem -= n as u64;
        }
        if let Some(mut dst) = self.st.file_dst.take() {
            if let Err(e) = std::io::Write::flush(&mut dst) {
                crate::lib::adhoc::connection::receiver::on_error::rx_at(
                    crate::lib::adhoc::connection::receiver::on_error::RxOnError::Error,
                    &"the sink of a file",
                    &"its flush failed",
                    Some(&e),
                );
            }
        }
        self.st.file_phase = 0;
        true
    }
}
