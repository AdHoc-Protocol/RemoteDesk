//! List of optional values with compact and flat layouts.
//!
//! Compact storage uses a presence [`BitList`] and only present values in index order. Flat storage
//! uses `Vec<Option<T>>` for constant-time access. When `Option<T>` can use a niche in `T`, the
//! optional value needs no additional tag storage.
//!
//! Insertion switches to flat storage when the present-value threshold is reached (1024 by
//! default). `shrink_to_fit` and `set_flat_threshold` can restore compact storage. Value comparison
//! uses `PartialEq`.

use super::bit_list::{self, BitList};
use core::fmt;
use core::hash::{Hash, Hasher};
use core::iter::FusedIterator;
use core::mem;

const DEFAULT_FLAT_THRESHOLD: usize = 1024;

#[derive(Clone)]
enum Repr<T> {
    /// Bit `i` is set when index `i` holds a value; `nulls.len()` is the length of the list.
    /// `ranks[k]` - the values at indices below `64 k`, one per word of `nulls` begun
    /// (`len.div_ceil(64)`): the place of index `i` is `ranks[i / 64]` plus a `popcnt` of one word.
    Compressed { nulls: BitList, values: Vec<T>, ranks: Vec<usize> },
    /// `count` is the number of `Some`.
    Flat { items: Vec<Option<T>>, count: usize },
}

#[derive(Clone)]
pub struct NullableObjectList<T> {
    repr: Repr<T>,
    flat_threshold: usize,
}

impl<T> Default for NullableObjectList<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// The number of values before index `i` in a compressed list: the rank of its word plus a `popcnt`
/// of the bits below `i` in it; past the last word begun every value is before.
#[inline]
fn rank_below(nulls: &BitList, ranks: &[usize], count: usize, i: usize) -> usize {
    match ranks.get(i >> 6) {
        Some(&r) => r + (nulls.get64(i & !63) & ((1u64 << (i & 63)) - 1)).count_ones() as usize,
        None => count,
    }
}

/// `ranks` from word `from` (`<= ranks.len()`) on made anew from `nulls`, one `popcnt` a word.
fn ranks_from(nulls: &BitList, ranks: &mut Vec<usize>, from: usize) {
    ranks.truncate(from);
    let mut r = match from.checked_sub(1) {
        Some(p) => ranks[p] + nulls.get64(p << 6).count_ones() as usize,
        None => 0,
    };
    for k in from..nulls.len().div_ceil(64) {
        ranks.push(r);
        r += nulls.get64(k << 6).count_ones() as usize;
    }
}

/// `ranks` grown to cover `len` indices, the new words all nulls (past every value: `count` before
/// each).
#[inline]
fn ranks_to(ranks: &mut Vec<usize>, count: usize, len: usize) {
    let blocks = len.div_ceil(64);
    if ranks.len() < blocks {
        ranks.resize(blocks, count);
    }
}

impl<T> NullableObjectList<T> {
    /// Does not allocate.
    #[inline]
    pub const fn new() -> Self {
        Self {
            repr: Repr::Compressed {
                nulls: BitList::new(),
                values: Vec::new(),
                ranks: Vec::new(),
            },
            flat_threshold: DEFAULT_FLAT_THRESHOLD,
        }
    }

    /// Room for `capacity` values.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            repr: Repr::Compressed {
                nulls: BitList::with_capacity(capacity),
                values: Vec::with_capacity(capacity),
                ranks: Vec::with_capacity(capacity.div_ceil(64)),
            },
            flat_threshold: DEFAULT_FLAT_THRESHOLD,
        }
    }

    /// `len` nulls.
    pub fn with_nulls(len: usize) -> Self {
        Self {
            repr: Repr::Compressed {
                nulls: BitList::filled(false, len),
                values: Vec::new(),
                ranks: vec![0; len.div_ceil(64)],
            },
            flat_threshold: DEFAULT_FLAT_THRESHOLD,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        match &self.repr {
            Repr::Compressed { nulls, .. } => nulls.len(),
            Repr::Flat { items, .. } => items.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of present (non-null) values.
    #[inline]
    pub fn cardinality(&self) -> usize {
        match &self.repr {
            Repr::Compressed { values, .. } => values.len(),
            Repr::Flat { count, .. } => *count,
        }
    }

    #[inline]
    pub fn is_flat(&self) -> bool {
        matches!(self.repr, Repr::Flat { .. })
    }

    #[inline]
    pub fn flat_threshold(&self) -> usize {
        self.flat_threshold
    }

    /// Sets the threshold and switches to the layout it calls for.
    pub fn set_flat_threshold(&mut self, threshold: usize) {
        self.flat_threshold = threshold;
        let count = self.cardinality();
        if !self.is_flat() && threshold <= count {
            self.make_flat();
        } else if self.is_flat() && count < threshold {
            self.make_compressed();
        }
    }

    fn make_flat(&mut self) {
        if let Repr::Compressed { nulls, values, .. } = &mut self.repr {
            let count = values.len();
            let mut vals = mem::take(values).into_iter();
            let items = nulls.iter().map(|present| if present { vals.next() } else { None }).collect();
            self.repr = Repr::Flat { items, count };
        }
    }

    fn make_compressed(&mut self) {
        if let Repr::Flat { items, .. } = &mut self.repr {
            let nulls: BitList = items.iter().map(Option::is_some).collect();
            let values = mem::take(items).into_iter().flatten().collect();
            let mut ranks = Vec::new();
            ranks_from(&nulls, &mut ranks, 0);
            self.repr = Repr::Compressed { nulls, values, ranks };
        }
    }

    #[inline]
    pub fn has_value(&self, i: usize) -> bool {
        match &self.repr {
            Repr::Compressed { nulls, .. } => nulls.get(i),
            Repr::Flat { items, .. } => matches!(items.get(i), Some(Some(_))),
        }
    }

    /// The compressed place of index `i` when it holds a value: one word of presence bits gives
    /// both the bit and the place.
    #[inline]
    fn place(nulls: &BitList, ranks: &[usize], i: usize) -> Option<usize> {
        let r = *ranks.get(i >> 6)?;
        let (w, b) = (nulls.get64(i & !63), i & 63);
        (w >> b & 1 != 0).then(|| r + (w & ((1u64 << b) - 1)).count_ones() as usize)
    }

    /// The value at `i`; `None` for a null and past the end.
    #[inline]
    pub fn get(&self, i: usize) -> Option<&T> {
        match &self.repr {
            Repr::Compressed { nulls, values, ranks } => Self::place(nulls, ranks, i).map(|p| &values[p]),
            Repr::Flat { items, .. } => items.get(i)?.as_ref(),
        }
    }

    #[inline]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        match &mut self.repr {
            Repr::Compressed { nulls, values, ranks } => Self::place(nulls, ranks, i).map(|p| &mut values[p]),
            Repr::Flat { items, .. } => items.get_mut(i)?.as_mut(),
        }
    }

    /// Sets index `i` and returns its previous value; past the end the list grows with nulls.
    pub fn set(&mut self, i: usize, value: Option<T>) -> Option<T> {
        if !self.is_flat() && self.cardinality() >= self.flat_threshold && value.is_some() && !self.has_value(i) {
            self.make_flat();
        }
        match &mut self.repr {
            Repr::Flat { items, count } => {
                if i >= items.len() {
                    items.resize_with(i + 1, || None);
                }
                *count += value.is_some() as usize;
                let old = mem::replace(&mut items[i], value);
                *count -= old.is_some() as usize;
                old
            }
            // the presence kept - a value over a value, a null over a null: one word gives the bit
            // and the place
            Repr::Compressed { nulls, values, ranks } => {
                if i < nulls.len() {
                    let (w, b) = (nulls.get64(i & !63), i & 63);
                    if (w >> b & 1 != 0) == value.is_some() {
                        let v = value?;
                        return Some(mem::replace(&mut values[ranks[i >> 6] + (w & ((1u64 << b) - 1)).count_ones() as usize], v));
                    }
                }
                Self::set_other(nulls, values, ranks, i, value)
            }
        }
    }

    /// [`set`](Self::set) in a compressed list, all but a value over a value: a value comes or goes
    /// - `values` moves and the ranks after the word change by one - or a null over a null.
    fn set_other(nulls: &mut BitList, values: &mut Vec<T>, ranks: &mut Vec<usize>, i: usize, value: Option<T>) -> Option<T> {
        let had = nulls.get(i);
        let r = rank_below(nulls, ranks, values.len(), i);
        match (had, value) {
            (true, Some(v)) => Some(mem::replace(&mut values[r], v)),
            (false, Some(v)) => {
                values.insert(r, v);
                ranks_to(ranks, values.len() - 1, i + 1);
                nulls.set1(i);
                for x in &mut ranks[(i >> 6) + 1..] {
                    *x += 1;
                }
                None
            }
            (true, None) => {
                nulls.set0(i);
                for x in &mut ranks[(i >> 6) + 1..] {
                    *x -= 1;
                }
                Some(values.remove(r))
            }
            (false, None) => {
                ranks_to(ranks, values.len(), i + 1);
                nulls.set0(i); // grows the list if needed
                None
            }
        }
    }

    /// Appends: compressed - the value to the end of `values` (its place is the number of values,
    /// no rank), a rank for a word begun; flat - `Vec::push`.
    #[inline]
    pub fn push(&mut self, value: Option<T>) {
        if value.is_some() && !self.is_flat() && self.cardinality() >= self.flat_threshold {
            self.make_flat();
        }
        match &mut self.repr {
            Repr::Flat { items, count } => {
                *count += value.is_some() as usize;
                items.push(value);
            }
            Repr::Compressed { nulls, values, ranks } => {
                if nulls.len() & 63 == 0 {
                    ranks.push(values.len()); // a word begun
                }
                nulls.push(value.is_some());
                if let Some(v) = value {
                    values.push(v);
                }
            }
        }
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
        if !self.is_flat() && value.is_some() && self.cardinality() >= self.flat_threshold {
            self.make_flat();
        }
        match &mut self.repr {
            Repr::Flat { items, count } => {
                *count += value.is_some() as usize;
                items.insert(i, value);
            }
            Repr::Compressed { nulls, values, ranks } => {
                match value {
                    None => nulls.insert(i, false),
                    Some(v) => {
                        values.insert(rank_below(nulls, ranks, values.len(), i), v);
                        nulls.insert(i, true);
                    }
                }
                ranks_from(nulls, ranks, i >> 6); // the bits from `i` on moved up
            }
        }
    }

    /// Removes the item at `i`, moving the items above down by one; returns its value.
    ///
    /// # Panics
    ///
    /// If `i >= len`.
    pub fn remove(&mut self, i: usize) -> Option<T> {
        assert!(i < self.len(), "index {i} out of range for length {}", self.len());
        match &mut self.repr {
            Repr::Flat { items, count } => {
                let v = items.remove(i);
                *count -= v.is_some() as usize;
                v
            }
            Repr::Compressed { nulls, values, ranks } => {
                let v = if nulls.get(i) { Some(values.remove(rank_below(nulls, ranks, values.len(), i))) } else { None };
                nulls.remove(i);
                ranks_from(nulls, ranks, i >> 6); // the bits above `i` moved down
                v
            }
        }
    }

    /// Swaps the items at `i` and `j`.
    pub fn swap(&mut self, i: usize, j: usize) {
        assert!(i < self.len() && j < self.len(), "index out of range");
        if i == j {
            return;
        }
        match &mut self.repr {
            Repr::Flat { items, .. } => items.swap(i, j),
            Repr::Compressed { nulls, values, ranks } => match (nulls.get(i), nulls.get(j)) {
                (true, true) => {
                    let n = values.len();
                    let (ri, rj) = (rank_below(nulls, ranks, n, i), rank_below(nulls, ranks, n, j));
                    values.swap(ri, rj);
                }
                (false, false) => {}
                _ => {
                    let a = self.set(i, None);
                    let b = self.set(j, None);
                    self.set(i, b);
                    self.set(j, a);
                }
            },
        }
    }

    /// Sets the length: shortens, or grows with nulls.
    pub fn set_len(&mut self, len: usize) {
        match &mut self.repr {
            Repr::Flat { items, count } => {
                if len < items.len() {
                    *count -= items[len..].iter().filter(|x| x.is_some()).count();
                }
                items.resize_with(len, || None);
            }
            Repr::Compressed { nulls, values, ranks } => {
                if len < nulls.len() {
                    values.truncate(rank_below(nulls, ranks, values.len(), len));
                    ranks.truncate(len.div_ceil(64));
                } else {
                    ranks_to(ranks, values.len(), len);
                }
                nulls.set_len(len);
            }
        }
    }

    #[inline]
    pub fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.set_len(len);
        }
    }

    /// Removes all items, keeping the storage and the layout: a list received again and again
    /// allocates once. A flat list stays flat - its `Vec<Option<T>>` can not become the compressed
    /// `Vec<T>`; `shrink_to_fit` takes it back.
    pub fn clear(&mut self) {
        match &mut self.repr {
            Repr::Compressed { nulls, values, ranks } => {
                nulls.clear();
                values.clear();
                ranks.clear();
            }
            Repr::Flat { items, count } => {
                items.clear();
                *count = 0;
            }
        }
    }

    /// Drops the nulls after the last value.
    pub fn trim(&mut self) {
        let end = self.prev_value_index(self.len()).map_or(0, |i| i + 1);
        self.truncate(end);
        self.shrink_to_fit();
    }

    /// Frees the unused memory; back to compressed when under the threshold.
    pub fn shrink_to_fit(&mut self) {
        if self.is_flat() && self.cardinality() < self.flat_threshold {
            self.make_compressed();
        }
        match &mut self.repr {
            Repr::Flat { items, .. } => items.shrink_to_fit(),
            Repr::Compressed { nulls, values, ranks } => {
                nulls.shrink_to_fit();
                values.shrink_to_fit();
                ranks.shrink_to_fit();
            }
        }
    }

    /// The first index `>= from` that holds a value.
    pub fn next_value_index(&self, from: usize) -> Option<usize> {
        match &self.repr {
            Repr::Compressed { nulls, .. } => nulls.next_one(from),
            Repr::Flat { items, .. } => items.get(from..)?.iter().position(Option::is_some).map(|p| from + p),
        }
    }

    /// The last index `< before` that holds a value.
    pub fn prev_value_index(&self, before: usize) -> Option<usize> {
        match &self.repr {
            Repr::Compressed { nulls, .. } => nulls.prev_one(before),
            Repr::Flat { items, .. } => items[..before.min(items.len())].iter().rposition(Option::is_some),
        }
    }

    /// The first null index `>= from`.
    pub fn next_null_index(&self, from: usize) -> Option<usize> {
        match &self.repr {
            Repr::Compressed { nulls, .. } => nulls.next_zero(from),
            Repr::Flat { items, .. } => items.get(from..)?.iter().position(Option::is_none).map(|p| from + p),
        }
    }

    /// The last null index `< before`.
    pub fn prev_null_index(&self, before: usize) -> Option<usize> {
        match &self.repr {
            Repr::Compressed { nulls, .. } => nulls.prev_zero(before),
            Repr::Flat { items, .. } => items[..before.min(items.len())].iter().rposition(Option::is_none),
        }
    }

    #[inline]
    pub fn iter(&self) -> Iter<'_, T> {
        Iter(match &self.repr {
            Repr::Compressed { nulls, values, .. } => IterRepr::Compressed(nulls.iter(), values.iter(), nulls),
            Repr::Flat { items, .. } => IterRepr::Flat(items.iter()),
        })
    }

    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        IterMut(match &mut self.repr {
            Repr::Compressed { nulls, values, .. } => IterMutRepr::Compressed(nulls.iter(), values.iter_mut()),
            Repr::Flat { items, .. } => IterMutRepr::Flat(items.iter_mut()),
        })
    }
}

impl<T: PartialEq> NullableObjectList<T> {
    pub fn index_of(&self, value: Option<&T>) -> Option<usize> {
        match (value, &self.repr) {
            (None, _) => self.next_null_index(0),
            (Some(v), Repr::Compressed { nulls, values, .. }) => values.iter().position(|x| x == v).and_then(|n| nulls.nth_one(n)),
            (Some(v), Repr::Flat { items, .. }) => items.iter().position(|x| x.as_ref() == Some(v)),
        }
    }

    pub fn last_index_of(&self, value: Option<&T>) -> Option<usize> {
        match (value, &self.repr) {
            (None, _) => self.prev_null_index(self.len()),
            (Some(v), Repr::Compressed { nulls, values, .. }) => values.iter().rposition(|x| x == v).and_then(|n| nulls.nth_one(n)),
            (Some(v), Repr::Flat { items, .. }) => items.iter().rposition(|x| x.as_ref() == Some(v)),
        }
    }

    #[inline]
    pub fn contains(&self, value: Option<&T>) -> bool {
        self.index_of(value).is_some()
    }

    /// Removes the first occurrence of `value`; `true` if there was one.
    pub fn remove_item(&mut self, value: Option<&T>) -> bool {
        match self.index_of(value) {
            Some(i) => {
                self.remove(i);
                true
            }
            None => false,
        }
    }
}

impl<T: PartialEq> PartialEq for NullableObjectList<T> {
    /// One layout both: the presence bits and the values, or the items, as slices.
    fn eq(&self, other: &Self) -> bool {
        match (&self.repr, &other.repr) {
            // all values or none (as many as the length or zero) - the same bits, not compared
            (Repr::Compressed { nulls: a, values: x, .. }, Repr::Compressed { nulls: b, values: y, .. }) => {
                a.len() == b.len() && x.len() == y.len() && (x.len() == a.len() || x.is_empty() || a == b) && x == y
            }
            (Repr::Flat { items: x, .. }, Repr::Flat { items: y, .. }) => x == y,
            _ => self.len() == other.len() && self.iter().eq(other.iter()),
        }
    }
}

impl<T: Eq> Eq for NullableObjectList<T> {}

impl<T: Hash> Hash for NullableObjectList<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.len().hash(state);
        for v in self.iter() {
            v.hash(state);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for NullableObjectList<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T> Extend<Option<T>> for NullableObjectList<T> {
    fn extend<I: IntoIterator<Item = Option<T>>>(&mut self, iter: I) {
        for v in iter {
            self.push(v);
        }
    }
}

impl<T> FromIterator<Option<T>> for NullableObjectList<T> {
    fn from_iter<I: IntoIterator<Item = Option<T>>>(iter: I) -> Self {
        let mut l = Self::new();
        l.extend(iter);
        l
    }
}

// iterators

enum IterRepr<'a, T> {
    /// The presence bits left, the values left, all the presence bits (for `fold` a word at a time).
    Compressed(bit_list::Iter<'a>, core::slice::Iter<'a, T>, &'a BitList),
    Flat(core::slice::Iter<'a, Option<T>>),
}

/// The items in order.
pub struct Iter<'a, T>(IterRepr<'a, T>);

impl<T> Clone for Iter<'_, T> {
    fn clone(&self) -> Self {
        Iter(match &self.0 {
            IterRepr::Compressed(n, v, b) => IterRepr::Compressed(n.clone(), v.clone(), b),
            IterRepr::Flat(i) => IterRepr::Flat(i.clone()),
        })
    }
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = Option<&'a T>;

    #[inline]
    fn next(&mut self) -> Option<Option<&'a T>> {
        match &mut self.0 {
            IterRepr::Compressed(nulls, values, _) => Some(if nulls.next()? { values.next() } else { None }),
            IterRepr::Flat(items) => items.next().map(Option::as_ref),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IterRepr::Compressed(nulls, ..) => nulls.size_hint(),
            IterRepr::Flat(items) => items.size_hint(),
        }
    }

    /// The layout chosen once, then the fold of the slice (flat) or of the presence bits a word at
    /// a time (compressed: one `get64` per 64 items, the place of the next value taken past for
    /// each bit set, no branch on the bit), not a `match` per item.
    #[inline]
    fn fold<B, F: FnMut(B, Option<&'a T>) -> B>(self, mut acc: B, mut f: F) -> B {
        match self.0 {
            IterRepr::Flat(items) => items.fold(acc, |a, x| f(a, x.as_ref())),
            IterRepr::Compressed(nulls, mut values, bits) => {
                let left = nulls.len();
                if 256 <= left && left < values.len() << 3 && values.len() < left {
                    // long and mixed, more than an eighth values: `BitList`'s pieces of bools
                    return nulls.fold(acc, |a, p| f(a, if p { values.next() } else { None }));
                }
                let (end, values) = (bits.len(), values.as_slice());
                let (mut pos, mut n) = (end - nulls.len(), 0);
                while pos < end {
                    let w = bits.get64(pos);
                    let k = (end - pos).min(64);
                    if w == 0 {
                        for _ in 0..k {
                            acc = f(acc, None);
                        }
                    } else if w == u64::MAX >> (64 - k) {
                        // all values: the slice
                        for v in &values[n..n + k] {
                            acc = f(acc, Some(v));
                        }
                        n += k;
                    } else if w.count_ones() <= 16 {
                        // few values: the nulls by runs before each
                        let (mut w, mut j) = (w, 0);
                        while w != 0 {
                            let z = w.trailing_zeros() as usize;
                            for _ in 0..z {
                                acc = f(acc, None);
                            }
                            acc = f(acc, Some(&values[n]));
                            n += 1;
                            j += z + 1;
                            w = w >> z >> 1;
                        }
                        for _ in j..k {
                            acc = f(acc, None);
                        }
                    } else {
                        let mut w = w;
                        for _ in 0..k {
                            let p = (w & 1) as usize;
                            // SAFETY: the values left are one per set bit left, `n` - those taken
                            // past: at a set bit `n < values.len()`; at a null it is not read
                            acc = f(acc, if p != 0 { Some(unsafe { &*values.as_ptr().add(n) }) } else { None });
                            n += p;
                            w >>= 1;
                        }
                    }
                    pos += k;
                }
                acc
            }
        }
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}
impl<T> FusedIterator for Iter<'_, T> {}

enum IterMutRepr<'a, T> {
    Compressed(bit_list::Iter<'a>, core::slice::IterMut<'a, T>),
    Flat(core::slice::IterMut<'a, Option<T>>),
}

pub struct IterMut<'a, T>(IterMutRepr<'a, T>);

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = Option<&'a mut T>;

    #[inline]
    fn next(&mut self) -> Option<Option<&'a mut T>> {
        match &mut self.0 {
            IterMutRepr::Compressed(nulls, values) => Some(if nulls.next()? { values.next() } else { None }),
            IterMutRepr::Flat(items) => items.next().map(Option::as_mut),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IterMutRepr::Compressed(nulls, _) => nulls.size_hint(),
            IterMutRepr::Flat(items) => items.size_hint(),
        }
    }
}

impl<T> ExactSizeIterator for IterMut<'_, T> {}
impl<T> FusedIterator for IterMut<'_, T> {}

impl<'a, T> IntoIterator for &'a NullableObjectList<T> {
    type Item = Option<&'a T>;
    type IntoIter = Iter<'a, T>;
    #[inline]
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut NullableObjectList<T> {
    type Item = Option<&'a mut T>;
    type IntoIter = IterMut<'a, T>;
    #[inline]
    fn into_iter(self) -> IterMut<'a, T> {
        self.iter_mut()
    }
}

/// The items in order, by value.
pub struct IntoIter<T>(IntoIterRepr<T>);

enum IntoIterRepr<T> {
    Compressed(BitList, usize, std::vec::IntoIter<T>),
    Flat(std::vec::IntoIter<Option<T>>),
}

impl<T> Iterator for IntoIter<T> {
    type Item = Option<T>;

    #[inline]
    fn next(&mut self) -> Option<Option<T>> {
        match &mut self.0 {
            IntoIterRepr::Compressed(nulls, pos, values) => {
                if *pos == nulls.len() {
                    return None;
                }
                *pos += 1;
                Some(if nulls.get(*pos - 1) { values.next() } else { None })
            }
            IntoIterRepr::Flat(items) => items.next(),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match &self.0 {
            IntoIterRepr::Compressed(nulls, pos, _) => nulls.len() - pos,
            IntoIterRepr::Flat(items) => items.len(),
        };
        (n, Some(n))
    }
}

impl<T> ExactSizeIterator for IntoIter<T> {}
impl<T> FusedIterator for IntoIter<T> {}

impl<T> IntoIterator for NullableObjectList<T> {
    type Item = Option<T>;
    type IntoIter = IntoIter<T>;
    fn into_iter(self) -> IntoIter<T> {
        IntoIter(match self.repr {
            Repr::Compressed { nulls, values, .. } => IntoIterRepr::Compressed(nulls, 0, values.into_iter()),
            Repr::Flat { items, .. } => IntoIterRepr::Flat(items.into_iter()),
        })
    }
}

#[cfg(test)]
impl<T> NullableObjectList<T> {
    /// `ranks` as `nulls` makes them: one per word begun.
    pub(crate) fn assert_ranks(&self) {
        if let Repr::Compressed { nulls, values, ranks } = &self.repr {
            let want: Vec<usize> = (0..nulls.len().div_ceil(64)).map(|k| if k == 0 { 0 } else { nulls.rank((k << 6) - 1) }).collect();
            assert_eq!(*ranks, want, "ranks");
            assert_eq!(values.len(), nulls.count_ones(), "values");
        }
    }
}
