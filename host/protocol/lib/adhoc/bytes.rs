// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Byte source and sink contracts used by the transport loops.
//!
//! [`BytesSrc`] serializes into a caller-owned buffer; [`BytesDst`] consumes received bytes. The
//! connection's loop calls both directions on its own thread. Partial consumption leaves the unread
//! tail with the caller, which retries it after the consumer signals readiness.

/// Where the bytes received go - typically the AdHoc receiver of the connection.
pub trait BytesDst: Send {
    /// Consumes the entire slice, returning `true` on success.
    /// A `false` result rejects the input. Implement `on_part` when backpressure can leave
    /// an unconsumed tail rather than failing the stream.
    fn on_bytes(&mut self, bytes: &[u8]) -> bool;

    /// Consumes bytes `[at, end)` from a transport receive buffer.
    ///
    /// Returns the consumed prefix length, or `None` if the stream is invalid. An unconsumed tail
    /// stays in the buffer; input resumes after the consumer calls
    /// [`External::wake_receive`](crate::lib::adhoc::connection::external::External::wake_receive).
    /// A sink consuming through the end of received data may exchange storage using
    /// [`RxBuf::take`](crate::lib::network::buf::RxBuf::take). The default delegates to
    /// [`on_slice`](Self::on_slice). The transport must provide valid bounds and exclusive access
    /// to this buffer for the call.
    fn on_block(&mut self, block: &crate::lib::network::buf::RxBuf, at: usize, end: usize) -> Option<usize> {
        // SAFETY: `[at, end)` are the connection's own bytes
        self.on_slice(unsafe { block.slice_mut(at, end) })
    }

    /// As [`on_block`](Self::on_block), but the bytes are not in a receive buffer: nobody may keep
    /// them past the call (a decrypting layer's own buffer). Default: [`on_part`](Self::on_part).
    fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
        self.on_part(bytes)
    }

    /// Read-only bytes: how many the sink takes - the rest stays with whoever offered them and is
    /// offered again once the sink's consumer wakes the receive. `None` - the stream is broken.
    /// Default: [`on_bytes`](Self::on_bytes), all or broken.
    fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
        if self.on_bytes(bytes) { Some(bytes.len()) } else { None }
    }

    /// The bytes of a record in the clear of the Noise of the socket: only frames of tunnels with a
    /// Noise of their own may be there - the sink takes them apart, anything else breaks the stream.
    /// How many it takes, as [`on_part`](Self::on_part). Default: none may be there - `None`.
    fn on_clear(&mut self, _bytes: &[u8]) -> Option<usize> {
        None
    }

    /// The same bytes, writable: a sink that decodes in place takes them without a copy. Default:
    /// [`on_bytes`](Self::on_bytes).
    #[inline]
    fn on_bytes_mut(&mut self, bytes: &mut [u8]) -> bool {
        self.on_bytes(bytes)
    }

    /// Returns whether the sink requests an orderly close after pending output is sent.
    /// The default keeps the connection open.
    fn graceful_close(&self) -> bool {
        false
    }

    /// The sink refused the stream for good (a pack it could not parse): the connection ends by our
    /// own verdict, its session does not park - not as a broken link, which a refusal is otherwise.
    /// Default: no.
    fn broken(&self) -> bool {
        false
    }

    /// The stream goes on from `position` of the peer's journal, past bytes this sink never got:
    /// what a pack left half received is dropped, a pack starts at `position`, the session hears
    /// it. Default: nothing.
    fn lost(&mut self, _position: i64) {}

    /// The bytes that follow do not continue a pack in flight: what a pack left half received is
    /// dropped. Default: nothing.
    fn abandon(&mut self) {}

    /// The bytes that follow come from the peer's journal, from the position
    /// [`received`](Self::received) named: a guaranteed pack in progress goes on with them; one of
    /// no guarantee, or a pack id read in part, is dropped. Default: nothing.
    fn continue_(&mut self) {}

    /// The position in the peer's journal the decoder has got to - the bytes of the guaranteed
    /// packs, the one in progress counted: what a datagram transport reports. `-1` - no position is
    /// known, or the sink counts none. Default: -1.
    fn received(&self) -> i64 {
        -1
    }

    /// Between packs: no pack, nor a pack id, is received in part - neither in hand nor kept by
    /// the session to go on with. Default: yes.
    fn idle(&self) -> bool {
        true
    }

    /// A sink holding a tail of its own takes no more until it is offered again
    /// ([`retry`](Self::retry)). Default: always.
    fn wants_more(&self) -> bool {
        true
    }

    /// A sink holding a tail offers it again. Default: nothing.
    fn retry(&mut self) {}

    /// The bytes this sink took and did not parse: the receiver of a session that moves onto the
    /// transport takes them ([`crate::lib::network::reseat`]). Default: none.
    fn take_tail(&mut self) -> Vec<u8> {
        Vec::new()
    }

    /// The socket of a stream under the session is gone, and the session parks: what survives a
    /// reconnect goes to the session (the receiver - a guaranteed pack in progress,
    /// [`crate::lib::adhoc::connection::receiver::Receiver`]), the rest is dropped with this sink.
    /// Default: nothing.
    fn suspend(&mut self) {}
}

impl<T: BytesDst + ?Sized> BytesDst for Box<T> {
    #[inline]
    fn on_bytes(&mut self, bytes: &[u8]) -> bool {
        (**self).on_bytes(bytes)
    }
    #[inline]
    fn on_bytes_mut(&mut self, bytes: &mut [u8]) -> bool {
        (**self).on_bytes_mut(bytes)
    }
    #[inline]
    fn on_block(&mut self, block: &crate::lib::network::buf::RxBuf, at: usize, end: usize) -> Option<usize> {
        (**self).on_block(block, at, end)
    }
    #[inline]
    fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
        (**self).on_slice(bytes)
    }
    #[inline]
    fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
        (**self).on_part(bytes)
    }
    #[inline]
    fn on_clear(&mut self, bytes: &[u8]) -> Option<usize> {
        (**self).on_clear(bytes)
    }
    #[inline]
    fn graceful_close(&self) -> bool {
        (**self).graceful_close()
    }
    #[inline]
    fn broken(&self) -> bool {
        (**self).broken()
    }
    #[inline]
    fn wants_more(&self) -> bool {
        (**self).wants_more()
    }
    #[inline]
    fn retry(&mut self) {
        (**self).retry()
    }
    #[inline]
    fn lost(&mut self, position: i64) {
        (**self).lost(position)
    }
    #[inline]
    fn abandon(&mut self) {
        (**self).abandon()
    }
    #[inline]
    fn continue_(&mut self) {
        (**self).continue_()
    }
    #[inline]
    fn received(&self) -> i64 {
        (**self).received()
    }
    #[inline]
    fn idle(&self) -> bool {
        (**self).idle()
    }
    #[inline]
    fn take_tail(&mut self) -> Vec<u8> {
        (**self).take_tail()
    }
    #[inline]
    fn suspend(&mut self) {
        (**self).suspend()
    }
}

/// Where the bytes to send come from - typically the AdHoc transmitter of the connection.
pub trait BytesSrc: Send {
    /// Serializes into `dst`, returning a positive byte count no greater than `dst.len()`. Zero
    /// means an unfinished packet is waiting; a negative value means the source is currently
    /// drained. A producer must wake transmission when it queues more output.
    fn read(&mut self, dst: &mut [u8]) -> isize;

    /// The bytes the last read gave are frames of tunnels with a Noise of their own: the Noise of
    /// the socket does not seal them again, they go in a record in the clear. A read gives bytes
    /// of one kind. Default: no.
    #[inline(always)]
    fn clear(&self) -> bool {
        false
    }

    /// No pack is half out: another source may take the transport from here
    /// ([`crate::lib::network::reseat`]). Default: always.
    fn at_boundary(&self) -> bool {
        true
    }

    /// The journal of the session this source sends, when it keeps one: a datagram transport runs
    /// it - the window is what it holds unreported, the reports of the peer give it back. Default:
    /// none.
    fn journal(&self) -> Option<&crate::lib::adhoc::connection::transmitter::resumable::Journal> {
        None
    }

    /// This source holds the journal: one source of the session at a time writes it - the next
    /// socket's takes over once the offline pull lets it go. Default: held.
    fn holds_journal(&mut self) -> bool {
        true
    }

    /// Runs `f` on the journal while this source holds it, the positions it moved published for the
    /// readers. `false` - not held, `f` did not run. Default: no journal.
    fn with_journal(&mut self, _f: &mut dyn FnMut(&mut crate::lib::adhoc::connection::transmitter::resumable::Resumable)) -> bool {
        false
    }

    /// The session is identified on this socket: the parked journal goes on. Default: nothing.
    fn online(&self) {}

    /// The peer asks the stream to go on from `position` of the journal: a guaranteed pack in the
    /// middle of serialization is finished into the journal - the peer has its head, the rest
    /// follows from there -, one of no guarantee is dropped. `-1` - the stream goes on from
    /// `position`; otherwise from the position returned, which the peer is to be told. Called only
    /// on a source that has a [`journal`](Self::journal). Default: -1.
    fn turn(&mut self, _position: i64) -> std::io::Result<i64> {
        Ok(-1)
    }

    /// The pack in flight is dropped: the stream goes on with the next one - a datagram transport
    /// whose peer lost the head of it, with no journal to send it again. Default: nothing.
    fn abandon(&mut self) {}

    /// Appends currently available output to `out`, stopping on either zero or a negative read.
    /// Always returns `true`; the return value does not distinguish pending output from a drained
    /// source.
    fn try_drain(&mut self, out: &mut Vec<u8>) -> bool {
        let mut chunk = [0u8; 4096];
        loop {
            let n = self.read(&mut chunk);
            if n <= 0 {
                return true;
            }
            out.extend_from_slice(&chunk[..n as usize]);
        }
    }
}

impl<T: BytesSrc + ?Sized> BytesSrc for Box<T> {
    #[inline]
    fn read(&mut self, dst: &mut [u8]) -> isize {
        (**self).read(dst)
    }
    #[inline]
    fn clear(&self) -> bool {
        (**self).clear()
    }
    #[inline]
    fn at_boundary(&self) -> bool {
        (**self).at_boundary()
    }
    #[inline]
    fn journal(&self) -> Option<&crate::lib::adhoc::connection::transmitter::resumable::Journal> {
        (**self).journal()
    }
    #[inline]
    fn holds_journal(&mut self) -> bool {
        (**self).holds_journal()
    }
    #[inline]
    fn with_journal(&mut self, f: &mut dyn FnMut(&mut crate::lib::adhoc::connection::transmitter::resumable::Resumable)) -> bool {
        (**self).with_journal(f)
    }
    #[inline]
    fn online(&self) {
        (**self).online()
    }
    #[inline]
    fn turn(&mut self, position: i64) -> std::io::Result<i64> {
        (**self).turn(position)
    }
    #[inline]
    fn abandon(&mut self) {
        (**self).abandon()
    }
}

/// For a source that produces its bytes into a vector of its own (a compressor, a record layer):
/// they go out from there, as much as each buffer of the transport takes.
#[derive(Default)]
pub struct Spill {
    buf: Vec<u8>,
    at: usize,
}

impl Spill {
    /// Copies what is spilled into `dst`; when nothing is, `produce` appends the next bytes first
    /// (false - nothing more).
    pub fn read(&mut self, dst: &mut [u8], produce: impl FnOnce(&mut Vec<u8>) -> bool) -> isize {
        if self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
            let more = produce(&mut self.buf);
            if self.buf.is_empty() {
                return if more { 0 } else { -1 };
            }
        }
        let n = (self.buf.len() - self.at).min(dst.len());
        dst[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        n as isize
    }

    pub fn is_empty(&self) -> bool {
        self.at == self.buf.len()
    }
}
