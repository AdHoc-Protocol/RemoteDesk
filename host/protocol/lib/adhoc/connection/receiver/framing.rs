// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! COBS-style framing with a `0xFF` packet delimiter and CRC-32C validation.
//!
//! Each packet starts with `0xFF`. Its payload and four-byte, most-significant-byte-first CRC are
//! encoded without further delimiter bytes. A short block `[c][c bytes]`, `c <= 253`, implies a
//! payload `0xFF` after the block except at packet end. A long block
//! `[254][l0][l1][l0 + 255 * l1 bytes]`, with `l0, l1 <= 254`, implies none.
//!
//! [`State::write`] decodes in place and forwards decoded runs to the receiver. Every wire byte is
//! checked for the delimiter so decoding can restart after a damaged frame. Packet delivery
//! completes only after its checksum matches.

use crate::lib::adhoc::helpers::{crc32c, CRC_LEN_BYTES};
use crate::lib::adhoc::connection::receiver::{RxHost, RxState};

/// The bytes of a long block at most: its length goes as two bytes 0..254, the low one first.
pub(crate) const LONG: usize = 254 + 254 * 255;

/// The high bit of each byte `0xFF` of the word: exact at the lowest such byte, the ones above it
/// may be off.
#[inline(always)]
pub(crate) fn ff_in(w: u64) -> u64 {
    let v = !w; // a 0xFF is 0 here
    v.wrapping_sub(0x0101_0101_0101_0101) & !v & 0x8080_8080_8080_8080
}

/// The bytes at the start of `src` before its first `0xFF` - all of them when it has none. A short
/// run the first word tells; a long one is looked through 32 bytes a step with AVX2, 16 with SSE2,
/// a word elsewhere.
#[inline]
pub(crate) fn ff_free(src: &[u8]) -> usize {
    if 8 <= src.len() {
        let m = ff_in(u64::from_le_bytes(src[..8].try_into().unwrap()));
        if m != 0 {
            return (m.trailing_zeros() / 8) as usize;
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the processor has AVX2
            return unsafe { ff_free_avx2(src) };
        }
        // SAFETY: SSE2 is in every x86_64
        return unsafe { ff_free_sse2(src) };
    }
    #[allow(unreachable_code)]
    ff_free_words(src, 0)
}

/// As [`ff_free`], from `i` on: a word a step, then a byte.
fn ff_free_words(src: &[u8], mut i: usize) -> usize {
    let n = src.len();
    while i + 8 <= n {
        let m = ff_in(u64::from_le_bytes(src[i..i + 8].try_into().unwrap()));
        if m != 0 {
            return i + (m.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n && src[i] != 0xFF {
        i += 1;
    }
    i
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn ff_free_avx2(src: &[u8]) -> usize {
    use std::arch::x86_64::*;
    let (p, n) = (src.as_ptr(), src.len());
    let ones = _mm256_set1_epi8(-1);
    let mut i = 0;
    while i + 32 <= n {
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(i) as *const __m256i), ones)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 32;
    }
    ff_free_words(src, i)
}

#[cfg(target_arch = "x86_64")]
unsafe fn ff_free_sse2(src: &[u8]) -> usize {
    use std::arch::x86_64::*;
    let (p, n) = (src.as_ptr(), src.len());
    let ones = _mm_set1_epi8(-1);
    let mut i = 0;
    while i + 16 <= n {
        let m = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_loadu_si128(p.add(i) as *const __m128i), ones)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 16;
    }
    ff_free_words(src, i)
}

/// Why the framing dropped what it was receiving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingError {
    /// A pack not whole at the `0xFF` of the next one, or bytes behind a pack before it.
    BytesDistortion,
    /// A `0xFF` with no byte behind it before the next one.
    FfFf,
    /// The CRC of the pack does not match.
    Crc,
}

/// The receiving framing.
pub struct State {
    /// The bytes left in the block being received: 0 - the next byte is a code.
    left: usize,
    /// The bytes of the length of a long block left to come: 2, 1 or 0.
    length: u8,
    /// The low byte of the length of a long block.
    low: usize,
    /// The block before was a short one: a `0xFF` of the data comes before the next block.
    implied: bool,
    /// A byte of the pack came after its `0xFF`.
    started: bool,
    /// The bytes up to the next `0xFF` are skipped: at the start, after a pack, after a distortion.
    seek: bool,
    /// A pack ended: a byte before the next `0xFF` is told as a distortion.
    after: bool,
    /// The CRC-32C of the bytes of the pack so far.
    crc: u32,
    pack_crc: u32,
    pack_crc_byte: i32,
    /// A pack is whole: the next decoded bytes are its CRC.
    waiting: bool,
    /// The last error, for the one who cares; the stream goes on from the next pack.
    pub last_error: Option<FramingError>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            left: 0,
            length: 0,
            low: 0,
            implied: false,
            started: false,
            seek: true,
            after: false,
            crc: 0,
            pack_crc: 0,
            pack_crc_byte: 0,
            waiting: false,
            last_error: None,
        }
    }

    /// The pack in flight is dropped: the bytes that follow are skipped up to the start of the next
    /// one.
    pub fn drop_frame(&mut self) {
        self.reset();
        self.seek = true;
        self.after = false;
    }

    /// Forgets the pack in flight: its CRC, where its blocks are.
    fn reset(&mut self) {
        self.crc = 0;
        self.waiting = false;
        self.left = 0;
        self.length = 0;
        self.implied = false;
    }

    /// The pack is dropped for `e`: noted, and told to the handler of the errors of the receive.
    #[cold]
    fn fail(&mut self, e: FramingError, rx: &RxState) {
        self.last_error = Some(e);
        use crate::lib::adhoc::connection::receiver::on_error::RxOnError;
        let (code, what) = match e {
            FramingError::BytesDistortion => (RxOnError::BytesDistortion, "a pack torn, or bytes behind a pack"),
            FramingError::FfFf => (RxOnError::FfffError, "a 0xFF with no byte of a pack behind it"),
            FramingError::Crc => (RxOnError::CrcError, "the CRC of a pack does not match: the pack is dropped"),
        };
        crate::lib::adhoc::connection::receiver::on_error::rx_at(code, rx, &what, None);
    }

    /// The pack in the receiver dropped too: a pack id read in part as well, or the next pack would
    /// go on with it.
    fn error_reset<H: RxHost>(&mut self, e: FramingError, rx: &mut RxState, host: &mut H) {
        self.fail(e, rx);
        super::dropped(rx, host);
        self.reset();
    }

    /// The wire bytes in `src`: decoded where they lie and received. False — never now: the
    /// receiver's own error drops the pack, the stream goes on at the next `0xFF`.
    pub fn write<H: RxHost>(&mut self, src: &mut [u8], rx: &mut RxState, host: &mut H) -> bool {
        let limit = src.len();
        let (mut p, mut from) = (0, 0); // the wire bytes taken; where the run being taken starts
        loop {
            let ff = p + ff_free(&src[p..]); // where the bytes of this pack end in the window
            if self.seek {
                if self.after && p < ff {
                    self.after = false; // bytes behind a pack, before the 0xFF of the next one: a 0xFF distorted, a pack lost
                    self.fail(FramingError::BytesDistortion, rx);
                }
            } else {
                while p < ff {
                    if 0 < self.left {
                        // the bytes of a block stay where they are
                        let n = self.left.min(ff - p);
                        p += n;
                        self.left -= n;
                    } else if 0 < self.length {
                        // a byte of the length of a long block: taken out between two runs
                        self.deliver(&src[from..p], rx, host);
                        let b = src[p] as usize;
                        if self.length == 2 {
                            self.low = b;
                        } else {
                            self.left = self.low + 255 * b;
                        }
                        self.length -= 1;
                        p += 1;
                        from = p;
                    } else {
                        // a code: the 0xFF the block before implies goes in its place, else it is
                        // taken out between two runs
                        let c = src[p];
                        if self.implied {
                            src[p] = 0xFF;
                        } else {
                            self.deliver(&src[from..p], rx, host);
                            from = p + 1;
                        }
                        p += 1;
                        self.started = true;
                        if c < 254 {
                            self.left = c as usize;
                            self.implied = true;
                        } else {
                            self.length = 2;
                            self.implied = false;
                        }
                    }
                }
            }
            if ff == limit {
                break;
            }
            // 0xFF: a pack starts
            if !self.seek {
                self.deliver(&src[from..ff], rx, host); // a whole pack ends there: its CRC checked, the bytes skipped from then on
            }
            if !self.seek {
                // the pack was not whole
                if self.started {
                    self.error_reset(FramingError::BytesDistortion, rx, host); // its id, body or CRC
                } else {
                    self.fail(FramingError::FfFf, rx); // no byte of it at all
                }
            } else if !rx.idle() {
                // bytes behind a pack the receiver took for a pack
                if self.after {
                    self.fail(FramingError::BytesDistortion, rx);
                }
                super::dropped(rx, host);
            }
            self.reset();
            self.seek = false;
            self.after = false;
            self.started = false;
            p = ff + 1;
            from = p;
        }
        if !self.seek {
            self.deliver(&src[from..limit], rx, host);
        }
        host.on_window(rx); // once a receive call: a host that counts guaranteed packs reports its position
        true
    }

    /// Hands the decoded bytes on: the CRC of a whole pack first, then the receiver; the bytes it
    /// takes go into the CRC. Bytes behind a pack go to none; the receiver's own error drops the
    /// pack, the rest of it is skipped.
    fn deliver<H: RxHost>(&mut self, dec: &[u8], rx: &mut RxState, host: &mut H) {
        let mut at = 0;
        while at < dec.len() {
            if self.waiting {
                self.getting_crc(dec[at], rx, host);
                at += 1;
                continue;
            }
            if self.seek {
                return;
            }
            let (n, done) = rx.write(&dec[at..], host);
            if n == 0 && !done {
                // the receiver takes no more of these bytes (a sink that holds them back): a frame
                // decoded in place cannot wait for it - the pack is dropped, told, as below, not
                // offered again for ever
                rx.error = Some(super::RxError::Broken("the receiver takes no more bytes of the frame"));
            }
            if n <= 0 && !done {
                // an id it cannot dispatch, a value out of its limits: the pack is dropped, the
                // stream goes on at the next 0xFF
                if let Some(e) = rx.error.take() {
                    e.report(rx);
                }
                super::dropped(rx, host);
                self.reset();
                self.seek = true;
                return;
            }
            self.crc = crc32c(self.crc, &dec[at..at + n as usize]);
            at += n as usize;
            if done {
                self.pack_crc = 0;
                self.pack_crc_byte = CRC_LEN_BYTES as i32 - 1;
                self.waiting = true;
            }
        }
    }

    /// A byte of the CRC of the pack received, the high byte first: the last one checks it - the
    /// pack goes to the host or is dropped. The bytes up to the next pack are of none.
    fn getting_crc<H: RxHost>(&mut self, crc_byte: u8, rx: &mut RxState, host: &mut H) {
        self.pack_crc |= (crc_byte as u32) << (self.pack_crc_byte * 8);
        self.pack_crc_byte -= 1;
        if -1 < self.pack_crc_byte {
            return;
        }
        if self.crc == self.pack_crc {
            self.dispatch(rx, host);
        } else {
            self.fail(FramingError::Crc, rx);
        }
        self.reset();
        self.seek = true;
        self.after = true;
    }

    fn dispatch<H: RxHost>(&mut self, rx: &mut RxState, host: &mut H) {
        if rx.drop_received {
            rx.drop_received = false;
            host.on_dropped(rx);
        } else {
            host.on_received(rx);
        }
    }
}

/// The receive cannot go on past `rx`'s error: an unknown pack id is a loss - the bytes may be of
/// another version of the protocol, the session may park; any other error broke the stream for
/// good.
#[inline]
pub(crate) fn broken(rx: &RxState) -> bool {
    rx.error.as_ref().is_some_and(|e| !matches!(e, super::RxError::InvalidId(_)))
}

/// The bytes of the network into the receiver `H` of a connection, framed.
pub struct Framing<H: RxHost> {
    host: H,
    rx: RxState,
    framing: State,
    /// For the bytes that come read-only: decoded in place here.
    copy: Vec<u8>,
}

impl<H: RxHost> Framing<H> {
    /// `id_bytes` — the bytes of a pack id with the address of its actor (`Receiver::ID_BYTES` of
    /// the generated code).
    pub fn new(host: H, id_bytes: i32) -> Self {
        Self { host, rx: RxState::new(id_bytes), framing: State::new(), copy: Vec::new() }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    /// The frames go to `host` from now on: the decoder and the parse start over, seeking the next
    /// frame; the keys of the session and of its envelope stay. Returns the host before.
    pub fn switch_to(&mut self, host: H) -> H {
        let keys = self.rx.keys.clone();
        let envelope = self.rx.envelope.take();
        self.rx = RxState::new(self.rx.id_bytes);
        self.rx.keys = keys;
        self.rx.envelope = envelope;
        self.framing = State::new();
        std::mem::replace(&mut self.host, host)
    }
}

impl<H: RxHost + Send> crate::lib::adhoc::BytesDst for Framing<H> {
    /// The stream goes on past bytes never received: the frame and the pack in flight are dropped,
    /// the session hears it.
    fn lost(&mut self, position: i64) {
        crate::lib::adhoc::BytesDst::abandon(self);
        super::positioned(&self.host, |p| p.position(position));
        self.host.on_lost();
    }

    fn broken(&self) -> bool {
        broken(&self.rx)
    }

    /// The bytes that follow do not continue the frame in flight: it and its pack are dropped.
    fn abandon(&mut self) {
        self.framing.drop_frame();
        super::dropped(&mut self.rx, &mut self.host);
    }

    /// The socket is gone and the session parks: a frame is checked whole (its CRC), so the pack in
    /// progress is dropped - the peer sends it again from its start.
    fn suspend(&mut self) {
        crate::lib::adhoc::BytesDst::abandon(self);
    }

    fn on_bytes(&mut self, bytes: &[u8]) -> bool {
        let mut copy = std::mem::take(&mut self.copy);
        copy.clear();
        copy.extend_from_slice(bytes);
        let ok = self.framing.write(&mut copy, &mut self.rx, &mut self.host);
        self.copy = copy;
        ok
    }

    #[inline]
    fn on_bytes_mut(&mut self, bytes: &mut [u8]) -> bool {
        self.framing.write(bytes, &mut self.rx, &mut self.host)
    }

    /// The transport's own bytes: decoded where they lie, no copy.
    #[inline]
    fn on_slice(&mut self, bytes: &mut [u8]) -> Option<usize> {
        self.framing.write(bytes, &mut self.rx, &mut self.host).then_some(bytes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::adhoc::connection::transmitter::framing::State as TxFraming;
    use crate::lib::adhoc::connection::receiver::{BytesDst, Rx};
    use crate::lib::adhoc::connection::transmitter::{BytesSrc, Tx, TxHost, TxState};

    #[derive(Default, Clone, Debug, PartialEq)]
    struct P {
        a: u64,
        s: String,
    }

    impl BytesSrc for P {
        fn get_bytes(&self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    0 => {
                        if !dst.put_val(9, 1, 1) {
                            return false;
                        }
                        dst.set_state(1);
                    }
                    1 => {
                        if !dst.put_ulong_c(self.a, 2) {
                            return false;
                        }
                        dst.set_state(2);
                    }
                    2 => return dst.put_string(&self.s, u32::MAX, 2),
                    _ => return true,
                }
            }
        }
    }

    impl BytesDst for P {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            loop {
                match src.state() {
                    0 => {
                        if !src.has_8bytes(1) {
                            return false;
                        }
                        self.a = src.get_ulong();
                        src.set_state(2);
                    }
                    1 => {
                        self.a = src.get_ulong_();
                        src.set_state(2);
                    }
                    2 => {
                        if !src.try_get_string(10_000, 3) {
                            return false;
                        }
                        src.set_state(3);
                    }
                    3 => {
                        self.s = src.get_string().unwrap_or_default();
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    struct Out(Vec<P>, Option<P>);
    impl TxHost for Out {
        fn on_serializing(&mut self) -> bool {
            self.1 = if self.0.is_empty() { None } else { Some(self.0.remove(0)) };
            self.1.is_some()
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            self.1.as_ref().unwrap().get_bytes(dst)
        }
        fn on_serialized(&mut self) {}
    }

    struct In(P, Vec<P>);
    impl RxHost for In {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            id == 9
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            self.0.put_bytes(src)
        }
        fn on_received(&mut self, _st: &mut RxState) {
            self.1.push(std::mem::take(&mut self.0));
        }
    }

    fn packs() -> Vec<P> {
        vec![
            // 0x7F and 0xFF bytes in the value: escaping is exercised
            P { a: 0x7F7F_FFFF_7F00_FF7F, s: "a\u{7F}\u{FF}b".into() },
            P { a: 0, s: "x".repeat(300) },
            P { a: u64::MAX, s: "юникод 😀".into() },
        ]
    }

    fn framed(size: usize) -> Vec<u8> {
        let mut st = TxState::new();
        let mut fr = TxFraming::new();
        let mut out = Out(packs(), None);
        let mut wire = Vec::new();
        let mut buf = vec![0u8; size];
        loop {
            let n = fr.read(&mut buf, &mut st, &mut out);
            if n < 0 {
                break;
            }
            wire.extend_from_slice(&buf[..n as usize]);
        }
        wire
    }

    #[test]
    fn framed_round_trip() {
        // the blocks the end of a transmit buffer cuts differ by the buffer: every wire decodes to
        // the packs, through every window
        for size in (32..96).chain([4096]) {
            let wire = framed(size);
            for window in 1..64 {
                let mut st = RxState::new(1);
                let mut fr = State::new();
                let mut inp = In(P::default(), vec![]);
                for chunk in wire.chunks(window) {
                    let mut c = chunk.to_vec();
                    assert!(fr.write(&mut c, &mut st, &mut inp));
                }
                assert_eq!(fr.last_error, None, "transmit buffer {size}, window {window}");
                assert_eq!(inp.1, packs(), "transmit buffer {size}, window {window}");
            }
        }
    }

    /// A pack torn by the start of the next frame: the host hears it dropped, the frames around it
    /// are received.
    #[test]
    fn a_torn_pack_is_told_to_the_host() {
        struct Torn(In, usize);
        impl RxHost for Torn {
            fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool {
                self.0.on_receiving(src, id)
            }
            fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
                self.0.put_bytes(src)
            }
            fn on_received(&mut self, st: &mut RxState) {
                self.0.on_received(st)
            }
            fn on_abandon(&mut self) {
                self.1 += 1;
                self.0 .0 = P::default();
            }
        }
        let wire = framed(4096);
        let ff: Vec<usize> = (0..wire.len()).filter(|&i| wire[i] == 0xFF).collect();
        assert!(3 <= ff.len(), "{ff:?}");
        let mid = ff[1] + (ff[2] - ff[1]) / 2;
        let mut torn = wire[..mid].to_vec();
        torn.extend_from_slice(&wire[ff[2]..]);
        let mut st = RxState::new(1);
        let mut fr = State::new();
        let mut host = Torn(In(P::default(), vec![]), 0);
        assert!(fr.write(&mut torn, &mut st, &mut host));
        let p = packs();
        assert_eq!((host.0 .1, host.1), (vec![p[0].clone(), p[2].clone()], 1));
    }

    /// The packs framed, their ids `id_bytes` wide.
    fn wire(id_bytes: usize) -> Vec<u8> {
        struct Wide(Vec<P>, Option<P>, usize);
        impl TxHost for Wide {
            fn on_serializing(&mut self) -> bool {
                self.1 = if self.0.is_empty() { None } else { Some(self.0.remove(0)) };
                self.1.is_some()
            }
            fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
                if dst.state() == 0 {
                    if !dst.put_val(9, self.2, 1) {
                        return false;
                    }
                    dst.set_state(1);
                }
                self.1.as_ref().unwrap().get_bytes(dst)
            }
            fn on_serialized(&mut self) {}
        }
        let mut st = TxState::new();
        let mut fr = TxFraming::new();
        let mut out = Wide(packs(), None, id_bytes);
        let mut wire = Vec::new();
        let mut buf = vec![0u8; 4096];
        loop {
            let n = fr.read(&mut buf, &mut st, &mut out);
            if n < 0 {
                break;
            }
            wire.extend_from_slice(&buf[..n as usize]);
        }
        wire
    }

    /// A pack id torn by the start of the next frame: dropped, not grafted onto the next pack's id.
    #[test]
    fn a_torn_pack_id_is_dropped() {
        let wire = wire(2);
        let ff: Vec<usize> = (0..wire.len()).filter(|&i| wire[i] == 0xFF).collect();
        assert!(3 <= ff.len(), "{ff:?}");
        let mut torn = wire[..ff[1] + 2].to_vec(); // the second frame: its 0xFF and one byte of the two of its pack id
        torn.extend_from_slice(&wire[ff[2]..]);
        let mut st = RxState::new(2);
        let mut fr = State::new();
        let mut host = In(P::default(), vec![]);
        assert!(fr.write(&mut torn, &mut st, &mut host));
        let p = packs();
        assert_eq!(fr.last_error, Some(FramingError::BytesDistortion));
        assert_eq!(host.1, vec![p[0].clone(), p[2].clone()]);
    }

    /// A frame the next one tore at any point - in the pack id, the body, the CRC -, whole in a
    /// window or a byte at a time: it is dropped, the frames around it are received.
    #[test]
    fn a_frame_torn_anywhere_is_dropped() {
        let p = packs();
        for id_bytes in [1, 2] {
            let wire = wire(id_bytes);
            let ff: Vec<usize> = (0..wire.len()).filter(|&i| wire[i] == 0xFF).collect();
            assert!(3 <= ff.len(), "{ff:?}");
            for cut in ff[1] + 1..ff[2] {
                let mut torn = wire[..cut].to_vec();
                torn.extend_from_slice(&wire[ff[2]..]);
                for window in [torn.len(), 1] {
                    let mut st = RxState::new(id_bytes as i32);
                    let mut fr = State::new();
                    let mut host = In(P::default(), vec![]);
                    for chunk in torn.chunks(window) {
                        let mut c = chunk.to_vec();
                        assert!(fr.write(&mut c, &mut st, &mut host));
                    }
                    assert!(host.1 == [p[0].clone(), p[2].clone()], "id {id_bytes} torn at {} of {}, window {window}: {} packs, {:?}", cut - ff[1], ff[2] - ff[1], host.1.len(), fr.last_error);
                    assert!(fr.last_error.is_some(), "id {id_bytes} torn at {} of {}, window {window}: told", cut - ff[1], ff[2] - ff[1]);
                }
            }
        }
    }

    /// Every way of finding the next 0xFF - the first word, AVX2, SSE2, words, bytes - gives the
    /// first one: at every place, after every length of other bytes (0x7F and 0xFE among them),
    /// whatever follows it.
    #[test]
    fn the_next_ff_found_every_way() {
        for len in 0..100 {
            for at in 0..=len {
                let mut v: Vec<u8> = (0..len).map(|i| [0x7F, 0xFE, 0x00, 0x80, (i * 37 % 251) as u8][i % 5]).collect();
                let expect = if at < len {
                    v[at] = 0xFF;
                    for x in &mut v[at + 1..] {
                        *x = if *x % 2 == 0 { 0xFF } else { *x }; // more behind the first
                    }
                    at
                } else {
                    len
                };
                assert_eq!(ff_free(&v), expect, "len {len} at {at}");
                assert_eq!(ff_free_words(&v, 0), expect, "words, len {len} at {at}");
                #[cfg(target_arch = "x86_64")]
                {
                    assert_eq!(unsafe { ff_free_sse2(&v) }, expect, "SSE2, len {len} at {at}");
                    if std::arch::is_x86_feature_detected!("avx2") {
                        assert_eq!(unsafe { ff_free_avx2(&v) }, expect, "AVX2, len {len} at {at}");
                    }
                }
            }
        }
    }

    /// What the transport handed over is parsed: the host hears it once a receive call - where a
    /// host that counts guaranteed packs reports its position to the peer.
    #[test]
    fn the_host_hears_each_window() {
        struct Windows(In, usize);
        impl RxHost for Windows {
            fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool {
                self.0.on_receiving(src, id)
            }
            fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
                self.0.put_bytes(src)
            }
            fn on_received(&mut self, st: &mut RxState) {
                self.0.on_received(st)
            }
            fn on_window(&mut self, _st: &mut RxState) {
                self.1 += 1;
            }
        }
        let wire = framed(4096);
        let mut st = RxState::new(1);
        let mut fr = State::new();
        let mut host = Windows(In(P::default(), vec![]), 0);
        let mut n = 0;
        for chunk in wire.chunks(10) {
            let mut c = chunk.to_vec();
            assert!(fr.write(&mut c, &mut st, &mut host));
            n += 1;
        }
        assert_eq!((host.0 .1.len(), host.1), (packs().len(), n));
    }

    /// Runs of plain bytes between escape bytes at every distance: the word paths and the byte
    /// paths take turns at every shift, both ways, through every window.
    #[test]
    fn framed_dense_escapes() {
        let mut seed = 0x9E37_79B9u32;
        let mut packs = vec![];
        for n in 0..40 {
            let mut s = String::new();
            for _ in 0..(n * 37 % 700) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                s.push(match seed % 11 {
                    0 => '\u{7F}',
                    1 => '\u{FF}',
                    2 => '\u{3FFF}',
                    3 => '\u{4000}',
                    4 => 'ж',
                    5 => '日',
                    6 => '😀',
                    _ => (b'a' + (seed >> 8) as u8 % 26) as char,
                });
            }
            packs.push(P { a: (seed as u64) << 32 | (n as u64 * 0x0101_7F7F), s });
        }
        let mut st = TxState::new();
        let mut fr = TxFraming::new();
        let mut out = Out(packs.clone(), None);
        let mut wire = Vec::new();
        let mut buf = vec![0u8; 97];
        loop {
            let n = fr.read(&mut buf, &mut st, &mut out);
            if n < 0 {
                break;
            }
            wire.extend_from_slice(&buf[..n as usize]);
        }
        for window in [1usize, 2, 3, 7, 8, 9, 15, 16, 17, 63, 64, 1000, 100_000] {
            let mut st = RxState::new(1);
            let mut fr = State::new();
            let mut inp = In(P::default(), vec![]);
            for chunk in wire.chunks(window) {
                let mut c = chunk.to_vec();
                assert!(fr.write(&mut c, &mut st, &mut inp));
            }
            assert_eq!(fr.last_error, None, "window {window}");
            assert!(inp.1 == packs, "window {window}");
        }
    }
}

#[cfg(test)]
mod framed_tests {
    use super::*;
    use crate::lib::adhoc::{BytesDst, BytesSrc as _};
    use crate::lib::adhoc::connection::transmitter::{TxHost, TxState};
    use crate::lib::adhoc::connection::receiver::Rx;
    use crate::lib::adhoc::connection::transmitter::Tx;

    /// Packs of id 9 with a 4-byte value; what it got and what it was told.
    #[derive(Default)]
    struct Host {
        got: Vec<u32>,
        cur: u32,
        abandoned: usize,
        lost: usize,
    }

    impl RxHost for Host {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            id == 9
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            if !src.try_get4(4, 1) {
                return false;
            }
            self.cur = src.u4;
            true
        }
        fn on_received(&mut self, st: &mut RxState) {
            self.got.push(self.cur);
        }
        fn on_abandon(&mut self) {
            self.abandoned += 1;
        }
        fn on_lost(&mut self) {
            self.lost += 1;
        }
    }

    const PACK: [u8; 5] = [9, 0x44, 0x33, 0x22, 0x11];

    /// A stream that goes on with no position named drops the pack in flight; the session hears
    /// nothing.
    #[test]
    fn plain_abandon_drops_the_pack_in_flight() {
        let mut s = crate::lib::adhoc::connection::receiver::Receiver::new(Host::default(), 1);
        assert!(s.on_bytes(&PACK[..3]));
        BytesDst::abandon(&mut s);
        assert!(s.on_bytes(&PACK));
        assert_eq!((s.host().got.as_slice(), s.host().abandoned, s.host().lost), (&[0x1122_3344][..], 1, 0));
    }

    /// The peer's journal jumped: the pack in flight is dropped and the session hears it.
    #[test]
    fn plain_lost_drops_the_pack_and_tells() {
        let mut s = crate::lib::adhoc::connection::receiver::Receiver::new(Host::default(), 1);
        assert!(s.on_bytes(&PACK[..2]));
        s.lost(0);
        assert!(s.on_bytes(&PACK));
        assert_eq!((s.host().got.as_slice(), s.host().abandoned, s.host().lost), (&[0x1122_3344][..], 1, 1));
    }

    /// Packs of id 9: a File field of at most 10 bytes.
    struct Capped;
    impl RxHost for Capped {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            id == 9
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            src.get_file(|_| Box::new(std::io::sink()), 10, 0)
        }
        fn on_received(&mut self, _st: &mut RxState) {}
    }

    /// A refusal for an unknown pack id is a loss; one for a pack the sink could not parse - its
    /// length over the cap - broke the stream for good.
    #[test]
    fn plain_broken_only_for_a_pack_it_could_not_parse() {
        let mut s = crate::lib::adhoc::connection::receiver::Receiver::new(Capped, 1);
        assert!(!s.on_bytes(&[7, 0]), "an unknown id");
        assert!(!BytesDst::broken(&s), "an unknown id is a loss");
        let mut s = crate::lib::adhoc::connection::receiver::Receiver::new(Capped, 1);
        assert!(!s.on_bytes(&[9, 11]), "11 bytes over the cap of 10");
        assert!(BytesDst::broken(&s));
    }

    /// Packs of id 9: a File field into a sink that fails after its first write.
    struct Failing(usize);
    struct FailingSink(bool);
    impl std::io::Write for FailingSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::other("the disk is full"));
            }
            self.0 = true;
            Ok(b.len().min(1))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl RxHost for Failing {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            id == 9
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            src.get_file(|_| Box::new(FailingSink(false)), 100, 0)
        }
        fn on_received(&mut self, _st: &mut RxState) {
            self.0 += 1;
        }
    }

    /// The sink of the application fails in the middle of a file: it is told, the rest of the file
    /// is read unseen, and the stream goes on with the next pack.
    #[test]
    fn a_failing_sink_loses_its_file_not_the_stream() {
        crate::lib::adhoc::connection::receiver::on_error::heard::on();
        let mut s = crate::lib::adhoc::connection::receiver::Receiver::new(Failing(0), 1);
        assert!(s.on_bytes(&[9, 5, 1, 2, 3, 4, 5, 9, 2, 6, 7]));
        assert!(!BytesDst::broken(&s));
        assert_eq!(s.host().0, 2, "both packs received");
        assert_eq!(crate::lib::adhoc::connection::receiver::on_error::heard::take(), vec![crate::lib::adhoc::connection::receiver::on_error::RxOnError::Error; 2]);
    }

    struct Out(usize);
    impl TxHost for Out {
        fn on_serializing(&mut self) -> bool {
            if self.0 == 0 {
                return false;
            }
            self.0 -= 1;
            true
        }
        fn get_bytes(&mut self, dst: &mut Tx<'_>) -> bool {
            loop {
                match dst.state() {
                    0 => {
                        if !dst.put_val(9, 1, 1) {
                            return false;
                        }
                        dst.set_state(1);
                    }
                    1 => return dst.put_val(0x1122_3344, 4, 2),
                    _ => return true,
                }
            }
        }
        fn on_serialized(&mut self) {}
    }

    /// A frame of the AdHoc framing cut by a jump of the stream: the frame and its pack are
    /// dropped, the next frame is whole.
    #[test]
    fn framed_lost_drops_the_frame() {
        let mut st = TxState::new();
        let mut fr = crate::lib::adhoc::connection::transmitter::framing::State::new();
        let mut out = Out(2);
        let mut wire = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = fr.read(&mut buf, &mut st, &mut out);
            if n < 0 {
                break;
            }
            wire.extend_from_slice(&buf[..n as usize]);
        }
        let mut s = Framing::new(Host::default(), 1);
        assert!(s.on_bytes(&wire[..3]));
        s.lost(0);
        let mut rest = wire[3..].to_vec(); // the rest of the first frame, then the second: the next frame starts at its mark
        assert!(s.on_bytes_mut(&mut rest));
        assert_eq!((s.host.got.as_slice(), s.host.abandoned, s.host.lost), (&[0x1122_3344][..], 1, 1));
    }

    /// The frames go to another upper layer: the half of a frame the first one had is no one's, the
    /// next whole frame is the new one's alone.
    #[test]
    fn framed_switch_to_another_upper_layer() {
        let mut st = TxState::new();
        let mut fr = crate::lib::adhoc::connection::transmitter::framing::State::new();
        let mut out = Out(2);
        let mut wire = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = fr.read(&mut buf, &mut st, &mut out);
            if n < 0 {
                break;
            }
            wire.extend_from_slice(&buf[..n as usize]);
        }
        let mut s = Framing::new(Host::default(), 1);
        assert!(s.on_bytes(&wire[..3]));
        let before = s.switch_to(Host::default());
        assert!(before.got.is_empty());
        let mut rest = wire[3..].to_vec();
        assert!(s.on_bytes_mut(&mut rest));
        assert_eq!(s.host.got.as_slice(), &[0x1122_3344][..], "the next whole frame, to the new layer");
    }
}
