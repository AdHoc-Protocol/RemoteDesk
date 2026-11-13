// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Streams sent: a pack or a field whose bytes go as a chunked stream through the stages of its
//! chain, a `Stream` field from the source the application opens, a `File` field of a length known
//! up front.

use super::*;

impl<'a> Tx<'a> {
    /// A pack nested in the one being sent that passes the chain `chain`: one level down, as
    /// [`put_bytes`](Self::put_bytes), its bytes through the stages of the chain into chunks, then
    /// the terminator. The stages are those the connection gives for the chain ([`TxHost::chain`]).
    /// `this_case` — the case of the pack that calls it.
    #[inline]
    pub fn put_chain<S: BytesSrc + ?Sized>(&mut self, chain: u32, src: &S, this_case: u32) -> bool { self.put_chain_at(chain, src, 1, this_case) }

    /// [`put_chain`](Self::put_chain) whose nested level starts at `enter`: a pack that is the leaf
    /// of the chain on one of its collection fields writes the field from its own case.
    pub fn put_chain_at<S: BytesSrc + ?Sized>(&mut self, chain: u32, src: &S, enter: u32, this_case: u32) -> bool {
        let parent = self.st.level;
        let level = parent + 1;
        if self.st.chains.len() <= level {
            self.st.chains.resize_with(level + 1, TxChain::default);
        }
        let mut ch = std::mem::take(&mut self.st.chains[level]);
        if self.st.depth <= level {
            // a new stream
            if self.st.slots.len() <= level {
                self.st.slots.push(TxSlot::default());
            }
            self.st.slots[level].state = enter;
            self.st.depth = level + 1;
            ch.arm((self.st.stages)(chain));
        }
        let done = {
            let TxChain { stages, terminator, leaf_mode, .. } = &mut ch;
            self.chain_body(stages, terminator, &mut |t: &mut Tx<'_>| t.chain_leaf(leaf_mode, src, level))
        };
        self.st.chains[level] = ch;
        self.st.level = parent;
        if done {
            self.st.depth = level;
            return true;
        }
        self.st.slots[parent].state = this_case;
        self.st.mode = mode::RETRY; // a value of the leaf the window cut waits in the chain, not in the transmitter
        false
    }

    /// A `Stream` field, or a field trimmed on this host: the bytes of the source through the
    /// stages of `chain` (`None` — none) into chunks, then the terminator. `open` gives the source
    /// once, on the first entry; above `max` (the `[S(N)]` cap) is an error of the sender. The
    /// buffer full — false, and the pack enters `this_case` again.
    #[inline]
    pub fn put_stream<F: FnOnce() -> Box<dyn std::io::Read + Send>>(&mut self, chain: Option<u32>, open: F, max: i64, this_case: u32) -> bool {
        self.stream_(chain, None::<fn() -> crate::lib::stages::Keys>, open, max, this_case)
    }

    /// The stream of the end of a plain tunnel ( a tunnel-only virtual connection ): as
    /// [`put_stream`](Self::put_stream), its stages keyed by `keys` - the keys of the chain of the
    /// tunnel, taken as the stream starts -, never by the carrier's: every relay on the way knows
    /// those.
    #[inline]
    pub fn put_tunnel_stream<K: FnOnce() -> crate::lib::stages::Keys, F: FnOnce() -> Box<dyn std::io::Read + Send>>(&mut self, chain: Option<u32>, keys: K, open: F, this_case: u32) -> bool {
        self.stream_(chain, Some(keys), open, i64::MAX, this_case)
    }

    fn stream_<K: FnOnce() -> crate::lib::stages::Keys, F: FnOnce() -> Box<dyn std::io::Read + Send>>(&mut self, chain: Option<u32>, keys: Option<K>, open: F, max: i64, this_case: u32) -> bool {
        let parent = self.st.level;
        let level = parent + 1;
        if self.st.chains.len() <= level {
            self.st.chains.resize_with(level + 1, TxChain::default);
        }
        let mut ch = std::mem::take(&mut self.st.chains[level]);
        if self.st.depth <= level {
            // a new stream
            if self.st.slots.len() <= level {
                self.st.slots.push(TxSlot::default());
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
            ch.complete = self.st.stream_complete.take(); // armed for this stream (`on_stream_complete`)
        } else if self.st.stream_complete.is_some() {
            self.st.stream_complete = None; // armed again as the stream goes on: it is this stream's, armed already
        }
        let done = {
            let TxChain { stages, terminator, raw, total, max, .. } = &mut ch;
            let max = *max;
            self.chain_body(stages, terminator, &mut |t: &mut Tx<'_>| chunked::raw_leaf(t, raw, total, max))
        };
        if done {
            ch.end(true);
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

    /// Chunks of the stream: `[length][bytes]` per window, at most [`chunked::MAX`] bytes each,
    /// then `[0x0000]`.
    pub(super) fn chain_body<L: FnMut(&mut Tx<'_>) -> bool>(&mut self, stages: &mut [chunked::TxStage], terminator: &mut bool, leaf: &mut L) -> bool {
        loop {
            if *terminator {
                if self.remaining() < 2 {
                    return false;
                }
                self.buffer[self.byte_] = 0;
                self.buffer[self.byte_ + 1] = 0;
                self.byte_ += 2;
                *terminator = false;
                return true;
            }
            if self.remaining() < 3 {
                return false; // the length and at least one byte
            }
            let len_at = self.byte_;
            self.byte_ = len_at + 2;
            let max = self.byte_max;
            if len_at + 2 + chunked::MAX < max {
                self.byte_max = len_at + 2 + chunked::MAX;
            }
            let done = chunked::pull(self, stages, leaf);
            self.byte_max = max;
            let n = self.byte_ - (len_at + 2);
            if n == 0 {
                self.byte_ = len_at; // nothing of the stream in this window
                if !done {
                    return false;
                }
            } else {
                self.buffer[len_at..len_at + 2].copy_from_slice(&(n as u16).to_le_bytes());
            }
            if done {
                *terminator = true;
            }
            // not done: the window ended the chunk, or the chunk is full — then the rest of the
            // window takes the next one
        }
    }

    /// The pack the chain carries, on the level of the chain: the value the previous window cut
    /// goes first.
    pub(crate) fn chain_leaf<S: BytesSrc + ?Sized>(&mut self, leaf_mode: &mut u8, src: &S, level: usize) -> bool {
        if *leaf_mode != mode::OK {
            self.st.mode = *leaf_mode;
            if !self.resume() {
                self.st.mode = mode::OK;
                return false; // this window has no room for it either
            }
            *leaf_mode = mode::OK;
        }
        self.st.mode = mode::OK;
        self.st.level = level;
        let done = src.get_bytes(self);
        self.st.level = level - 1;
        if done {
            self.st.depth = level + 1; // the stream stays open: its end is due
            return true;
        }
        let m = self.st.mode;
        if m != mode::OK && m != mode::RETRY {
            *leaf_mode = m;
        }
        self.st.mode = mode::OK;
        false
    }

    /// An embedded `File` field: the varint of the length, then exactly that many bytes of the
    /// source. `open` gives the source and its length, once, on the first entry. The buffer full —
    /// false, and the pack enters `this_case` again: the file goes on from where it stopped. A
    /// length above `max` (the `[S(N)]` cap) or a source ending before its length are errors of the
    /// sender.
    pub fn put_file<F: FnOnce() -> (Box<dyn std::io::Read + Send>, u64)>(&mut self, open: F, max: i64, this_case: u32) -> bool {
        if self.st.file_phase == 0 {
            let (src, len) = open();
            assert!(len as i64 <= max, "File size {len} exceeds max {max}");
            self.st.file_src = Some(src);
            self.st.file_rem = len;
            if !self.varint(len) {
                self.st.file_phase = 1;
                self.retry_at(this_case);
                return false;
            }
            self.st.file_phase = 2;
        } else if self.st.file_phase == 1 {
            let rest = self.st.u8_;
            if !self.varint(rest) {
                self.retry_at(this_case);
                return false;
            }
            self.st.file_phase = 2;
        }
        while 0 < self.st.file_rem {
            let room = self.byte_max - self.byte_;
            if room == 0 {
                self.retry_at(this_case);
                return false;
            }
            let n = if self.st.file_rem < room as u64 { self.st.file_rem as usize } else { room };
            let src = self.st.file_src.as_mut().unwrap();
            let read = match std::io::Read::read(src, &mut self.buffer[self.byte_..self.byte_ + n]) {
                // the length is the end-authority: a source at its end before it can never make it
                // up, and a pause would hold the pack for good - the guard of the loop ends the
                // connection, its host hears why
                Ok(0) => panic!("the source of a file ended with {} of its bytes still owed", self.st.file_rem),
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0, // nothing now: the pack pauses, its producer wakes the transmit
                Err(e) => panic!("the source of a file failed: {e}"), // A stream read error terminates the connection.
            };
            if read == 0 {
                self.retry_at(this_case);
                self.hold();
                return false;
            }
            self.byte_ += read;
            self.st.file_rem -= read as u64;
        }
        self.st.file_src = None;
        self.st.file_phase = 0;
        true
    }
}
