// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Protocol utility types, slice comparison and hashing, Base64 decoding, and byte search.

// OwnCell

/// What only the thread that runs a connection touches ( its loop - [`crate::lib::adhoc::connection::external::External::on_its_thread`] ):
/// an actor of the connection, the position of its receiver - lent with no lock, one thing at a time runs there. A lend from
/// inside a lend of the same cell ( a handler that reaches its own actor again ) panics: the value is lent already.
pub struct OwnCell<T> {
    v: std::cell::UnsafeCell<T>,
    lent: std::cell::Cell<bool>,
}

// SAFETY: the value moves between threads only with the connection ( `T: Send` ); it is touched only on the thread that runs
// the connection, or where nothing else runs it ( `lend` )
unsafe impl<T: Send> Sync for OwnCell<T> {}

impl<T> OwnCell<T> {
    pub const fn new(v: T) -> Self {
        Self {
            v: std::cell::UnsafeCell::new(v),
            lent: std::cell::Cell::new(false),
        }
    }

    /// The value, the cell gone: nothing lends it any more.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.v.into_inner()
    }

    /// The value is lent now: a lend runs further up the stack - asked on the thread that runs the connection.
    #[inline(always)]
    pub fn lent(&self) -> bool {
        self.lent.get()
    }

    /// The value, lent to `f`.
    ///
    /// # Safety
    ///
    /// On the thread that runs the connection, or where no thread runs it ( a parked session: its journal held, a session not
    /// seated yet ).
    #[inline(always)]
    pub unsafe fn lend<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        struct Back<'a>(&'a std::cell::Cell<bool>);
        impl Drop for Back<'_> {
            #[inline(always)]
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        assert!(!self.lent.replace(true), "the actor is lent already: reached again from inside its own handler");
        let _back = Back(&self.lent);
        f(&mut *self.v.get())
    }
}

impl<T: Default> Default for OwnCell<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

// NullableBool

#[derive(Clone, Copy, Debug, Hash)]
pub struct NullableBool {
    pub value: u8,
}

impl NullableBool {
    pub const NULL: u8 = 2;

    #[inline]
    pub fn new() -> Self {
        Self { value: Self::NULL }
    }

    #[inline]
    pub fn from_bool(v: bool) -> Self {
        Self { value: if v { 1 } else { 0 } }
    }

    #[inline]
    pub fn from_byte(v: u8) -> Self {
        Self { value: v }
    }

    #[inline]
    pub fn get_value(&self) -> bool {
        self.value == 1
    }

    #[inline]
    pub fn set_value(&mut self, v: bool) {
        self.value = if v { 1 } else { 0 };
    }

    #[inline]
    pub fn has_value(&self) -> bool {
        self.value != Self::NULL
    }

    #[inline]
    pub fn to_null(&mut self) {
        self.value = Self::NULL;
    }
}

impl Default for NullableBool {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for NullableBool {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for NullableBool {}

impl From<bool> for NullableBool {
    fn from(v: bool) -> Self {
        Self::from_bool(v)
    }
}

impl From<u8> for NullableBool {
    fn from(v: u8) -> Self {
        Self::from_byte(v)
    }
}

/// Holds `b`: a null one holds neither.
impl PartialEq<bool> for NullableBool {
    #[inline]
    fn eq(&self, b: &bool) -> bool {
        self.value == *b as u8
    }
}

impl PartialEq<NullableBool> for bool {
    #[inline]
    fn eq(&self, a: &NullableBool) -> bool {
        a == self
    }
}

/// `None` — null.
impl From<Option<bool>> for NullableBool {
    #[inline]
    fn from(v: Option<bool>) -> Self {
        v.map_or(Self::new(), Self::from_bool)
    }
}

impl From<NullableBool> for Option<bool> {
    #[inline]
    fn from(a: NullableBool) -> Self {
        a.has_value().then_some(a.value == 1)
    }
}

/// The byte: 0, 1, or [`NullableBool::NULL`].
impl From<NullableBool> for u8 {
    #[inline]
    fn from(a: NullableBool) -> Self {
        a.value
    }
}

// A large array of constant length in the heap

/// The items of `v` as an array of constant length in the heap: its bytes are never on the stack
/// (`Box::new([ … ])` builds the array there first). The generated code keeps an array of constant
/// length larger than 512 bytes this way.
#[inline]
pub fn boxed_array<T, const N: usize>(v: Vec<T>) -> Box<[T; N]> {
    match v.into_boxed_slice().try_into() {
        Ok(a) => a,
        Err(v) => panic!("an array of {N} items, not of {}", v.len()),
    }
}

/// An array of constant length in the heap, each item what `item` gives for its index.
#[inline]
pub fn boxed_array_with<T, const N: usize>(item: impl FnMut(usize) -> T) -> Box<[T; N]> {
    boxed_array((0..N).map(item).collect())
}

// ArrayEqualHash — sequence equality for slices

pub fn array_equal<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

pub fn array_hash<T: std::hash::Hash>(list: &[T]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for item in list {
        item.hash(&mut hasher);
    }
    hasher.finish()
}

// Base64 decode

static CHAR2BYTE: [u8; 256] = {
    let mut t = [0u8; 256]; // Bytes outside the Base64 alphabet map to zero.
                            // Build at const time is not trivial in Rust; use a runtime init approach via a function. For
                            // now, a lookup table:
    t[b'A' as usize] = 0;
    t[b'B' as usize] = 1;
    t[b'C' as usize] = 2;
    t[b'D' as usize] = 3;
    t[b'E' as usize] = 4;
    t[b'F' as usize] = 5;
    t[b'G' as usize] = 6;
    t[b'H' as usize] = 7;
    t[b'I' as usize] = 8;
    t[b'J' as usize] = 9;
    t[b'K' as usize] = 10;
    t[b'L' as usize] = 11;
    t[b'M' as usize] = 12;
    t[b'N' as usize] = 13;
    t[b'O' as usize] = 14;
    t[b'P' as usize] = 15;
    t[b'Q' as usize] = 16;
    t[b'R' as usize] = 17;
    t[b'S' as usize] = 18;
    t[b'T' as usize] = 19;
    t[b'U' as usize] = 20;
    t[b'V' as usize] = 21;
    t[b'W' as usize] = 22;
    t[b'X' as usize] = 23;
    t[b'Y' as usize] = 24;
    t[b'Z' as usize] = 25;
    t[b'a' as usize] = 26;
    t[b'b' as usize] = 27;
    t[b'c' as usize] = 28;
    t[b'd' as usize] = 29;
    t[b'e' as usize] = 30;
    t[b'f' as usize] = 31;
    t[b'g' as usize] = 32;
    t[b'h' as usize] = 33;
    t[b'i' as usize] = 34;
    t[b'j' as usize] = 35;
    t[b'k' as usize] = 36;
    t[b'l' as usize] = 37;
    t[b'm' as usize] = 38;
    t[b'n' as usize] = 39;
    t[b'o' as usize] = 40;
    t[b'p' as usize] = 41;
    t[b'q' as usize] = 42;
    t[b'r' as usize] = 43;
    t[b's' as usize] = 44;
    t[b't' as usize] = 45;
    t[b'u' as usize] = 46;
    t[b'v' as usize] = 47;
    t[b'w' as usize] = 48;
    t[b'x' as usize] = 49;
    t[b'y' as usize] = 50;
    t[b'z' as usize] = 51;
    t[b'0' as usize] = 52;
    t[b'1' as usize] = 53;
    t[b'2' as usize] = 54;
    t[b'3' as usize] = 55;
    t[b'4' as usize] = 56;
    t[b'5' as usize] = 57;
    t[b'6' as usize] = 58;
    t[b'7' as usize] = 59;
    t[b'8' as usize] = 60;
    t[b'9' as usize] = 61;
    t[b'+' as usize] = 62;
    t[b'/' as usize] = 63;
    t
};

/// Decodes `len` Base64 characters from `src_index` in place, writing at `dst_index`. Returns the
/// first output index after the decoded bytes.
pub fn base64decode(bytes: &mut [u8], mut src_index: usize, mut dst_index: usize, len: usize) -> usize {
    let mut max = src_index + len;
    while max > src_index && bytes[max - 1] == b'=' {
        max -= 1;
    }
    let new_len = max - src_index;
    let full_blocks = new_len >> 2;
    for _ in 0..full_blocks {
        let b = (CHAR2BYTE[bytes[src_index] as usize] as u32) << 18
            | (CHAR2BYTE[bytes[src_index + 1] as usize] as u32) << 12
            | (CHAR2BYTE[bytes[src_index + 2] as usize] as u32) << 6
            | (CHAR2BYTE[bytes[src_index + 3] as usize] as u32);
        src_index += 4;
        bytes[dst_index] = (b >> 16) as u8;
        bytes[dst_index + 1] = (b >> 8) as u8;
        bytes[dst_index + 2] = b as u8;
        dst_index += 3;
    }
    match new_len & 3 {
        3 => {
            let b = (CHAR2BYTE[bytes[src_index] as usize] as u32) << 12 | (CHAR2BYTE[bytes[src_index + 1] as usize] as u32) << 6 | (CHAR2BYTE[bytes[src_index + 2] as usize] as u32);
            bytes[dst_index] = (b >> 10) as u8;
            bytes[dst_index + 1] = (b >> 2) as u8;
            dst_index += 2;
        }
        2 => {
            bytes[dst_index] = (((CHAR2BYTE[bytes[src_index] as usize] as u32) << 6 | (CHAR2BYTE[bytes[src_index + 1] as usize] as u32)) >> 4) as u8;
            dst_index += 1;
        }
        _ => {}
    }
    dst_index
}

// Boyer-Moore string search

/// Builds a Boyer-Moore byte-pattern table for `src`.
pub fn boyer_moore_pattern(src: &[u8]) -> Vec<u32> {
    // Short patterns cost less to scan directly than to initialize the alphabet table.
    if src.len() <= 16 {
        let mut ret = vec![0u32; src.len()];
        for i in (0..src.len()).rev() {
            if ret[i] == 0 {
                let ch = src[i];
                let p = ((i as u32) << 8) | ch as u32;
                for ii in (0..=i).rev() {
                    if src[ii] == ch {
                        ret[ii] = p;
                    }
                }
            }
        }
        return ret;
    }
    let mut last = [0u32; 256];
    for (i, &ch) in src.iter().enumerate() {
        last[ch as usize] = ((i as u32) << 8) | ch as u32;
    }
    src.iter().map(|&ch| last[ch as usize]).collect()
}

/// Builds a search table from the low byte of each UTF-16 unit in `src`. For ASCII text these are
/// the original bytes; other units may have the same low byte. An entry is the index of the last
/// occurrence above the low byte: the whole unit there would run into the index.
pub fn boyer_moore_pattern_text(src: &str) -> Vec<u32> {
    if src.is_ascii() {
        return boyer_moore_pattern(src.as_bytes());
    }
    let units: Vec<u16> = src.encode_utf16().collect();
    let len = units.len();
    let mut ret = vec![0u32; len];
    for i in (0..len).rev() {
        if ret[i] == 0 {
            let ch = units[i];
            let p = ((i as u32) << 8) | (ch & 0xFF) as u32;
            for ii in (0..=i).rev() {
                if units[ii] == ch {
                    ret[ii] = p;
                }
            }
        }
    }
    ret
}

/// Returns the first case-sensitive byte match, or -1 when no match exists.
pub fn boyer_moore_case_sensitive(bytes: &[u8], pattern: &[u32]) -> i32 {
    let len = pattern.len();
    if len == 0 {
        return 0; // the empty pattern is found at once
    }
    if bytes.len() < len {
        return -1;
    }
    let max = bytes.len();
    let mut i = 0;
    while i + len <= max {
        let mut found = true;
        for j in (0..len).rev() {
            let p = pattern[j];
            if (p as u8) == bytes[i + j] {
                continue;
            }
            let last = (p >> 8) as usize;
            i += std::cmp::max(1, j.saturating_sub(last));
            found = false;
            break;
        }
        if found {
            return i as i32;
        }
    }
    -1
}

/// Returns the first ASCII case-insensitive byte match, or -1 when no match exists.
pub fn boyer_moore_case_insensitive(bytes: &[u8], pattern: &[u32]) -> i32 {
    let len = pattern.len();
    if len == 0 {
        return 0; // the empty pattern is found at once
    }
    if bytes.len() < len {
        return -1;
    }
    let max = bytes.len();
    let mut i = 0;
    while i + len <= max {
        let mut found = true;
        for j in (0..len).rev() {
            let p = pattern[j];
            let pc = p as u8;
            let bc = bytes[i + j];
            let diff = (pc as i32) - (bc as i32);
            if diff == 0 {
                continue;
            }
            // pc is lowercase pattern char, bc is uppercase input → diff = 32
            if diff == 32 && pc >= b'a' && pc <= b'z' {
                continue;
            }
            // pc is uppercase pattern char, bc is lowercase input → diff = -32
            if diff == -32 && pc >= b'A' && pc <= b'Z' {
                continue;
            }
            let last = (p >> 8) as usize;
            i += std::cmp::max(1, j.saturating_sub(last));
            found = false;
            break;
        }
        if found {
            return i as i32;
        }
    }
    -1
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nullable_bool_states() {
        let mut nb = NullableBool::new();
        assert!(!nb.has_value());
        assert_eq!(nb.value, NullableBool::NULL);

        nb.set_value(true);
        assert!(nb.has_value());
        assert!(nb.get_value());

        nb.set_value(false);
        assert!(nb.has_value());
        assert!(!nb.get_value());

        nb.to_null();
        assert!(!nb.has_value());
    }

    /// Cross-type comparisons with `bool`, `Option<bool>`, and `u8`. The null state compares
    /// unequal to both `true` and `false`.
    #[test]
    fn nullable_bool_against_bool() {
        let (t, f, n) = (NullableBool::from(true), NullableBool::from(false), NullableBool::new());
        assert!(t == true && f == false && true == t && false == f);
        assert!(t != false && f != true && n != true && n != false && true != n && false != n);
        assert_eq!([Option::<bool>::from(t), f.into(), n.into()], [Some(true), Some(false), None]);
        assert!(NullableBool::from(Some(true)) == t && NullableBool::from(Some(false)) == f && NullableBool::from(None) == n);
        assert_eq!([u8::from(f), t.into(), n.into()], [0, 1, NullableBool::NULL]);
    }

    #[test]
    fn base64_roundtrip() {
        // "SGVsbG8=" is base64 for "Hello"
        let mut buf = b"SGVsbG8=".to_vec();
        let n = base64decode(&mut buf, 0, 0, 8);
        assert_eq!(&buf[..n], b"Hello");
    }

    /// Every tail: no padding, one '=' (3 characters), two (2 characters); a byte of no alphabet
    /// decodes as 0.
    #[test]
    fn base64_every_tail() {
        for (text, want) in [(&b"QUJD"[..], &b"ABC"[..]), (b"QUI=", b"AB"), (b"QQ==", b"A"), (b"/w==", &[0xFFu8][..])] {
            let mut buf = text.to_vec();
            let n = base64decode(&mut buf, 0, 0, text.len());
            assert_eq!(&buf[..n], want, "{}", String::from_utf8_lossy(text));
        }
        let mut buf = vec![0xC3u8, 0xA9, b'=', b'='];
        let _ = base64decode(&mut buf, 0, 0, 4); // no panic
    }

    #[test]
    fn boyer_moore_search() {
        let text = b"say hello world, hello rust";
        assert_eq!(boyer_moore_pattern_text("hello"), boyer_moore_pattern(b"hello"), "a text of ASCII is its bytes ( C# string )");
        let pattern = boyer_moore_pattern(b"hello");
        assert_eq!(boyer_moore_case_sensitive(text, &pattern), 4, "the first byte of the first match");
        assert_eq!(boyer_moore_case_sensitive(b"hell", &pattern), -1);
        assert_eq!(boyer_moore_case_sensitive(b"hello", &pattern), 0);
    }

    #[test]
    fn boyer_moore_tables_preserve_last_occurrences() {
        for len in [0, 1, 2, 7, 15, 16, 17, 31, 32, 33, 255, 256, 257, 1024, 4096] {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 73) as u8).collect();
            let expected: Vec<u32> = bytes
                .iter()
                .map(|ch| {
                    let last = bytes.iter().rposition(|other| other == ch).unwrap();
                    ((last as u32) << 8) | *ch as u32
                })
                .collect();
            assert_eq!(boyer_moore_pattern(&bytes), expected);
        }
        for text in ["", "\0", "ab\0aba\0", "hello hello", "éé水𝄞\0水"] {
            let units: Vec<u16> = text.encode_utf16().collect();
            let expected: Vec<u32> = units
                .iter()
                .map(|ch| {
                    let last = units.iter().rposition(|other| other == ch).unwrap();
                    ((last as u32) << 8) | (*ch & 0xFF) as u32
                })
                .collect();
            assert_eq!(boyer_moore_pattern_text(text), expected);
        }
        // a unit past 0xFF keeps the index whole: `水` (U+6C34) first is index 0, not 0x6C
        assert_eq!(boyer_moore_pattern_text("水a")[0] >> 8, 0);
        let pattern = boyer_moore_pattern_text("水a");
        assert_eq!(boyer_moore_case_sensitive(&[0x00, 0x34, b'a'], &pattern), 1, "the low bytes of the units are matched");
    }

    #[test]
    fn boyer_moore_case_insensitive_search() {
        let text = b"Hello World";
        let pattern = boyer_moore_pattern(b"hello");
        assert_eq!(boyer_moore_case_sensitive(text, &pattern), -1);
        assert_eq!(boyer_moore_case_insensitive(text, &pattern), 0);
        assert_eq!(boyer_moore_case_insensitive(b"x HeLLo", &pattern), 2);
    }
}
