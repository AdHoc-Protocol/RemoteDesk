//! List of optional `Copy` values with a presence bitset.
//!
//! Compact storage keeps only present values in index order; bitset rank locates each value. Flat
//! storage reserves one value slot per logical index and uses the bitset for presence. Null slots
//! contain the default value.
//!
//! Insertion switches to flat storage when the present-value threshold is reached (1024 by
//! default). `shrink_to_fit` and `set_flat_threshold` can restore compact storage.

use super::bit_list::{self, BitList};
use core::fmt;
use core::hash::{Hash, Hasher};
use core::iter::FusedIterator;
use core::mem;

const DEFAULT_FLAT_THRESHOLD: usize = 1024;

#[derive(Clone)]
pub struct NullablePrimitiveList<T> {
    /// Bit `i` is set when index `i` holds a value. `nulls.len()` is the length of the list.
    nulls: BitList,
    /// Compressed: the present values in index order. Flat: one slot per index,
    /// `values.len() == len`.
    values: Vec<T>,
    /// The number of present values.
    count: usize,
    /// Compressed: `ranks[k]` - the values at indices below `64 k`, one per word of `nulls` begun
    /// (`len.div_ceil(64)`): the compressed place of index `i` is `ranks[i / 64]` plus a `popcnt`
    /// of one word. Flat: empty.
    ranks: Vec<usize>,
    flat: bool,
    flat_threshold: usize,
}

impl<T: Copy + Default> Default for NullablePrimitiveList<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Default> NullablePrimitiveList<T> {
    /// Does not allocate.
    #[inline]
    pub const fn new() -> Self {
        Self { nulls: BitList::new(), values: Vec::new(), count: 0, ranks: Vec::new(), flat: false, flat_threshold: DEFAULT_FLAT_THRESHOLD }
    }

    /// Room for `capacity` values.
    pub fn with_capacity(capacity: usize) -> Self {
        Self { nulls: BitList::with_capacity(capacity), values: Vec::with_capacity(capacity), ..Self::new() }
    }

    /// `len` nulls.
    pub fn with_nulls(len: usize) -> Self {
        Self { nulls: BitList::filled(false, len), ranks: vec![0; len.div_ceil(64)], ..Self::new() }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.nulls.len()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.nulls.is_empty()
    }

    /// The number of present (non-null) values.
    #[inline(always)]
    pub fn cardinality(&self) -> usize {
        self.count
    }

    #[inline(always)]
    pub fn is_flat(&self) -> bool {
        self.flat
    }

    #[inline(always)]
    pub fn flat_threshold(&self) -> usize {
        self.flat_threshold
    }

    /// The presence bits: bit `i` is set when index `i` holds a value.
    #[inline(always)]
    pub fn presence(&self) -> &BitList {
        &self.nulls
    }

    /// Value slots allocated.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.values.capacity()
    }

    /// Sets the threshold and switches to the layout it calls for.
    pub fn set_flat_threshold(&mut self, threshold: usize) {
        self.flat_threshold = threshold;
        if !self.flat && threshold <= self.count {
            self.make_flat();
        } else if self.flat && self.count < threshold {
            self.make_compressed();
        }
    }

    /// In place: the storage of `values` and `ranks` stays, a list received again and again
    /// allocates once.
    #[inline(never)]
    fn make_flat(&mut self) {
        let len = self.nulls.len();
        let mut packed = self.count;
        self.values.resize(len, T::default());
        // Present indices are ascending and never below their packed place. Moving values back
        // from the last index, a word of presence bits at a time, therefore preserves every source
        // still to be read; a null gets the default.
        let v = self.values.as_mut_ptr();
        let mut base = len;
        while packed != 0 {
            let top = base - 1 & 63; // the last bit of the word
            base -= top + 1;
            let w = self.nulls.get64(base);
            for b in (0..=top).rev() {
                let present = w >> b & 1 != 0;
                // SAFETY: `base + b < len == values.len()`; `packed - 1` (`0` once all are moved)
                // is below `count <= len`
                unsafe {
                    let x = *v.add(packed.max(1) - 1);
                    *v.add(base + b) = if present { x } else { T::default() };
                }
                packed -= present as usize;
            }
        }
        self.values[..base].fill(T::default()); // below the first value
        self.flat = true;
        self.ranks.clear();
    }

    fn make_compressed(&mut self) {
        // Present indices are ascending and never below their packed destination. Moving values
        // forward therefore preserves every source still to be read.
        for (packed, i) in self.nulls.ones().enumerate() {
            self.values[packed] = self.values[i];
        }
        self.values.truncate(self.count);
        self.flat = false;
        self.ranks_from(0);
    }

    /// Compressed: `ranks` from word `from` (`<= ranks.len()`) on made anew from `nulls`, one
    /// `popcnt` a word.
    fn ranks_from(&mut self, from: usize) {
        self.ranks.truncate(from);
        let mut r = match from.checked_sub(1) {
            Some(p) => self.ranks[p] + self.nulls.get64(p << 6).count_ones() as usize,
            None => 0,
        };
        for k in from..self.nulls.len().div_ceil(64) {
            self.ranks.push(r);
            r += self.nulls.get64(k << 6).count_ones() as usize;
        }
    }

    /// Compressed: `ranks` grown to cover `len` indices, the new words all nulls (past every
    /// value).
    #[inline]
    fn ranks_to(&mut self, len: usize) {
        let blocks = len.div_ceil(64);
        if self.ranks.len() < blocks {
            self.ranks.resize(blocks, self.count);
        }
    }

    /// The number of values before index `i`: the compressed position of index `i`. Compressed: a
    /// rank of the word plus a `popcnt` of the bits below `i` in it.
    #[inline]
    fn rank_below(&self, i: usize) -> usize {
        if self.flat {
            return if i == 0 { 0 } else { self.nulls.rank(i - 1) };
        }
        match self.ranks.get(i >> 6) {
            Some(&r) => r + (self.nulls.get64(i & !63) & ((1u64 << (i & 63)) - 1)).count_ones() as usize,
            None => self.count, // past the last word begun: every value is before
        }
    }

    #[inline]
    pub fn has_value(&self, i: usize) -> bool {
        self.nulls.get(i)
    }

    /// The value at `i`; `None` for a null and past the end.
    #[inline]
    pub fn get(&self, i: usize) -> Option<T> {
        if self.flat {
            return if self.nulls.get(i) { Some(self.values[i]) } else { None };
        }
        // compressed: one word of presence bits gives both the bit and the place
        let r = *self.ranks.get(i >> 6)?;
        let (w, b) = (self.nulls.get64(i & !63), i & 63);
        if w >> b & 1 == 0 {
            return None;
        }
        Some(self.values[r + (w & ((1u64 << b) - 1)).count_ones() as usize])
    }

    /// Sets index `i` and returns its previous value; past the end the list grows with nulls.
    #[inline]
    pub fn set(&mut self, i: usize, value: Option<T>) -> Option<T> {
        if i >= self.nulls.len() {
            return self.set_past(i, value);
        }
        // one word of presence bits gives the bit and the place; the presence kept - a value over a
        // value, a null over a null - no bit and no rank changes
        let (w, b) = (self.nulls.get64(i & !63), i & 63);
        if (w >> b & 1 != 0) != value.is_some() {
            return self.set_change(i, w, value);
        }
        let v = value?;
        // SAFETY: `i < len`: compressed has a rank for its word; the place of a set bit is below
        // `values.len()` (`count` compressed, `len` flat)
        let p = if self.flat { i } else { unsafe { *self.ranks.get_unchecked(i >> 6) + (w & ((1u64 << b) - 1)).count_ones() as usize } };
        Some(mem::replace(unsafe { self.values.get_unchecked_mut(p) }, v))
    }

    /// [`set`](Self::set) of `i < len` where a value comes or goes; `w` - the word of `i`.
    #[inline(never)]
    fn set_change(&mut self, i: usize, w: u64, value: Option<T>) -> Option<T> {
        if self.flat {
            self.nulls.set(i, value.is_some());
            return match value {
                Some(v) => {
                    self.values[i] = v;
                    self.count += 1;
                    None
                }
                None => {
                    self.count -= 1;
                    Some(self.values[i])
                }
            };
        }
        let p = self.ranks[i >> 6] + (w & ((1u64 << (i & 63)) - 1)).count_ones() as usize;
        match value {
            Some(v) => {
                if self.count >= self.flat_threshold {
                    self.make_flat();
                    return self.set(i, Some(v));
                }
                self.values.insert(p, v);
                self.nulls.set1(i);
                self.count += 1;
                for x in &mut self.ranks[(i >> 6) + 1..] {
                    *x += 1;
                }
                None
            }
            None => {
                let old = self.values.remove(p);
                self.nulls.set0(i);
                self.count -= 1;
                for x in &mut self.ranks[(i >> 6) + 1..] {
                    *x -= 1;
                }
                Some(old)
            }
        }
    }

    /// [`set`](Self::set) past the end: the list grows with nulls to `i`.
    fn set_past(&mut self, i: usize, value: Option<T>) -> Option<T> {
        if value.is_some() && !self.flat && self.count >= self.flat_threshold {
            self.make_flat();
        }
        if self.flat {
            self.values.resize(i + 1, T::default());
            if let Some(v) = value {
                self.values[i] = v;
            }
        } else {
            if let Some(v) = value {
                self.values.push(v); // past every value
            }
            self.ranks_to(i + 1);
        }
        self.count += value.is_some() as usize;
        self.nulls.set(i, value.is_some()); // grows the list
        None
    }

    #[inline]
    pub fn push(&mut self, value: Option<T>) {
        // The value goes to the end of `values` (the compressed place of the last index is `count`,
        // no rank): always flat - the default for a null - only a value compressed.
        let some = value.is_some();
        if some && !self.flat && self.count >= self.flat_threshold {
            self.make_flat();
        }
        if some | self.flat {
            self.values.push(value.unwrap_or_default());
        }
        if !self.flat && self.nulls.len() & 63 == 0 {
            self.ranks.push(self.count); // a word begun
        }
        self.count += some as usize;
        self.nulls.push(some);
    }

    /// Removes the last item: `None` when empty, `Some(None)` for a null.
    pub fn pop(&mut self) -> Option<Option<T>> {
        let last = self.len().checked_sub(1)?;
        Some(self.remove(last))
    }

    /// Inserts `value` at `i`, moving the items from `i` on up by one; past the end the list grows
    /// as `set` does.
    pub fn insert(&mut self, i: usize, value: Option<T>) {
        if i >= self.len() {
            self.set(i, value);
            return;
        }
        if self.flat {
            self.values.insert(i, value.unwrap_or_default());
            self.nulls.insert(i, value.is_some());
            self.count += value.is_some() as usize;
            return;
        }
        match value {
            None => self.nulls.insert(i, false),
            Some(v) => {
                if self.count >= self.flat_threshold {
                    self.make_flat();
                    self.insert(i, Some(v));
                    return;
                }
                let r = self.rank_below(i);
                self.values.insert(r, v);
                self.nulls.insert(i, true);
                self.count += 1;
            }
        }
        self.ranks_from(i >> 6); // the bits from `i` on moved up
    }

    /// Removes the item at `i`, moving the items above down by one; returns its value.
    ///
    /// # Panics
    ///
    /// If `i >= len`.
    pub fn remove(&mut self, i: usize) -> Option<T> {
        assert!(i < self.len(), "index {i} out of range for length {}", self.len());
        let had = self.nulls.get(i);
        let v = if self.flat {
            let v = self.values.remove(i);
            had.then_some(v)
        } else if had {
            Some(self.values.remove(self.rank_below(i)))
        } else {
            None
        };
        self.nulls.remove(i);
        self.count -= had as usize;
        if !self.flat {
            self.ranks_from(i >> 6); // the bits above `i` moved down
        }
        v
    }

    /// Swaps the items at `i` and `j`.
    pub fn swap(&mut self, i: usize, j: usize) {
        assert!(i < self.len() && j < self.len(), "index out of range");
        let (a, b) = (self.get(i), self.get(j));
        if self.flat {
            self.values.swap(i, j);
            self.nulls.set(i, b.is_some());
            self.nulls.set(j, a.is_some());
        } else if a.is_some() != b.is_some() {
            self.set(i, b);
            self.set(j, a);
        } else if let (Some(a), Some(b)) = (a, b) {
            let (ri, rj) = (self.rank_below(i), self.rank_below(j));
            self.values[ri] = b;
            self.values[rj] = a;
        }
    }

    /// Sets the length: shortens, or grows with nulls.
    pub fn set_len(&mut self, len: usize) {
        if len < self.len() {
            self.count = self.rank_below(len);
            self.values.truncate(if self.flat { len } else { self.count });
            self.nulls.truncate(len);
            self.ranks.truncate(if self.flat { 0 } else { len.div_ceil(64) });
        } else {
            if self.flat {
                self.values.resize(len, T::default());
            } else {
                self.ranks_to(len);
            }
            self.nulls.set_len(len);
        }
    }

    #[inline]
    pub fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.set_len(len);
        }
    }

    /// Removes all items; back to the compressed layout.
    pub fn clear(&mut self) {
        self.nulls.clear();
        self.values.clear();
        self.ranks.clear();
        self.count = 0;
        self.flat = false;
    }

    /// Drops the nulls after the last value.
    pub fn trim(&mut self) {
        self.truncate(self.nulls.last_one().map_or(0, |i| i + 1));
        self.shrink_to_fit();
    }

    /// Frees the unused memory; back to compressed when under the threshold.
    pub fn shrink_to_fit(&mut self) {
        if self.flat && self.count < self.flat_threshold {
            self.make_compressed();
        }
        self.values.shrink_to_fit();
        self.ranks.shrink_to_fit();
        self.nulls.shrink_to_fit();
    }

    /// The first index `>= from` that holds a value.
    #[inline]
    pub fn next_value_index(&self, from: usize) -> Option<usize> {
        self.nulls.next_one(from)
    }

    /// The last index `< before` that holds a value.
    #[inline]
    pub fn prev_value_index(&self, before: usize) -> Option<usize> {
        self.nulls.prev_one(before)
    }

    /// The first null index `>= from`.
    #[inline]
    pub fn next_null_index(&self, from: usize) -> Option<usize> {
        self.nulls.next_zero(from)
    }

    /// The last null index `< before`.
    #[inline]
    pub fn prev_null_index(&self, before: usize) -> Option<usize> {
        self.nulls.prev_zero(before)
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, T> {
        Iter { nulls: self.nulls.iter(), bits: &self.nulls, values: &self.values, pos: 0, n: 0, flat: self.flat }
    }
}

impl<T: Copy + Default + PartialEq> NullablePrimitiveList<T> {
    pub fn index_of(&self, value: Option<T>) -> Option<usize> {
        match value {
            None => self.nulls.next_zero(0),
            Some(v) if self.flat => self.nulls.ones().find(|&i| self.values[i] == v),
            Some(v) => self.values.iter().position(|x| *x == v).and_then(|n| self.nulls.nth_one(n)),
        }
    }

    pub fn last_index_of(&self, value: Option<T>) -> Option<usize> {
        match value {
            None => self.nulls.prev_zero(self.len()),
            Some(v) if self.flat => {
                let mut before = self.len();
                while let Some(i) = self.nulls.prev_one(before) {
                    if self.values[i] == v {
                        return Some(i);
                    }
                    before = i;
                }
                None
            }
            Some(v) => self.values.iter().rposition(|x| *x == v).and_then(|n| self.nulls.nth_one(n)),
        }
    }

    #[inline]
    pub fn contains(&self, value: Option<T>) -> bool {
        self.index_of(value).is_some()
    }

    /// Removes the first occurrence of `value`; `true` if there was one.
    pub fn remove_item(&mut self, value: Option<T>) -> bool {
        match self.index_of(value) {
            Some(i) => {
                self.remove(i);
                true
            }
            None => false,
        }
    }
}

impl<T: Copy + Default + PartialEq> PartialEq for NullablePrimitiveList<T> {
    /// The presence bits, then the values: compressed both or all values, the slices. All values
    /// or none - the same bits, not compared.
    fn eq(&self, other: &Self) -> bool {
        let (len, count) = (self.len(), self.count);
        if len != other.len() || count != other.count || count != len && count != 0 && self.nulls != other.nulls {
            return false;
        }
        match (self.flat, other.flat) {
            (false, false) => self.values == other.values,
            _ if count == len => self.values == other.values,
            (true, true) => self.nulls.ones().all(|i| self.values[i] == other.values[i]),
            (true, false) => self.nulls.ones().zip(&other.values).all(|(i, v)| self.values[i] == *v),
            (false, true) => self.values.iter().zip(other.nulls.ones()).all(|(v, i)| *v == other.values[i]),
        }
    }
}

impl<T: Copy + Default + Eq> Eq for NullablePrimitiveList<T> {}

impl<T: Copy + Default + Hash> Hash for NullablePrimitiveList<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.nulls.hash(state);
        // the values in index order whatever the layout: compressed, the slice
        if self.flat {
            for i in self.nulls.ones() {
                self.values[i].hash(state);
            }
        } else {
            for v in &self.values {
                v.hash(state);
            }
        }
    }
}

impl<T: Copy + Default + fmt::Debug> fmt::Debug for NullablePrimitiveList<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: Copy + Default> Extend<Option<T>> for NullablePrimitiveList<T> {
    fn extend<I: IntoIterator<Item = Option<T>>>(&mut self, iter: I) {
        for v in iter {
            self.push(v);
        }
    }
}

impl<T: Copy + Default> FromIterator<Option<T>> for NullablePrimitiveList<T> {
    fn from_iter<I: IntoIterator<Item = Option<T>>>(iter: I) -> Self {
        let mut l = Self::new();
        l.extend(iter);
        l
    }
}

// iterator

/// The items in order: one presence bit per step, the compressed values consumed in step.
#[derive(Clone)]
pub struct Iter<'a, T> {
    nulls: bit_list::Iter<'a>,
    /// The presence bits, for `fold` a word at a time.
    bits: &'a BitList,
    values: &'a [T],
    pos: usize,
    /// The next compressed value.
    n: usize,
    flat: bool,
}

impl<T: Copy> Iterator for Iter<'_, T> {
    type Item = Option<T>;

    #[inline]
    fn next(&mut self) -> Option<Option<T>> {
        let present = self.nulls.next()?;
        self.pos += 1;
        if !present {
            return Some(None);
        }
        let i = if self.flat {
            self.pos - 1
        } else {
            self.n += 1;
            self.n - 1
        };
        Some(Some(self.values[i]))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.nulls.size_hint()
    }

    /// A word of presence bits at a time (one `get64` per 64 items). Flat: every index has its
    /// slot, the value is read whatever the bit and only chosen by it; compressed: the next value
    /// read at each step and taken past for each bit set. No branch on the bit either way.
    #[inline]
    fn fold<B, F: FnMut(B, Option<T>) -> B>(self, mut acc: B, mut f: F) -> B {
        let (mut pos, end, mut n) = (self.pos, self.bits.len(), self.n);
        while pos < end {
            let w = self.bits.get64(pos);
            let k = (end - pos).min(64);
            if self.flat {
                for (j, &v) in self.values[pos..pos + k].iter().enumerate() {
                    acc = f(acc, if w >> j & 1 != 0 { Some(v) } else { None });
                }
            } else if let Some(last) = self.values.len().checked_sub(1) {
                for j in 0..k {
                    let p = (w >> j & 1) as usize;
                    // SAFETY: `n.min(last)` is a place of `values`; at a null the value read is not
                    // used
                    let v = unsafe { *self.values.get_unchecked(n.min(last)) };
                    acc = f(acc, if p != 0 { Some(v) } else { None });
                    n += p;
                }
            } else {
                for _ in 0..k {
                    acc = f(acc, None); // no values: all nulls
                }
            }
            pos += k;
        }
        acc
    }
}

impl<T: Copy> ExactSizeIterator for Iter<'_, T> {}
impl<T: Copy> FusedIterator for Iter<'_, T> {}

impl<'a, T: Copy + Default> IntoIterator for &'a NullablePrimitiveList<T> {
    type Item = Option<T>;
    type IntoIter = Iter<'a, T>;
    #[inline]
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_reuses_storage_and_preserves_sparse_values() {
        for stride in [1, 2, 63, 64, 65, 257, 1024] {
            let mut list = NullablePrimitiveList::with_nulls(513);
            list.set_flat_threshold(0);
            let mut model = vec![None; list.len()];
            for i in (0..list.len()).step_by(stride) {
                model[i] = Some(i as u32 + 1);
                list.set(i, model[i]);
            }
            let storage = list.values.as_ptr();
            let capacity = list.capacity();
            list.set_flat_threshold(usize::MAX);
            assert!(!list.is_flat());
            assert_eq!(list.values.as_ptr(), storage);
            assert_eq!(list.capacity(), capacity);
            assert_eq!(list.iter().collect::<Vec<_>>(), model);
            assert_eq!(list.values.len(), list.cardinality());
        }

        let mut empty = NullablePrimitiveList::<u32>::with_nulls(513);
        empty.set_flat_threshold(0);
        let storage = empty.values.as_ptr();
        empty.set_flat_threshold(usize::MAX);
        assert_eq!(empty.values.as_ptr(), storage);
        assert_eq!(empty.cardinality(), 0);
        assert_eq!(empty.len(), 513);
        assert!(empty.iter().all(|v| v.is_none()));
    }
}

#[cfg(test)]
impl<T: Copy + Default> NullablePrimitiveList<T> {
    /// `ranks` as `nulls` makes them: one per word begun in compressed, none in flat.
    pub(crate) fn assert_ranks(&self) {
        if self.flat {
            assert!(self.ranks.is_empty(), "ranks in flat");
            return;
        }
        let want: Vec<usize> = (0..self.len().div_ceil(64)).map(|k| if k == 0 { 0 } else { self.nulls.rank((k << 6) - 1) }).collect();
        assert_eq!(self.ranks, want, "ranks");
        assert_eq!(self.values.len(), self.count, "values");
    }
}
