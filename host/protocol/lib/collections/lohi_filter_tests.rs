use super::{empty_filters, RawIndex, RawTable};
use std::{cell::Cell, panic::{catch_unwind, AssertUnwindSafe}, rc::Rc};

fn assert_filters<V, I: RawIndex, const BITS: u32>(table: &RawTable<u64, V, I, BITS>) {
    if table.capacity() == 0 {
        assert_eq!(table.filters, empty_filters());
    }
    for bucket in 0..=table.bucket_mask {
        let head = unsafe { (*table.buckets.as_ptr().add(bucket)).get() };
        let filter = unsafe { *table.filters.as_ptr().add(bucket) };
        assert_eq!(head == 0, filter == 0);
        if head != 0 {
            let mut i = head - 1;
            loop {
                let hash = unsafe { *table.key(i) };
                assert_ne!(filter & table.filter_bit(hash), 0);
                if i >= table.lo_size { break; }
                i = unsafe { table.link(i) };
            }
        }
    }
}

fn removals<I: RawIndex, const BITS: u32>(keys: [u64; 5], missing: u64) {
    for order in [[0, 2, 4, 1, 3], [4, 3, 2, 1, 0], [1, 0, 3, 2, 4]] {
        let mut table = RawTable::<u64, (), I, BITS>::with_capacity(32);
        for key in keys { table.insert_unique(key, key, (), |v| *v); }
        assert_filters(&table);
        // Even though the bucket is occupied, the absent class reads no entries.
        assert!(table.find(missing, |_| panic!("miss entered a chain")).is_none());
        assert!(table.get_mut(missing, |_| panic!("mutable miss entered a chain")).is_none());
        assert!(table.remove(missing, |_| panic!("remove miss entered a chain"), |v| *v).is_none());
        let mut present = [true; 5];
        for index in order {
            assert_eq!(table.remove(keys[index], |v| *v == keys[index], |v| *v), Some((keys[index], ())));
            present[index] = false;
            assert_filters(&table);
            for (i, key) in keys.iter().enumerate() {
                assert_eq!(table.get(*key, |v| v == key).map(|(k, _)| *k), present[i].then_some(*key));
            }
        }
        assert_eq!(table.len(), 0);
        // Last removal clears conservative bits, including bits shared by two old keys.
        for key in keys {
            assert!(table.find(key, |_| panic!("empty bucket entered a chain")).is_none());
            table.insert_unique(key, key, (), |v| *v);
            assert_filters(&table);
            assert_eq!(table.remove(key, |v| *v == key, |v| *v), Some((key, ())));
        }
    }
}

#[test]
fn native_hash_classes_survive_chain_removal() {
    let key = |class: u64, suffix: u64| class << (usize::BITS - 3) | suffix;
    removals::<u32, { usize::BITS }>([key(1, 5), key(1, 69), key(2, 133), key(3, 197), key(4, 261)], key(7, 325));
}

#[test]
fn cached_32_bit_classes_survive_chain_removal() {
    let key = |class: u64, suffix: u64| class << 29 | suffix;
    removals::<u32, 32>([key(1, 5), key(1, 69), key(2, 133), key(3, 197), key(4, 261)], key(7, 325));
}

#[test]
fn two_byte_32_bit_classes_survive_chain_removal() {
    // The table of TwoBytesMap: u16 indices, a 32-bit hash with the filter bit in its top three.
    let key = |class: u64, suffix: u64| class << 29 | suffix;
    removals::<u16, 32>([key(1, 5), key(1, 69), key(2, 133), key(3, 197), key(4, 261)], key(7, 325));
}

fn lifecycle<I: RawIndex, const BITS: u32>(key: impl Fn(u64) -> u64) {
    let mut table = RawTable::<u64, (), I, BITS>::new();
    assert_filters(&table);
    for i in 0..300 {
        let h = key(i);
        table.insert_unique(h, h, (), |v| *v);
        assert_filters(&table);
    }
    table.reserve(1000, |v| *v);
    assert_filters(&table);
    let mut clone = table.clone();
    assert_filters(&clone);
    table.clear();
    assert_filters(&table);
    table.shrink_to_fit(|v| *v);
    assert_eq!(table.capacity(), 0);
    assert_filters(&table);
    clone.retain(|v, _| (0..40).any(|i| key(i) == *v), |v| *v);
    assert_eq!(clone.len(), 40);
    clone.shrink_to_fit(|v| *v);
    assert_filters(&clone);
    for i in 0..300 {
        let h = key(i);
        assert_eq!(clone.get(h, |v| *v == h).map(|(k, _)| *k), (i < 40).then_some(h));
    }
    let mut owned = clone.into_iter();
    assert_eq!(owned.len(), 40);
    assert!(owned.next().is_some());
    assert_eq!(owned.len(), 39);
}

#[test]
fn resize_retain_clone_clear_and_shrink_rebuild_all_hash_widths() {
    lifecycle::<u32, { usize::BITS }>(|i| (i % 8) << (usize::BITS - 3) | (i * 2048 + 5));
    lifecycle::<u32, 32>(|i| (i % 8) << 29 | (i * 2048 + 5));
    lifecycle::<u16, 32>(|i| (i % 8) << 29 | (i * 193 % 65536));
}

#[test]
fn lazy_panic_does_not_set_filter_and_replacement_does_not_grow() {
    let mut table = RawTable::<u64, u64>::with_capacity(4);
    for i in 0..4 { table.insert_unique(i, i, i, |v| *v); }
    let missing = 7 << (usize::BITS - 3) | 8;
    assert!(catch_unwind(AssertUnwindSafe(|| {
        table.find_or_insert(missing, |_| panic!("missing class compared"), || panic!("factory"), |v| *v);
    })).is_err());
    assert_filters(&table);
    assert_eq!(table.capacity(), 4);
    let (existed, value) = table.find_or_insert(2, |v| *v == 2, || panic!("existing factory"), |v| *v);
    assert!(existed);
    assert_eq!(*value, 2);
    assert_eq!(table.capacity(), 4);
    assert_filters(&table);
}

struct FallibleClone { hash: u64, token: Rc<()>, remaining: Rc<Cell<usize>> }
impl Clone for FallibleClone {
    fn clone(&self) -> Self {
        let n = self.remaining.get();
        assert_ne!(n, 0, "clone panic");
        self.remaining.set(n - 1);
        Self { hash: self.hash, token: self.token.clone(), remaining: self.remaining.clone() }
    }
}

#[test]
fn partially_cloned_tables_drop_only_initialized_values() {
    let token = Rc::new(());
    let remaining = Rc::new(Cell::new(0));
    let mut table = RawTable::<FallibleClone>::new();
    for i in 0..32 {
        let hash = (i % 8) << (usize::BITS - 3) | 5;
        table.insert_unique(hash, FallibleClone { hash, token: token.clone(), remaining: remaining.clone() }, (), |v| v.hash);
    }
    for n in [0, 1, 5, 17, 31] {
        remaining.set(n);
        assert!(catch_unwind(AssertUnwindSafe(|| table.clone())).is_err());
        assert_eq!(Rc::strong_count(&token), 33);
    }
    remaining.set(32);
    let clone = table.clone();
    assert_eq!(Rc::strong_count(&token), 65);
    drop(clone);
    assert_eq!(Rc::strong_count(&token), 33);
    drop(table);
    assert_eq!(Rc::strong_count(&token), 1);
}
