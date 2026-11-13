use super::NullablePrimitiveMap;
use std::{
    collections::HashMap,
    hash::{BuildHasher, Hasher, RandomState},
    rc::Rc,
};

#[derive(Clone, Default)]
struct Identity;
#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0.wrapping_mul(31).wrapping_add(byte as u64);
        }
    }
}
impl BuildHasher for Identity {
    type Hasher = IdentityHasher;
    fn build_hasher(&self) -> IdentityHasher {
        IdentityHasher::default()
    }
}

fn key(class: u64, suffix: u64) -> u64 {
    class << (usize::BITS - 3) | suffix
}

#[test]
fn shared_bits_survive_head_middle_and_tail_removal() {
    // All keys use bucket 5. First two also share the same filter bit.
    let keys = [key(1, 5), key(1, 69), key(2, 133), key(3, 197), key(4, 261)];
    for order in [[0, 2, 4, 1, 3], [4, 3, 2, 1, 0], [1, 0, 3, 2, 4]] {
        let mut map = NullablePrimitiveMap::with_capacity_and_hasher(32, Identity);
        for &k in &keys {
            map.insert(Some(k), k);
        }
        let mut present = [true; 5];
        for index in order {
            assert_eq!(map.remove(Some(keys[index])), Some(keys[index]));
            present[index] = false;
            for (i, &k) in keys.iter().enumerate() {
                assert_eq!(map.get(Some(k)).copied(), present[i].then_some(k));
            }
            assert_eq!(map.get(Some(key(7, 325))), None);
        }
        // Last deletion must reset the mask; reinsertion must set it again.
        assert!(map.is_empty());
        for class in 0..8 {
            let k = key(class, 5);
            assert_eq!(map.remove(Some(k)), None);
            assert_eq!(map.insert(Some(k), class), None);
            assert_eq!(map.get(Some(k)), Some(&class));
            assert_eq!(map.remove(Some(k)), Some(class));
        }
    }
}

#[test]
fn empty_clear_shrink_clone_and_null_key() {
    let mut map = NullablePrimitiveMap::with_hasher(Identity);
    let empty_clone = map.clone();
    assert_eq!(empty_clone.get(Some(0)), None);
    assert_eq!(map.capacity(), 0);
    for class in 0..8 {
        assert_eq!(map.get(Some(key(class, 0))), None);
    }
    map.insert(None, 99);
    assert_eq!(map.get(None), Some(&99));
    assert_eq!(map.capacity(), 0);
    for class in 0..8 {
        map.insert(Some(key(class, class)), class);
    }
    map.reserve(300);
    let mut clone = map.clone();
    map.clear();
    for class in 0..8 {
        assert_eq!(map.get(Some(key(class, class))), None);
    }
    assert_eq!(map.get(None), None);
    for class in 0..8 {
        assert_eq!(clone.get(Some(key(class, class))), Some(&class));
    }
    clone.retain(|k, _| k.is_none());
    clone.shrink_to_fit();
    assert_eq!(clone.capacity(), 0);
    assert_eq!(clone.get(None), Some(&99));
    for class in 0..8 {
        assert_eq!(clone.get(Some(key(class, class))), None);
    }
    clone.insert(Some(key(7, 5)), 77);
    assert_eq!(clone.get(Some(key(7, 5))), Some(&77));
    map.insert(Some(key(6, 5)), 66);
    assert_eq!(map.get(Some(key(6, 5))), Some(&66));
}

#[test]
fn collisions_growth_updates_and_lazy_values() {
    let mut map = NullablePrimitiveMap::with_hasher(Identity);
    for i in 0..512u64 {
        let k = key(i % 8, i * 2048 + 5);
        assert_eq!(map.insert(Some(k), i), None);
    }
    for i in 0..512u64 {
        let k = key(i % 8, i * 2048 + 5);
        assert_eq!(map.insert(Some(k), i + 1), Some(i));
        assert_eq!(*map.get_or_insert_with(Some(k), || panic!("existing key")), i + 1);
        *map.get_key_mut(k).unwrap() += 1;
    }
    for i in 0..512u64 {
        let k = key(i % 8, i * 2048 + 5);
        assert_eq!(map.get_key(k), Some(&(i + 2)));
    }
    let missing = key(0, 512 * 2048 + 5);
    assert_eq!(*map.get_or_insert_with(Some(missing), || 123), 123);
    map.retain(|k, _| k != Some(missing));
    assert_eq!(map.get(Some(missing)), None);
    for i in (0..512u64).rev() {
        assert_eq!(map.remove(Some(key(i % 8, i * 2048 + 5))), Some(i + 2));
    }
    assert!(map.is_empty());
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn differential<S: BuildHasher + Clone>(hasher: S, seed: u64) {
    let token = Rc::new(());
    let mut map = NullablePrimitiveMap::with_hasher(hasher);
    let mut model = HashMap::new();
    let mut rng = Rng(seed | 1);
    for step in 0..20_000 {
        let k = if rng.next() % 13 == 0 { None } else { Some(key(rng.next() % 8, rng.next() % 1024)) };
        let v = rng.next();
        match rng.next() % 100 {
            0..=34 => assert_eq!(map.insert(k, (v, token.clone())).map(|x| x.0), model.insert(k, v)),
            35..=59 => assert_eq!(map.remove(k).map(|x| x.0), model.remove(&k)),
            60..=69 => {
                assert_eq!(map.get(k).map(|x| x.0), model.get(&k).copied());
                assert_eq!(map.contains_key(k), model.contains_key(&k));
            }
            70..=74 => {
                if let Some(x) = map.get_mut(k) {
                    x.0 ^= 1;
                }
                if let Some(x) = model.get_mut(&k) {
                    *x ^= 1;
                }
            }
            75..=79 => assert_eq!(map.get_or_insert_with(k, || (v, token.clone())).0, *model.entry(k).or_insert(v)),
            80..=82 => {
                map.retain(|_, x| (x.0 ^ v) % 3 != 0);
                model.retain(|_, x| (*x ^ v) % 3 != 0);
            }
            83..=84 => map.shrink_to_fit(),
            85..=86 => map.reserve((v % 800) as usize),
            87 => {
                let clone = map.clone();
                assert!(clone == map);
                map = clone;
            }
            88 if v % 8 == 0 => {
                map.clear();
                model.clear();
            }
            _ => {}
        }
        assert_eq!(map.len(), model.len());
        if step % 97 == 0 {
            for (&k, &v) in &model {
                assert_eq!(map.get(k).map(|x| x.0), Some(v));
            }
            for (k, v) in map.iter() {
                assert_eq!(model.get(&k), Some(&v.0));
            }
            // Fresh absent keys exercise both zero and stale nonzero masks.
            for class in 0..8 {
                assert_eq!(map.get(Some(key(class, 1024 + step))), None);
            }
        }
    }
    assert_eq!(Rc::strong_count(&token), 1 + map.len());
    drop(map);
    assert_eq!(Rc::strong_count(&token), 1);
}

#[test]
fn randomized_operations_match_std_with_drop_tracking() {
    for seed in 1..=8 {
        differential(RandomState::new(), seed);
        differential(rustc_hash::FxBuildHasher, seed);
        differential(Identity, seed);
    }
}

#[test]
fn lazy_value_panic_does_not_change_filters() {
    let mut map = NullablePrimitiveMap::with_capacity_and_hasher(4, Identity);
    map.insert(Some(key(1, 5)), 11);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        map.get_or_insert_with(Some(key(7, 5)), || panic!("value construction"));
    }));
    assert!(result.is_err());
    assert_eq!(map.get(Some(key(1, 5))), Some(&11));
    assert_eq!(map.get(Some(key(7, 5))), None);
    assert_eq!(map.len(), 1);
    assert_eq!(map.insert(Some(key(7, 5)), 77), None);
    assert_eq!(map.get(Some(key(7, 5))), Some(&77));
}
