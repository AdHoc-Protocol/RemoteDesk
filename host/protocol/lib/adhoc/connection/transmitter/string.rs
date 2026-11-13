// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Strings sent: the count of the UTF-16 units, then each unit a varint, straight from the UTF-8 of
//! the string - runs of ASCII copied whole, runs of shorter characters a vector at a time
//! ([`crate::lib::adhoc::ascii`]).

use super::*;

impl Tx<'_> {
    /// Checks the encoded string length in UTF-16 code units.
    ///
    /// # Panics
    ///
    /// Panics when the string exceeds the protocol limit `max`.
    #[inline]
    pub fn check_length(src: &str, max: u32) {
        let units = crate::lib::adhoc::ascii::utf16_len_ascii(src).0;
        if (max as usize) < units {
            overflow("a string", units as u64, max as u64);
        }
    }

    /// A string: the count of its UTF-16 units, then each unit a varint (`put(string, uint)`). The
    /// buffer full — false, and the pack enters `this_case` again: the string goes on from where it
    /// stopped.
    pub fn put_string(&mut self, src: &str, max: u32, this_case: u32) -> bool {
        let b = src.as_bytes();
        let (n, at) = (b.len(), self.byte_);
        // the common case first: a short ASCII string whole in the buffer - its length is one byte,
        // each of its bytes the varint of its unit; no state, one copy
        if self.st.str_phase == 0 && n < 0x80 && n < self.byte_max - at && crate::lib::adhoc::ascii::prefix(b) == n {
            if (max as usize) < n {
                overflow("a string", n as u64, max as u64);
            }
            debug_assert!(at + 1 + n <= self.byte_max && self.byte_max <= self.buffer.len());
            // SAFETY: `at + 1 + n <= byte_max <= buffer.len()` ( `put_raw` ); `b` is not in `buffer`
            unsafe {
                let p = self.buffer.as_mut_ptr().add(at);
                *p = n as u8;
                std::ptr::copy_nonoverlapping(b.as_ptr(), p.add(1), n);
            }
            self.byte_ = at + 1 + n;
            return true;
        }
        self.put_string_rest(src, max, this_case)
    }

    /// [`put_string`](Self::put_string) of a string that is long, not ASCII, or cut by the buffer.
    #[inline(never)]
    fn put_string_rest(&mut self, src: &str, max: u32, this_case: u32) -> bool {
        if self.st.str_phase == 0 {
            let (units, ascii) = crate::lib::adhoc::ascii::utf16_len_ascii(src);
            self.st.str_ascii = ascii;
            if ( max as usize ) < units {
                overflow("a string", units as u64, max as u64); // above the limit of the description: nothing of it goes
            }
            self.st.str_pos = 0;
            self.st.str_low = 0;
            self.st.str_partial = false;
            if !self.varint(units as u64) {
                self.st.str_phase = 1;
                self.retry_at(this_case);
                return false;
            }
            self.st.str_phase = 2;
        } else if self.st.str_phase == 1 {
            let rest = self.st.u8_;
            if !self.varint(rest) {
                self.retry_at(this_case);
                return false;
            }
            self.st.str_phase = 2;
        }
        if self.put_chars(src) {
            self.st.str_phase = 0;
            return true;
        }
        self.retry_at(this_case);
        false
    }

    /// The characters of the string from where it stopped.
    pub(super) fn put_chars(&mut self, src: &str) -> bool {
        if self.st.str_partial {
            let rest = self.st.u8_;
            if !self.varint(rest) {
                return false;
            }
            self.st.str_partial = false;
        }
        if self.st.str_low != 0 {
            let low = self.st.str_low;
            self.st.str_low = 0;
            if !self.put_unit(low) {
                return false;
            }
        }
        let bytes = src.as_bytes();
        let mut pos = self.st.str_pos;
        while pos < bytes.len() {
            // a run of ASCII: each byte is its own varint
            let room = self.byte_max - self.byte_;
            if room == 0 {
                self.st.str_pos = pos;
                return false;
            }
            // looked for only as far as the buffer takes: a long string goes out in many buffers,
            // and a search to its end in each of them is quadratic
            let lim = (pos + room).min(bytes.len());
            let run_end = if pos < self.st.str_ascii { self.st.str_ascii.min(lim) } else { pos + crate::lib::adhoc::ascii::prefix(&bytes[pos..lim]) };
            if pos < run_end {
                let n = (run_end - pos).min(room);
                self.buffer[self.byte_..self.byte_ + n].copy_from_slice(&bytes[pos..pos + n]);
                self.byte_ += n;
                pos += n;
                continue;
            }
            if bytes[pos] & 0xE0 == 0xC0 {
                // a run of characters below U+0800, ASCII among them: 8 bytes a step, each the same
                // length in varints
                let n = crate::lib::adhoc::ascii::short_to_varints(&bytes[pos..], &mut self.buffer[self.byte_..self.byte_max]);
                if 0 < n {
                    self.byte_ += n;
                    pos += n;
                    continue;
                }
            }
            if bytes[pos] & 0xF0 == 0xE0 {
                // a run of characters U+0800..=U+FFFF: three bytes each in UTF-8, two or three in
                // varints
                let (n, w) = crate::lib::adhoc::ascii::three_to_varints(&bytes[pos..], &mut self.buffer[self.byte_..self.byte_max]);
                if 0 < n {
                    self.byte_ += w;
                    pos += n;
                    continue;
                }
            }
            if 6 <= room {
                // a character above 0x7F straight from its UTF-8 to the varints of its UTF-16
                // units: 2..=6 bytes
                let at = self.byte_;
                let b0 = bytes[pos] as u32;
                let w = if b0 < 0xE0 {
                    let cp = (b0 & 0x1F) << 6 | (bytes[pos + 1] as u32 & 0x3F); // 0x80..=0x7FF: two bytes
                    self.buffer[at] = 0x80 | cp as u8;
                    self.buffer[at + 1] = (cp >> 7) as u8;
                    pos += 2;
                    2
                } else if b0 < 0xF0 {
                    let cp = (b0 & 0x0F) << 12 | (bytes[pos + 1] as u32 & 0x3F) << 6 | (bytes[pos + 2] as u32 & 0x3F);
                    pos += 3;
                    unit_varint(&mut self.buffer[at..], cp)
                } else {
                    let cp = ((b0 & 0x07) << 18 | (bytes[pos + 1] as u32 & 0x3F) << 12 | (bytes[pos + 2] as u32 & 0x3F) << 6 | (bytes[pos + 3] as u32 & 0x3F)) - 0x1_0000;
                    pos += 4;
                    let n = unit_varint(&mut self.buffer[at..], 0xD800 + (cp >> 10));
                    n + unit_varint(&mut self.buffer[at + n..], 0xDC00 + (cp & 0x3FF))
                };
                self.byte_ = at + w;
                continue;
            }
            // a character above 0x7F: one or two UTF-16 units SAFETY: `pos` is at the start of a
            // character - runs of ASCII and whole characters are stepped over
            let ch = unsafe { src.get_unchecked(pos..) }.chars().next().unwrap();
            pos += ch.len_utf8();
            let mut units = [0u16; 2];
            let units = ch.encode_utf16(&mut units);
            if units.len() == 2 {
                self.st.str_low = units[1];
            }
            self.st.str_pos = pos;
            if !self.put_unit(units[0]) {
                return false;
            }
            if self.st.str_low != 0 {
                let low = self.st.str_low;
                self.st.str_low = 0;
                if !self.put_unit(low) {
                    return false;
                }
            }
        }
        self.st.str_pos = pos;
        true
    }

    #[inline]
    pub(super) fn put_unit(&mut self, unit: u16) -> bool {
        if self.varint(unit as u64) {
            return true;
        }
        self.st.str_partial = true;
        false
    }

    /// A string that passes the chain `chain`: prefix-less — its UTF-16 units as varints, or its
    /// UTF-8 bytes (`utf`: on the leg where the receiver takes raw bytes) — through the stages into
    /// chunks; the end of the stream ends it. `src` is the same string on every entry of
    /// `this_case`. Above `max` UTF-16 units — [`overflow`].
    pub fn put_string_chain(&mut self, chain: u32, src: &str, utf: bool, max: u32, this_case: u32) -> bool {
        let parent = self.st.level;
        let level = parent + 1;
        if self.st.chains.len() <= level {
            self.st.chains.resize_with(level + 1, TxChain::default);
        }
        let mut ch = std::mem::take(&mut self.st.chains[level]);
        if self.st.depth <= level {
            // a new stream
            let units = crate::lib::adhoc::ascii::utf16_len(src);
            if ( max as usize ) < units {
                overflow("a string", units as u64, max as u64);
            }
            if self.st.slots.len() <= level {
                self.st.slots.push(TxSlot::default());
            }
            self.st.depth = level + 1;
            ch.arm((self.st.stages)(chain));
        }
        let mut strs = self.st.strs.take();
        let done = {
            let TxChain { stages, terminator, str_pos, frag, on_frag, .. } = &mut ch;
            self.chain_body(stages, terminator, &mut |t: &mut Tx<'_>| {
                if !*on_frag {
                    if !str_leaf(t, src, utf, str_pos) {
                        return false;
                    }
                    let Some(next) = strs.as_mut() else { return true };
                    // the string of the field is out: the fragments after it, moved in
                    *on_frag = true;
                    *str_pos = 0;
                    match next(std::mem::take(frag)) {
                        Some(f) => *frag = f,
                        None => {
                            strs = None; // drained: the string ends here
                            return true;
                        }
                    }
                }
                loop {
                    if !str_leaf(t, frag, utf, str_pos) {
                        return false;
                    }
                    *str_pos = 0;
                    let mut spent = std::mem::take(frag);
                    spent.clear(); // its memory goes back to the supplier
                    match strs.as_mut().and_then(|next| next(spent)) {
                        Some(f) => *frag = f,
                        None => {
                            strs = None;
                            return true;
                        }
                    }
                }
            })
        };
        self.st.strs = strs;
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
}

/// The varint of a UTF-16 unit of 0x80 and up into `dst`: 2 bytes below 0x4000, else 3. Returns the
/// bytes written.
#[inline(always)]
pub(super) fn unit_varint(dst: &mut [u8], u: u32) -> usize {
    dst[0] = 0x80 | u as u8;
    if u < 0x4000 {
        dst[1] = (u >> 7) as u8;
        return 2;
    }
    dst[1] = 0x80 | (u >> 7) as u8;
    dst[2] = (u >> 14) as u8;
    3
}

/// The leaf of a string, prefix-less: its UTF-16 units, a varint each, or — on the leg where the
/// receiver takes raw bytes — its UTF-8 bytes. The end of the stream ends the string. `pos` — the
/// bytes of `s` already out. A character goes whole, its units together.
pub(crate) fn str_leaf(dst: &mut Tx<'_>, s: &str, utf: bool, pos: &mut usize) -> bool {
    let b = s.as_bytes();
    if utf {
        let n = (b.len() - *pos).min(dst.byte_max - dst.byte_);
        let at = dst.byte_;
        dst.buffer[at..at + n].copy_from_slice(&b[*pos..*pos + n]);
        dst.byte_ += n;
        *pos += n;
        return *pos == b.len();
    }
    #[inline]
    fn len(u: u16) -> usize {
        if u < 0x80 { 1 } else if u < 0x4000 { 2 } else { 3 }
    }
    while *pos < b.len() {
        // a run of ASCII, as far as the window takes: each byte its own varint
        let room = dst.byte_max - dst.byte_;
        let run = crate::lib::adhoc::ascii::prefix(&b[*pos..b.len().min(*pos + room)]);
        if 0 < run {
            let at = dst.byte_;
            dst.buffer[at..at + run].copy_from_slice(&b[*pos..*pos + run]);
            dst.byte_ += run;
            *pos += run;
            continue;
        }
        if b[*pos] & 0xE0 == 0xC0 {
            // a run of characters below U+0800, ASCII among them: 8 bytes a step, each the same
            // length in varints
            let n = crate::lib::adhoc::ascii::short_to_varints(&b[*pos..], &mut dst.buffer[dst.byte_..dst.byte_max]);
            if 0 < n {
                dst.byte_ += n;
                *pos += n;
                continue;
            }
        }
        if b[*pos] & 0xF0 == 0xE0 {
            // a run of characters U+0800..=U+FFFF: three bytes each in UTF-8, two or three in
            // varints
            let (n, w) = crate::lib::adhoc::ascii::three_to_varints(&b[*pos..], &mut dst.buffer[dst.byte_..dst.byte_max]);
            if 0 < n {
                dst.byte_ += w;
                *pos += n;
                continue;
            }
        }
        if room == 0 {
            return false;
        }
        let c = s[*pos..].chars().next().unwrap();
        let mut units = [0u16; 2];
        let units = c.encode_utf16(&mut units);
        let need: usize = units.iter().map(|&u| len(u)).sum();
        if dst.byte_max - dst.byte_ < need {
            return false;
        }
        for &u in units.iter() {
            let mut v = u as u32;
            while 0x7F < v {
                dst.buffer[dst.byte_] = (v as u8) | 0x80;
                dst.byte_ += 1;
                v >>= 7;
            }
            dst.buffer[dst.byte_] = v as u8;
            dst.byte_ += 1;
        }
        *pos += c.len_utf8();
    }
    true
}
