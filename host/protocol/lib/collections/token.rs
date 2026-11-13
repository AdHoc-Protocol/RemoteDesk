//! Fast resumable access to collection slots, without a version counter or a held iterator.
//!
//! Obtain a token with `token()` (first entry) or `token_of(key)` (lookup), advance it with
//! `next_token(token)`, and read with `entry_by_token` / `key_by_token` / `value_by_token`. `None`
//! means not found or end of traversal. Nullable collections visit keyed entries first, then
//! [`NULL_TOKEN`]; their ordinary iterators may visit null first.
//!
//! A token belongs to the collection that issued it. It does not borrow that collection and carries
//! no generation: insertion, removal, clear, retain, reserve, shrink or a storage-layout change can
//! invalidate it. Start again after structural changes. Reading and changing only existing values
//! preserve tokens. Slot/presence checks protect memory access but cannot identify an old token
//! whose slot has been reused for another key.
//!
//! Hash-table tokens are physical entry indices, not ordinal positions in `iter()`. Bitset/flat
//! tokens use the unsigned key. Compressed ByteMap tokens pack the value index and unsigned key;
//! use the accessor methods instead of interpreting that encoding.

/// Collection-local slot token. No allocation, lifetime or version counter.
pub type Token = usize;

/// Token for the `None` key. End of traversal is `None`, not this value.
pub const NULL_TOKEN: Token = usize::MAX;

#[inline(always)]
pub(crate) fn dense_contains(token: Token, lo_end: usize, hi_start: usize, cap: usize) -> bool {
    token < lo_end || (hi_start <= token && token < cap)
}

#[inline(always)]
pub(crate) fn dense_first(lo_end: usize, hi_start: usize, cap: usize) -> Option<Token> {
    if lo_end != 0 {
        Some(0)
    } else if hi_start < cap {
        Some(hi_start)
    } else {
        None
    }
}

#[inline(always)]
pub(crate) fn dense_next(token: Token, lo_end: usize, hi_start: usize, cap: usize) -> Option<Token> {
    if !dense_contains(token, lo_end, hi_start, cap) {
        return None;
    }
    // A live index is below capacity, so increment cannot overflow.
    let next = token + 1;
    if next < lo_end {
        Some(next)
    } else if next < hi_start {
        (hi_start < cap).then_some(hi_start)
    } else {
        (next < cap).then_some(next)
    }
}

#[inline(always)]
pub(crate) fn or_null(token: Option<Token>, has_null: bool) -> Option<Token> {
    token.or_else(|| has_null.then_some(NULL_TOKEN))
}

/// Where a pass by number is - the generated sending goes entry after entry and keeps this in its
/// slot: `n + 1` of the entry given last and its token, and for a bitset the keys of that word
/// still to come. The same entry again or the next one is a step, O(1), where a bitset has no
/// `n`-th entry at hand; `n` 0 starts anew. Fields of their own, not packed: one compare and two
/// stores a step.
#[derive(Copy, Clone, Debug)]
pub struct Seq {
    /// `n + 1` of the entry given last: the number of the next one.
    next: u32,
    token: u32,
    bits: u64,
}

impl Default for Seq {
    /// No entry given yet: `next` is no number, the first step starts anew.
    #[inline(always)]
    fn default() -> Self {
        Self { next: u32::MAX, token: 0, bits: 0 }
    }
}

/// The token of the `n`-th entry of a pass, see [`Seq`].
#[inline(always)]
pub(crate) fn seq_token(n: usize, s: &mut Seq, first: impl Fn() -> Option<Token>, next: impl Fn(Token) -> Option<Token>) -> Token {
    let t = if n == s.next as usize {
        next(s.token as usize) // the next one: the most of a pass
    } else if n + 1 == s.next as usize && n != 0 {
        return s.token as usize; // the same one again: a map's key, then its value
    } else if n == 0 {
        first()
    } else {
        walk(n, &first, &next)
    };
    let Some(t) = t else { no_entry(n) };
    s.next = n as u32 + 1;
    s.token = t as u32;
    t
}

/// The `n`-th set bit of `words` in a pass, see [`Seq`]: the next is the lowest of the bits kept,
/// as the iterator of a bitset.
#[inline(always)]
pub(crate) fn seq_bit(n: usize, s: &mut Seq, words: &[u64]) -> usize {
    if n == s.next as usize {
        let mut bits = s.bits;
        let mut w = s.token as usize >> 6;
        while bits == 0 {
            w += 1;
            if w == words.len() {
                no_entry(n)
            }
            bits = unsafe { *words.get_unchecked(w) };
        }
        let k = w << 6 | bits.trailing_zeros() as usize;
        s.bits = bits & (bits - 1);
        s.next = n as u32 + 1;
        s.token = k as u32;
        return k;
    }
    if n + 1 == s.next as usize && n != 0 {
        return s.token as usize;
    }
    seq_bit_from_start(n, s, words)
}

/// A pass that starts, or that did not go bit after bit: the `n`-th bit counted by words.
#[cold]
#[inline(never)]
fn seq_bit_from_start(n: usize, s: &mut Seq, words: &[u64]) -> usize {
    let mut r = n;
    for (w, &word) in words.iter().enumerate() {
        let c = word.count_ones() as usize;
        if r < c {
            let mut bits = word;
            for _ in 0..r {
                bits &= bits - 1;
            }
            let k = w << 6 | bits.trailing_zeros() as usize;
            s.bits = bits & (bits - 1);
            s.next = n as u32 + 1;
            s.token = k as u32;
            return k;
        }
        r -= c;
    }
    no_entry(n)
}

#[cold]
#[inline(never)]
fn no_entry(n: usize) -> ! {
    panic!("no entry number {n}")
}

/// A pass that did not go entry after entry: counted from the start.
#[cold]
#[inline(never)]
fn walk(n: usize, first: &impl Fn() -> Option<Token>, next: &impl Fn(Token) -> Option<Token>) -> Option<Token> {
    let mut t = first();
    for _ in 0..n {
        t = next(t?);
    }
    t
}

/// A key the collection does not keep as such (a bit of a `ByteSet`, a slot of a flat
/// `TwoBytesMap`): read with `*`, as the `&K` an entry of a hash table gives.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Key<K>(pub K);

impl<K> core::ops::Deref for Key<K> {
    type Target = K;
    #[inline(always)]
    fn deref(&self) -> &K {
        &self.0
    }
}
