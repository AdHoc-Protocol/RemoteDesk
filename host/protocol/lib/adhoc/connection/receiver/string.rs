// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Incremental decoding from varint-encoded UTF-16 units into UTF-8.
//!
//! Length-prefixed strings begin with a code-unit count; chain text streams omit that prefix. ASCII
//! runs are copied directly, and character batches use the fast paths in
//! [`crate::lib::adhoc::ascii`]. State retains incomplete units and surrogate pairs between input
//! windows.

use super::*;

/// How many strings given up by collections a receiver keeps.
const SPARES_MAX: usize = 256;
/// The longest memory of such a string kept.
const SPARE_BYTES_MAX: usize = 256;

impl RxState {
    /// The received string, taken (`get_string`).
    #[inline]
    pub fn get_string(&mut self) -> Option<String> {
        self.str.take()
    }

    /// The received string into the field `dst`: the string it held gives its memory to the next
    /// one received (a field received again and again allocates nothing once the sizes settle).
    #[inline]
    pub fn get_string_into(&mut self, dst: &mut Option<String>) {
        if let Some(old) = std::mem::replace(dst, self.str.take()) {
            if self.spare.capacity() < old.capacity() {
                self.spare = old.into_bytes();
            }
        }
    }

    /// The received string into the storage of a field that is not null (a pack's own `_new()`, an
    /// item of a collection): as [`get_string_into`](Self::get_string_into), the string it held gives
    /// its memory to the next one received.
    #[inline]
    pub fn get_string_to(&mut self, dst: &mut String) {
        if let Some(s) = self.str.take() {
            let old = std::mem::replace(dst, s);
            if self.spare.capacity() < old.capacity() {
                self.spare = old.into_bytes();
            }
        }
    }

    /// A string a collection gave up when received again: its memory for the strings to come in this pack - what it does
    /// not take goes when it ends ( [`drop_spares`](Self::drop_spares) ). No more than [`SPARES_MAX`] are kept, none
    /// above [`SPARE_BYTES_MAX`]: the rest are dropped.
    #[inline]
    pub fn give_string(&mut self, s: String) {
        if self.spares.len() < SPARES_MAX && s.capacity() <= SPARE_BYTES_MAX {
            self.spares.push(s.into_bytes());
        }
    }

    /// The memory of the strings collections gave up and the pack did not take: freed when the pack ends.
    #[inline]
    pub(super) fn drop_spares(&mut self) {
        if !self.spares.is_empty() {
            self.spares = Vec::new();
        }
    }

    /// Memory for the next string: the one a field gave up or the last a collection gave up, emptied.
    #[inline]
    pub(super) fn spare_bytes(&mut self) -> Vec<u8> {
        let mut v = std::mem::take(&mut self.spare);
        if v.capacity() == 0 {
            v = self.spares.pop().unwrap_or_default();
        }
        v.clear();
        v
    }
}

impl<'a> Rx<'a> {
    /// A string — its length, then its UTF-16 units, each a varint — into the receiver
    /// (`try_get_string`).
    pub fn try_get_string(&mut self, max_chars: u32, get_string_case: u32) -> bool {
        // The common case first: the length in one byte, every character ASCII and in the window -
        // one write of the state, the string; the counters below are for a string cut by the
        // window, every path that reads them sets them first.
        let p = self.byte_;
        if p < self.byte_max {
            // the length a varint of one byte, or of two ( up to 16383 characters )
            let b0 = self.buffer[p] as usize;
            let (len, h) = if b0 < 0x80 {
                (b0, 1)
            } else if p + 1 < self.byte_max && self.buffer[p + 1] < 0x80 {
                (b0 & 0x7F | (self.buffer[p + 1] as usize) << 7, 2)
            } else {
                (usize::MAX, 0)
            };
            if len as u64 <= max_chars as u64 && p + h + len <= self.byte_max {
                let src = &self.buffer[p + h..p + h + len];
                if crate::lib::adhoc::ascii::prefix(src) == len {
                    let mut v = self.st.spare_bytes();
                    v.extend_from_slice(src);
                    // SAFETY: every byte is ASCII
                    self.st.str = Some(unsafe { String::from_utf8_unchecked(v) });
                    self.byte_ = p + h + len;
                    return true;
                }
            }
        }
        self.st.u4 = max_chars;
        self.st.u8_ = u64::MAX; // the length is not received yet
        self.st.u8 = 0;
        self.st.bytes_left = 0;
        if self.varint() {
            match self.check_length_and_getting_string() {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) => {
                    self.st.error = Some(e);
                    return false;
                }
            }
        }
        self.slot().state = get_string_case;
        self.st.mode = mode::STR;
        false
    }

    pub(super) fn check_length_and_getting_string(&mut self) -> Result<bool, RxError> {
        if (self.st.u4 as u64) < self.st.u8 {
            return Err(RxError::Overflow {
                got: self.st.u8,
                max: self.st.u4 as u64,
            });
        }
        self.st.u8_ = self.st.u8; // the length
        self.st.u4 = 0; // characters received
        self.st.u8 = 0; // a character cut by a window
        self.st.bytes_left = 0;
        self.st.wide = false;
        self.st.bys.clear();
        self.st.hi = 0;
        Ok(self.getting_string())
    }

    /// Goes on with the characters of the string: taken for ASCII until a byte above 0x7F.
    pub(super) fn getting_string(&mut self) -> bool {
        let len = self.st.u8_ as usize;
        if !self.st.wide {
            let from = self.st.u4 as usize;
            let n = self.remaining().min(len - from);
            let src = &self.buffer[self.byte_..self.byte_ + n];
            let ascii = crate::lib::adhoc::ascii::prefix(src);
            match if ascii == n { None } else { Some(ascii) } {
                None => {
                    if n == len {
                        // the whole string in this window
                        let mut v = self.st.spare_bytes();
                        v.extend_from_slice(src);
                        // SAFETY: every byte is ASCII
                        self.st.str = Some(unsafe { String::from_utf8_unchecked(v) });
                        self.byte_ += n;
                        self.st.u4 = len as u32;
                        return true;
                    }
                    if from == 0 {
                        self.st.bys.reserve(len); // one allocation for a string cut by windows
                    }
                    self.st.bys.extend_from_slice(src);
                    self.byte_ += n;
                    self.st.u4 = (from + n) as u32;
                    if from + n < len {
                        return false;
                    }
                    let spare = self.st.spare_bytes();
                    let bys = std::mem::replace(&mut self.st.bys, spare);
                    // SAFETY: every byte is ASCII
                    self.st.str = Some(unsafe { String::from_utf8_unchecked(bys) });
                    return true;
                }
                Some(at) => {
                    // the ASCII before `at` is UTF-8 already: the rest is decoded straight into
                    // UTF-8 too
                    let st = &mut *self.st;
                    // a byte a character at least - the string is no shorter; the kernels reserve
                    // the rest a window at a time, by the bytes in it: the string handed over does
                    // not hold three times its length, a length declared and not sent does not take
                    // memory
                    st.bys.reserve(len - from);
                    st.bys.extend_from_slice(&src[..at]);
                    self.byte_ += at;
                    st.u4 = (from + at) as u32;
                    st.wide = true;
                }
            }
        }
        self.getting_chars(len)
    }

    /// The characters of the string, each a varint, until `target` of them (`getting_chars`).
    /// Decodes straight into UTF-8: runs of ASCII are copied whole, every other unit is a varint of
    /// 2-3 bytes; a surrogate pair is one character, an unpaired surrogate is U+FFFD (as
    /// `String::from_utf16_lossy` makes it).
    pub(super) fn getting_chars(&mut self, target: usize) -> bool {
        let st = &mut *self.st;
        let mut pos = self.byte_;
        let limit = self.byte_max;
        let mut count = st.u4 as usize;
        let mut val = st.u8;
        let mut shift = st.bytes_left;
        while count < target && pos < limit {
            if shift == 0 && st.hi == 0 {
                let run = crate::lib::adhoc::ascii::prefix(&self.buffer[pos..limit.min(pos + (target - count))]);
                if 0 < run {
                    st.bys.extend_from_slice(&self.buffer[pos..pos + run]);
                    pos += run;
                    count += run;
                    continue;
                }
                // a run of units below 0x800, ASCII among them: 8 bytes a step, each the same
                // length in varints and in UTF-8
                let (n, units) = crate::lib::adhoc::ascii::varints_to_short(&self.buffer[pos..limit], target - count, &mut st.bys);
                if 0 < n {
                    pos += n;
                    count += units;
                    continue;
                }
                // a run of units U+0800..=U+FFFF: two or three bytes each in varints, three in
                // UTF-8
                let (n, units) = crate::lib::adhoc::ascii::varints_to_three(&self.buffer[pos..limit], target - count, &mut st.bys);
                if 0 < n {
                    pos += n;
                    count += units;
                    continue;
                }
            }
            let b = self.buffer[pos];
            pos += 1;
            if 0x80 <= b {
                if shift < crate::lib::adhoc::varint_text::UNIT_SHIFT_MAX {
                    val |= ((b & 0x7F) as u64) << shift;
                    shift += 7;
                } else {
                    // past three bytes: the byte that ends the unit makes U+FFFD
                    shift = crate::lib::adhoc::varint_text::UNIT_BROKEN;
                }
                continue;
            }
            let unit = crate::lib::adhoc::varint_text::end_unit(val as u32, b, shift);
            count += 1;
            val = 0;
            shift = 0;
            crate::lib::adhoc::ascii::push_unit(&mut st.bys, &mut st.hi, unit);
        }
        self.byte_ = pos;
        st.u4 = count as u32;
        if count < target {
            st.u8 = val;
            st.bytes_left = shift;
            return false;
        }
        crate::lib::adhoc::ascii::end_units(&mut st.bys, &mut st.hi);
        let spare = st.spare_bytes();
        let bys = std::mem::replace(&mut st.bys, spare);
        // SAFETY: only whole characters were written
        st.str = Some(unsafe { String::from_utf8_unchecked(bys) });
        true
    }

    /// A string that passes the chain `chain`: prefix-less — UTF-16 units as varints, or UTF-8
    /// bytes (`utf`) — up to the terminator of the stream. Then the string is at
    /// [`get_string`](RxState::get_string), as after [`try_get_string`](Self::try_get_string).
    pub fn get_string_chain(&mut self, chain: u32, utf: bool, max_chars: u32, this_case: u32) -> bool {
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
            ch.arm((self.st.stages)(chain));
            ch.text.arm(utf, max_chars);
        }
        let done = {
            let RxChain { stages, frame, text, .. } = &mut ch;
            self.chain_body(stages, frame, &mut |r: &mut Rx<'_>| str_sink(r, text))
        };
        if done {
            self.st.str = Some(ch.text.finish());
        }
        self.st.chains[level] = ch;
        self.st.level = parent;
        if done && self.st.error.is_none() {
            self.st.depth = level;
            return true;
        }
        self.st.slots[parent].state = this_case;
        self.st.mode = mode::RETRY;
        false
    }
}

/// The string being received through a chain.
#[derive(Default)]
pub(crate) struct RxText {
    pub(crate) utf: bool,
    pub(crate) max_chars: u32,
    /// A varint unit in part: its bits so far and their shift.
    acc: u32,
    shift: u8,
    /// The units received, for the limit.
    units: usize,
    /// A high surrogate waiting for its low one.
    hi: u16,
    /// The string, UTF-8 already: the units are decoded into it as they come.
    bytes: Vec<u8>,
}

impl RxText {
    pub(crate) fn arm(&mut self, utf: bool, max_chars: u32) {
        self.utf = utf;
        self.max_chars = max_chars;
        self.acc = 0;
        self.shift = 0;
        self.units = 0;
        self.hi = 0;
        self.bytes.clear();
    }

    /// The end of the stream: the string. What is wrong with it is told to the handler of the
    /// errors and mended, the stream goes on: broken UTF-8 becomes U+FFFD, the bits of a character
    /// the end cut are dropped, characters above the limit are kept.
    pub(crate) fn finish(&mut self) -> String {
        use crate::lib::adhoc::connection::receiver::on_error::{rx, RxOnError};
        let s = if self.utf {
            // the bytes are taken, not copied; only broken UTF-8 is mended
            match String::from_utf8(std::mem::take(&mut self.bytes)) {
                Ok(s) => s,
                Err(e) => {
                    rx(RxOnError::BytesDistortion, &"malformed UTF-8 in a string: mended with U+FFFD");
                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                }
            }
        } else {
            if self.shift != 0 {
                rx(RxOnError::BytesDistortion, &"the stream of a string ends in the middle of a character: its bits are dropped");
                self.shift = 0;
                self.acc = 0;
            }
            crate::lib::adhoc::ascii::end_units(&mut self.bytes, &mut self.hi);
            // SAFETY: only whole characters were written
            unsafe { String::from_utf8_unchecked(std::mem::take(&mut self.bytes)) }
        };
        let chars = if self.utf { crate::lib::adhoc::ascii::utf16_len(&s) } else { self.units };
        if self.max_chars < chars as u32 {
            rx(RxOnError::Overflow, &format_args!("a string of {chars} characters, {} at most: kept", self.max_chars));
        }
        s
    }
}

/// The leaf of a string being received: the window into the string, whole.
pub(crate) fn str_sink(src: &mut Rx<'_>, t: &mut RxText) {
    let (from, to) = (src.byte_, src.byte_max);
    if t.utf {
        t.bytes.extend_from_slice(&src.buffer[from..to]);
    } else {
        let b = &src.buffer[from..to];
        let mut i = 0;
        while i < b.len() {
            if t.shift == 0 && t.hi == 0 {
                // a run of ASCII: each byte a whole unit, copied as it is
                let run = crate::lib::adhoc::ascii::prefix(&b[i..]);
                if 0 < run {
                    t.bytes.extend_from_slice(&b[i..i + run]);
                    t.units += run;
                    i += run;
                    continue;
                }
                // a run of units below 0x800, ASCII among them: 8 bytes a step, each the same
                // length in varints and in UTF-8
                let (n, units) = crate::lib::adhoc::ascii::varints_to_short(&b[i..], usize::MAX, &mut t.bytes);
                if 0 < n {
                    t.units += units;
                    i += n;
                    continue;
                }
            }
            let x = b[i];
            i += 1;
            if x & 0x80 != 0 {
                if (t.shift as i32) < crate::lib::adhoc::varint_text::UNIT_SHIFT_MAX {
                    t.acc |= ((x & 0x7F) as u32) << t.shift;
                    t.shift += 7;
                } else {
                    // past three bytes: the byte that ends the unit makes U+FFFD
                    t.shift = crate::lib::adhoc::varint_text::UNIT_BROKEN as u8;
                }
                continue;
            }
            let unit = crate::lib::adhoc::varint_text::end_unit(t.acc, x, t.shift as i32);
            crate::lib::adhoc::ascii::push_unit(&mut t.bytes, &mut t.hi, unit);
            t.units += 1;
            t.acc = 0;
            t.shift = 0;
        }
    }
    src.byte_ = to; // over the limit the characters are kept, finish tells it
}

#[cfg(test)]
mod text_tests {
    use super::*;
    use crate::lib::adhoc::connection::receiver::on_error::{heard, RxOnError};

    /// A string the end cut in the middle of a character, longer than its limit: told, mended, kept
    /// - the stream goes on.
    #[test]
    fn a_broken_string_is_told_and_mended() {
        heard::on();
        let mut t = RxText::default();
        t.arm(false, 2);
        t.bytes.extend_from_slice(b"abc");
        t.units = 3;
        t.shift = 7;
        assert_eq!(t.finish(), "abc");
        t.arm(true, 10);
        t.bytes.extend_from_slice(&[b'a', 0xC3]);
        assert_eq!(t.finish(), "a�");
        assert_eq!(heard::take(), vec![RxOnError::BytesDistortion, RxOnError::Overflow, RxOnError::BytesDistortion]);
    }
    /// A unit of more than three varint bytes, or above U+FFFF, through the leaf: told, U+FFFD, one
    /// character as the bytes that end units count them - no shift past 14.
    #[test]
    fn a_unit_past_three_bytes_is_told_and_mended() {
        heard::on();
        let wire = [b'a', 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, b'b', 0xFF, 0xFF, 0x04, b'c'];
        let mut st = RxState::new(1);
        let mut rx = Rx::new(&wire, &mut st);
        let mut t = RxText::default();
        t.arm(false, 100);
        str_sink(&mut rx, &mut t);
        assert!(rx.error.is_none(), "the stream goes on");
        assert_eq!(t.finish(), "a\u{FFFD}b\u{FFFD}c");
        assert_eq!(t.units, 5);
        assert_eq!(heard::take(), vec![RxOnError::BytesDistortion, RxOnError::BytesDistortion]);
    }

    /// A string of varint units over its limit, through the leaf: received whole, the stream not
    /// broken - the end tells the overflow.
    #[test]
    fn a_string_over_its_limit_is_kept_and_told() {
        heard::on();
        let mut wire = Vec::new();
        for _ in 0..5 {
            wire.extend_from_slice(&[0xE5, 0xCB, 0x01]); // U+65E5 as a varint
        }
        let mut st = RxState::new(1);
        let mut rx = Rx::new(&wire, &mut st);
        let mut t = RxText::default();
        t.arm(false, 2);
        str_sink(&mut rx, &mut t);
        assert!(rx.error.is_none(), "the stream goes on");
        assert_eq!(rx.byte_, wire.len());
        assert_eq!(t.finish(), "日日日日日");
        assert_eq!(heard::take(), vec![RxOnError::Overflow]);
    }

    /// Unpaired UTF-16 surrogates decode as U+FFFD; a valid pair decodes as one Unicode scalar.
    /// Exercise both field decoding across input windows and text decoding at a chain leaf.
    #[test]
    fn unpaired_surrogates_become_fffd() {
        let units: [u16; 12] = [0x61, 0xD800, 0x62, 0xDC00, 0xD83D, 0xDE00, 0x5E9, 0xD800, 0xD800, 0xDC01, 0x65E5, 0xDBFF];
        let want = "a\u{FFFD}b\u{FFFD}😀ש\u{FFFD}\u{10001}日\u{FFFD}";
        let mut body = Vec::new();
        for &u in &units {
            let mut v = u as u32;
            while 0x7F < v {
                body.push(v as u8 | 0x80);
                v >>= 7;
            }
            body.push(v as u8);
        }
        let mut wire = vec![units.len() as u8];
        wire.extend_from_slice(&body);
        for size in 1..=wire.len() {
            let mut st = RxState::new(1);
            let mut done = false;
            for (i, w) in wire.chunks(size).enumerate() {
                let mut rx = Rx::new(w, &mut st);
                done = if i == 0 { rx.try_get_string(100, 1) } else { rx.getting_string() };
                assert!(rx.error.is_none());
            }
            assert!(done, "window {size}");
            assert_eq!(st.get_string().as_deref(), Some(want), "window {size}");
        }
        let mut st = RxState::new(1);
        let mut rx = Rx::new(&body, &mut st);
        let mut t = RxText::default();
        t.arm(false, 100);
        str_sink(&mut rx, &mut t);
        assert_eq!(t.finish(), want);
    }

    /// The path of a string whole in the window and the one of a string cut by it agree: ASCII and
    /// not, lengths around the one-byte length, windows of every size; two strings in a row - the
    /// state the first leaves does not spoil the second.
    #[test]
    fn whole_and_cut_strings_agree() {
        for len in [0usize, 1, 5, 126, 127, 128, 200] {
            for wide in [false, true] {
                let want: String = (0..len).map(|i| if wide && i == len / 2 { 'ж' } else { (b'a' + (i % 26) as u8) as char }).collect();
                let mut wire = Vec::new();
                for _ in 0..2 {
                    let mut n = want.encode_utf16().count() as u32;
                    while 0x7F < n {
                        wire.push(n as u8 | 0x80);
                        n >>= 7;
                    }
                    wire.push(n as u8);
                    for u in want.encode_utf16() {
                        let mut v = u as u32;
                        while 0x7F < v {
                            wire.push(v as u8 | 0x80);
                            v >>= 7;
                        }
                        wire.push(v as u8);
                    }
                }
                for size in 1..=wire.len() {
                    let mut st = RxState::new(1);
                    let mut got = Vec::new();
                    let mut pending = false;
                    for w in wire.chunks(size) {
                        let mut rx = Rx::new(w, &mut st);
                        if pending {
                            if rx.resume() != 1 {
                                continue;
                            }
                            pending = false;
                            got.push(rx.get_string().unwrap());
                        }
                        while rx.byte_ < rx.byte_max {
                            if !rx.try_get_string(1000, 1) {
                                pending = true;
                                break;
                            }
                            got.push(rx.get_string().unwrap());
                        }
                    }
                    assert_eq!(got, vec![want.clone(), want.clone()], "len {len} wide {wide} window {size}");
                }
            }
        }
    }

    /// A varint whole in the window and one cut by it agree, 1 to 10 bytes.
    #[test]
    fn whole_and_cut_varints_agree() {
        for v in [0u64, 1, 127, 128, 16_383, 16_384, 1 << 35, u64::MAX >> 1, u64::MAX] {
            let mut wire = Vec::new();
            for _ in 0..2 {
                let mut x = v;
                while 0x7F < x {
                    wire.push(x as u8 | 0x80);
                    x >>= 7;
                }
                wire.push(x as u8);
            }
            for size in 1..=wire.len() {
                let mut st = RxState::new(1);
                let mut got = Vec::new();
                let mut started = false;
                for w in wire.chunks(size) {
                    let mut rx = Rx::new(w, &mut st);
                    while rx.byte_ < rx.byte_max {
                        let done = if started { rx.varint() } else { rx.try_get_varint(1) };
                        started = !done;
                        if done {
                            got.push(rx.u8);
                        } else {
                            break;
                        }
                    }
                }
                assert_eq!(got, vec![v, v], "value {v} window {size}");
            }
        }
    }
}
