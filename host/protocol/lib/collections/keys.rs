//! Key conversions for collections with one-byte or two-byte key domains.
//!
//! Implement [`ByteKey`] or [`TwoBytesKey`] to map a key to its unsigned storage index and
//! reconstruct it. Collection layout depends on this domain rather than the key type's in-memory
//! size.

/// Key conversion for one-byte domains used by maps, sets, and packed lists.
///
/// Every supported key must map to a distinct byte and round-trip through
/// `from_byte(to_byte(key))`. Types covering fewer than 256 values may implement the trait, but
/// callers must avoid introducing encodings outside that domain.
pub trait ByteKey: Copy {
    fn to_byte(self) -> u8;
    fn from_byte(b: u8) -> Self;
}

impl ByteKey for u8 {
    #[inline(always)]
    fn to_byte(self) -> u8 {
        self
    }
    #[inline(always)]
    fn from_byte(b: u8) -> Self {
        b
    }
}

/// Stored as its two's-complement bits: `-1` is `255`, so iteration yields `0..=127`, then
/// `-128..=-1`.
impl ByteKey for i8 {
    #[inline(always)]
    fn to_byte(self) -> u8 {
        self as u8
    }
    #[inline(always)]
    fn from_byte(b: u8) -> Self {
        b as i8
    }
}

/// Maps zero to `false` and nonzero bytes to `true`. Inserting boolean keys uses only bytes zero
/// and one; raw operations over the entire byte domain can introduce other encodings.
impl ByteKey for bool {
    #[inline(always)]
    fn to_byte(self) -> u8 {
        self as u8
    }
    #[inline(always)]
    fn from_byte(b: u8) -> Self {
        b != 0
    }
}

/// Key conversion for two-byte domains used by maps and sets.
///
/// Every supported key must map to a distinct `u16` and round-trip through `from_u16(to_u16(key))`.
pub trait TwoBytesKey: Copy {
    fn to_u16(self) -> u16;
    fn from_u16(v: u16) -> Self;
}

impl TwoBytesKey for u16 {
    #[inline(always)]
    fn to_u16(self) -> u16 {
        self
    }
    #[inline(always)]
    fn from_u16(v: u16) -> Self {
        v
    }
}

impl TwoBytesKey for i16 {
    #[inline(always)]
    fn to_u16(self) -> u16 {
        self as u16
    }
    #[inline(always)]
    fn from_u16(v: u16) -> Self {
        v as i16
    }
}
