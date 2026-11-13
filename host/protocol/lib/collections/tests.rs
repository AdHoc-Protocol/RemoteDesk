//! Differential tests: every collection against a `std` model, under random operations.
//!
//! The hash collections also run with a deliberately weak hasher (8 distinct hashes) so that chains
//! grow long and every branch of removal and relocation is exercised. Values are `Rc`s: a leak or a
//! double drop shows up in the strong count.
//!
//! `FUZZ=n cargo test` runs the randomized tests with `n` times as many seeds.

#![allow(clippy::manual_is_multiple_of, clippy::unnecessary_map_or)]

use super::*;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::rc::Rc;

/// Seeds `0..n * FUZZ`.
fn seeds(n: u64) -> std::ops::Range<u64> {
    let scale = std::env::var("FUZZ").ok().and_then(|s| s.parse().ok()).unwrap_or(1u64);
    0..n * scale
}

/// xorshift64*: deterministic, no dependencies.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// At most 8 distinct hashes: long chains everywhere.
#[derive(Clone, Default)]
struct Weak;

struct WeakHasher(u64);

impl Hasher for WeakHasher {
    fn finish(&self) -> u64 {
        self.0 % 8
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = self.0.wrapping_mul(31).wrapping_add(*b as u64);
        }
    }
}

impl BuildHasher for Weak {
    type Hasher = WeakHasher;
    fn build_hasher(&self) -> WeakHasher {
        WeakHasher(0)
    }
}

// hash maps

fn primitive_map_run<S: BuildHasher + Default + Clone>(seed: u64, key_range: u64, ops: usize) {
    let token = Rc::new(());
    let mut rng = Rng::new(seed);
    let mut m: PrimitiveMap<u32, (u64, Rc<()>), S> = PrimitiveMap::with_hasher(S::default());
    let mut model: HashMap<u32, u64> = HashMap::new();
    for step in 0..ops {
        let k = rng.below(key_range) as u32;
        match rng.below(100) {
            0..=39 => {
                let v = rng.next();
                let old = m.insert(k, (v, token.clone())).map(|(v, _)| v);
                assert_eq!(old, model.insert(k, v), "insert {k}");
            }
            40..=69 => assert_eq!(m.remove(k).map(|(v, _)| v), model.remove(&k), "remove {k}"),
            70..=84 => assert_eq!(m.get(k).map(|(v, _)| *v), model.get(&k).copied(), "get {k}"),
            85..=89 => {
                let v = rng.next();
                let got = m.get_or_insert_with(k, || (v, token.clone())).0;
                assert_eq!(got, *model.entry(k).or_insert(v));
            }
            90..=92 => {
                if let Some(x) = m.get_mut(k) {
                    x.0 += 1;
                    *model.get_mut(&k).unwrap() += 1;
                } else {
                    assert!(!model.contains_key(&k));
                }
            }
            93 => {
                let r = rng.next() | 1;
                m.retain(|k, v| (k as u64 ^ v.0 ^ r) % 3 != 0);
                model.retain(|k, v| (*k as u64 ^ *v ^ r) % 3 != 0);
            }
            94 => m.shrink_to_fit(),
            95 => m.reserve(rng.below(100) as usize),
            96 => {
                let c = m.clone();
                assert_eq!(c.len(), m.len());
                assert!(c.iter().all(|(k, v)| m.get(k).map(|x| x.0) == Some(v.0)));
                m = c;
            }
            97 if step % 50 == 0 => {
                m.clear();
                model.clear();
            }
            _ => {}
        }
        assert_eq!(m.len(), model.len());
        if step % 97 == 0 {
            let mut got: Vec<(u32, u64)> = m.iter().map(|(k, v)| (k, v.0)).collect();
            got.sort_unstable();
            let mut want: Vec<(u32, u64)> = model.iter().map(|(k, v)| (*k, *v)).collect();
            want.sort_unstable();
            assert_eq!(got, want, "contents at step {step}");
            assert_eq!(m.iter().count(), m.len());
            assert_eq!(m.iter().fold(0, |n, _| n + 1), m.len());
        }
    }
    assert_eq!(Rc::strong_count(&token), 1 + m.len());
    let collected: Vec<_> = m.into_iter().collect();
    assert_eq!(collected.len(), model.len());
    drop(collected);
    assert_eq!(Rc::strong_count(&token), 1);
}

#[test]
fn primitive_map_matches_std() {
    for seed in seeds(8) {
        primitive_map_run::<rustc_hash::FxBuildHasher>(seed, 64, 4000);
        primitive_map_run::<rustc_hash::FxBuildHasher>(seed, 5000, 20000);
        primitive_map_run::<Weak>(seed, 64, 4000);
        primitive_map_run::<Weak>(seed, 600, 8000);
    }
}

fn map_run<S: BuildHasher + Default + Clone>(seed: u64, key_range: u64, ops: usize) {
    let token = Rc::new(());
    let mut rng = Rng::new(seed);
    let mut m: Map<String, (u64, Rc<()>), S> = Map::with_hasher(S::default());
    let mut model: HashMap<String, u64> = HashMap::new();
    for step in 0..ops {
        let k = format!("key{}", rng.below(key_range));
        match rng.below(100) {
            0..=39 => {
                let v = rng.next();
                assert_eq!(m.insert(k.clone(), (v, token.clone())).map(|(v, _)| v), model.insert(k, v));
            }
            40..=69 => assert_eq!(m.remove(k.as_str()).map(|(v, _)| v), model.remove(&k)),
            70..=84 => assert_eq!(m.get(k.as_str()).map(|(v, _)| *v), model.get(&k).copied()),
            85..=89 => {
                let v = rng.next();
                let got = m.get_or_insert_with(k.clone(), || (v, token.clone())).0;
                assert_eq!(got, *model.entry(k).or_insert(v));
            }
            90..=92 => {
                let r = rng.next() | 1;
                m.retain(|_, v| (v.0 ^ r) % 3 != 0);
                model.retain(|_, v| (*v ^ r) % 3 != 0);
            }
            93 => m.shrink_to_fit(),
            94 => {
                let c = m.clone();
                assert!(c == m);
                m = c;
            }
            _ => {}
        }
        assert_eq!(m.len(), model.len());
        if step % 89 == 0 {
            for (k, v) in &model {
                assert_eq!(m.get(k.as_str()).map(|x| x.0), Some(*v));
            }
        }
    }
    assert_eq!(Rc::strong_count(&token), 1 + m.len());
    drop(m);
    assert_eq!(Rc::strong_count(&token), 1);
}

#[test]
fn map_matches_std() {
    for seed in seeds(6) {
        map_run::<rustc_hash::FxBuildHasher>(seed, 300, 6000);
        map_run::<Weak>(seed, 100, 4000);
    }
}

#[test]
fn nullable_maps() {
    let mut m: NullableMap<String, u32> = NullableMap::new();
    assert_eq!(m.insert(None, 1), None);
    assert_eq!(m.insert(Some("a".into()), 2), None);
    assert_eq!(m.insert(None, 3), Some(1));
    assert_eq!(m.len(), 2);
    assert_eq!(m.null_value(), Some(&3));
    assert_eq!(m.get(Some("a")), Some(&2));
    assert_eq!(m.iter().next(), Some((None, &3)));
    assert_eq!(m.iter().count(), 2);
    assert_eq!(m.remove_null(), Some(3));
    assert_eq!(m.len(), 1);

    let mut p: NullablePrimitiveMap<u16, u32> = NullablePrimitiveMap::new();
    p.insert(None, 7);
    p.insert(Some(1), 8);
    assert_eq!(p.get(None), Some(&7));
    assert_eq!(p.keys().collect::<Vec<_>>(), vec![None, Some(1)]);
}

// hash sets

fn primitive_set_run<S: BuildHasher + Default + Clone>(seed: u64, key_range: u64, ops: usize) {
    let mut rng = Rng::new(seed);
    let mut s: NullablePrimitiveSet<u64, S> = NullablePrimitiveSet::with_hasher(S::default());
    let mut model: HashSet<Option<u64>> = HashSet::new();
    for step in 0..ops {
        let k = match rng.below(key_range + 1) {
            0 => None,
            x => Some(x * 1_000_003),
        };
        match rng.below(10) {
            0..=3 => assert_eq!(s.insert(k), model.insert(k)),
            4..=6 => assert_eq!(s.remove(k), model.remove(&k)),
            7 => assert_eq!(s.contains(k), model.contains(&k)),
            8 if step % 31 == 0 => {
                let r = rng.next();
                s.retain(|k| k.map_or(true, |k| (k ^ r) % 4 != 0));
                model.retain(|k| k.map_or(true, |k| (k ^ r) % 4 != 0));
            }
            _ => {}
        }
        assert_eq!(s.len(), model.len());
        if step % 53 == 0 {
            let got: HashSet<Option<u64>> = s.iter().collect();
            assert_eq!(got, model);
        }
    }
}

#[test]
fn primitive_sets_match_std() {
    for seed in seeds(8) {
        primitive_set_run::<rustc_hash::FxBuildHasher>(seed, 50, 5000);
        primitive_set_run::<rustc_hash::FxBuildHasher>(seed, 3000, 20000);
        primitive_set_run::<Weak>(seed, 200, 6000);
    }
}

#[test]
fn set_algebra() {
    let a: PrimitiveSet<u32> = (0..100).collect();
    let b: PrimitiveSet<u32> = (50..150).collect();
    let mut i = a.clone();
    i.intersect_with(&b);
    assert_eq!(i, (50..100).collect());
    let mut d = a.clone();
    d.difference_with(b.iter());
    assert_eq!(d, (0..50).collect());
    let mut x = a.clone();
    x.symmetric_difference_with(b.iter());
    assert_eq!(x, (0..50).chain(100..150).collect());
    assert!(i.is_subset(&a) && a.is_superset(&i) && !a.is_subset(&b));
    assert!(d.is_disjoint(&b));

    let mut n: NullableSet<String> = ["x".to_string(), "y".to_string()].into_iter().collect();
    n.insert_null();
    let m: NullableSet<String> = ["y".to_string()].into_iter().collect();
    assert!(m.is_subset(&n));
    n.difference_with(&m);
    assert_eq!(n.len(), 2);
    assert!(n.contains_null() && n.contains("x") && !n.contains("y"));
    n.intersect_with(&m);
    assert!(n.is_empty());
}

fn nullable_set_run<S: BuildHasher + Default + Clone>(seed: u64, key_range: u64, ops: usize) {
    let mut rng = Rng::new(seed);
    let mut s: NullableSet<String, S> = NullableSet::with_hasher(S::default());
    let mut model: HashSet<Option<String>> = HashSet::new();
    for step in 0..ops {
        let k = match rng.below(key_range + 1) {
            0 => None,
            x => Some(format!("k{x}")),
        };
        match rng.below(10) {
            0..=3 => assert_eq!(s.insert_opt(k.clone()), model.insert(k)),
            4..=6 => assert_eq!(s.remove_opt(k.as_deref()), model.remove(&k)),
            _ => assert_eq!(s.contains_opt(k.as_deref()), model.contains(&k)),
        }
        assert_eq!(s.len(), model.len());
        if step % 53 == 0 {
            let got: HashSet<Option<String>> = s.iter_opt().map(|k| k.cloned()).collect();
            assert_eq!(got, model);
        }
    }
}

#[test]
fn nullable_set_matches_std() {
    for seed in seeds(6) {
        nullable_set_run::<rustc_hash::FxBuildHasher>(seed, 400, 6000);
        nullable_set_run::<Weak>(seed, 100, 4000);
    }
}

// byte collections

#[test]
fn byte_set_matches_model() {
    let mut rng = Rng::new(7);
    let mut s: ByteSet<i8> = ByteSet::new();
    let mut model = std::collections::BTreeSet::<u8>::new();
    for _ in 0..20000 {
        let k = rng.next() as u8;
        match rng.below(3) {
            0 => assert_eq!(s.insert(k as i8), model.insert(k)),
            1 => assert_eq!(s.remove(k as i8), model.remove(&k)),
            _ => assert_eq!(s.contains(k as i8), model.contains(&k)),
        }
        assert_eq!(s.len(), model.len());
        let probe = rng.next() as u8;
        assert_eq!(s.count_less_than(probe as i8), model.range(..probe).count());
        assert_eq!(s.rank(probe as i8), model.range(..=probe).count());
        assert_eq!(
            s.next_after(probe as i8).map(|k| k as u8),
            model.range(probe.saturating_add(1)..).next().copied().filter(|_| probe < 255)
        );
        assert_eq!(s.prev_before(probe as i8).map(|k| k as u8), model.range(..probe).next_back().copied());
    }
    // Iteration is ascending by byte value: 0..=127, then -128..=-1.
    assert_eq!(s.iter().map(|k| k as u8).collect::<Vec<_>>(), model.iter().copied().collect::<Vec<_>>());
    assert_eq!(s.first().map(|k| k as u8), model.first().copied());
    assert_eq!(s.last().map(|k| k as u8), model.last().copied());

    let a: ByteSet = (0..=100).collect();
    let b: ByteSet = (50..=200).collect();
    assert_eq!((a & b).len(), 51);
    assert_eq!((a | b).len(), 201);
    assert_eq!((a - b).len(), 50);
    assert_eq!((a ^ b).len(), 150);
    assert!((a & b).is_subset(&a) && !a.is_subset(&b) && (a - b).is_disjoint(&b));

    let mut n: NullableByteSet = [None, Some(3u8), Some(250)].into_iter().collect();
    assert_eq!(n.len(), 3);
    assert_eq!(n.iter().collect::<Vec<_>>(), vec![None, Some(3), Some(250)]);
    assert!(n.remove(None) && !n.contains(None));
}

fn byte_map_run(seed: u64) {
    let token = Rc::new(());
    let mut rng = Rng::new(seed);
    let mut m: ByteMap<u8, (u32, Rc<()>)> = ByteMap::new();
    let mut model = std::collections::BTreeMap::<u8, u32>::new();
    for step in 0..30000 {
        let k = rng.next() as u8;
        match rng.below(20) {
            0..=8 => {
                let v = rng.next() as u32;
                assert_eq!(m.insert(k, (v, token.clone())).map(|x| x.0), model.insert(k, v));
            }
            9..=13 => assert_eq!(m.remove(k).map(|x| x.0), model.remove(&k)),
            14..=16 => assert_eq!(m.get(k).map(|x| x.0), model.get(&k).copied()),
            17 => {
                let v = rng.next() as u32;
                assert_eq!(m.get_or_insert_with(k, || (v, token.clone())).0, *model.entry(k).or_insert(v));
            }
            18 if step % 7 == 0 => {
                let r = rng.next() as u32;
                m.retain(|k, v| (k as u32 ^ v.0 ^ r) % 4 != 0);
                model.retain(|k, v| (*k as u32 ^ *v ^ r) % 4 != 0);
            }
            19 if step % 11 == 0 => {
                m.shrink_to_fit();
                let c = m.clone();
                assert!(c == m);
                m = c;
            }
            _ => {}
        }
        assert_eq!(m.len(), model.len());
        if step % 101 == 0 {
            let got: Vec<(u8, u32)> = m.iter().map(|(k, v)| (k, v.0)).collect();
            let want: Vec<(u8, u32)> = model.iter().map(|(k, v)| (*k, *v)).collect();
            assert_eq!(got, want);
        }
        if step == 15000 {
            m.clear();
            model.clear();
        }
    }
    assert_eq!(Rc::strong_count(&token), 1 + m.len());
    let half: Vec<_> = m.into_iter().take(model.len() / 2).collect();
    drop(half);
    assert_eq!(Rc::strong_count(&token), 1);
}

#[test]
fn byte_map_matches_model() {
    for seed in seeds(6) {
        byte_map_run(seed);
    }
    // Values of 16 bytes at most are flat from the first key: no rank to count
    let mut s: ByteMap<u8, u32> = ByteMap::new();
    s.insert(7, 70);
    assert!(s.is_flat());
    assert_eq!((s.get(7), s.get(8)), (Some(&70), None));
    assert_eq!((s.insert(7, 71), s.len()), (Some(70), 1));
    // Larger ones: both layouts, the switch on the 129th key and back.
    let mut m: ByteMap<u8, [u32; 5]> = (0..128u8).rev().map(|k| (k, [k as u32; 5])).collect(); // in reverse: inserts in front
    assert!(!m.is_flat());
    assert_eq!(m.iter().map(|(k, v)| (k as u32, v[4])).collect::<Vec<_>>(), (0..128u32).map(|k| (k, k)).collect::<Vec<_>>());
    m.insert(200, [1; 5]);
    assert!(m.is_flat());
    assert_eq!(m.get(5), Some(&[5; 5]));
    m.remove(200);
    m.shrink_to_fit();
    assert!(!m.is_flat());
    assert_eq!(m.values().map(|v| v[0]).sum::<u32>(), (0..128u32).sum::<u32>());

    let mut n: NullableByteMap<u8, &str> = NullableByteMap::new();
    n.insert(None, "null");
    n.insert(Some(1), "one");
    assert_eq!(n.iter().collect::<Vec<_>>(), vec![(None, &"null"), (Some(1), &"one")]);
}

// two-byte collections

fn two_bytes_map_run(seed: u64, key_range: u64) {
    let token = Rc::new(());
    let mut rng = Rng::new(seed);
    let mut m: TwoBytesMap<u16, (u64, Rc<()>)> = TwoBytesMap::new();
    let mut model: HashMap<u16, u64> = HashMap::new();
    let mut went_flat = false;
    for step in 0..60000 {
        // Patterned keys collide under the identity hash.
        let k = (rng.below(key_range) as u16).wrapping_mul(if seed % 2 == 0 { 1 } else { 256 });
        match rng.below(20) {
            0..=9 => {
                let v = rng.next();
                assert_eq!(m.insert(k, (v, token.clone())).map(|x| x.0), model.insert(k, v));
            }
            10..=13 => assert_eq!(m.remove(k).map(|x| x.0), model.remove(&k)),
            14..=17 => assert_eq!(m.get(k).map(|x| x.0), model.get(&k).copied()),
            18 if step % 13 == 0 => {
                let r = rng.next();
                m.retain(|k, v| (k as u64 ^ v.0 ^ r) % 5 != 0);
                model.retain(|k, v| (*k as u64 ^ *v ^ r) % 5 != 0);
            }
            19 if step % 17 == 0 => m.shrink_to_fit(),
            _ => {}
        }
        went_flat |= m.is_flat();
        assert_eq!(m.len(), model.len());
        if step % 997 == 0 {
            let mut got: Vec<(u16, u64)> = m.iter().map(|(k, v)| (k, v.0)).collect();
            got.sort_unstable();
            let mut want: Vec<(u16, u64)> = model.iter().map(|(k, v)| (*k, *v)).collect();
            want.sort_unstable();
            assert_eq!(got, want);
        }
    }
    let _ = went_flat;
    assert_eq!(Rc::strong_count(&token), 1 + m.len());
    let c = m.clone();
    assert_eq!(Rc::strong_count(&token), 1 + 2 * m.len());
    drop(c);
    drop(m);
    assert_eq!(Rc::strong_count(&token), 1);
}

#[test]
fn two_bytes_map_matches_std() {
    for seed in seeds(4) {
        two_bytes_map_run(seed, 300);
        two_bytes_map_run(seed, 40000);
    }
}

#[test]
fn two_bytes_set_layouts() {
    let mut s: TwoBytesSet<i16> = TwoBytesSet::new();
    for k in 0..512i16 {
        assert!(s.insert(k * 7));
    }
    assert!(!s.is_flat());
    assert!(s.insert(-1));
    assert!(s.is_flat(), "a set goes flat past 512 keys with bucket filters");
    assert_eq!(s.len(), 513);
    assert!(s.contains(-1) && s.contains(7) && !s.contains(8));
    for k in 0..488i16 {
        assert!(s.remove(k * 7));
    }
    s.shrink_to_fit();
    assert!(!s.is_flat());
    assert_eq!(s.len(), 25);
    assert!(s.contains(-1) && s.contains(488 * 7));

    let mut n: NullableTwoBytesSet<u16> = NullableTwoBytesSet::new();
    assert!(n.insert(None) && n.insert(Some(5)) && !n.insert(Some(5)));
    assert_eq!(n.iter().collect::<Vec<_>>(), vec![None, Some(5)]);

    let mut nm: NullableTwoBytesMap<u16, u8> = NullableTwoBytesMap::new();
    nm.insert(None, 1);
    nm.insert(Some(9), 2);
    assert_eq!(nm.get(None), Some(&1));
    assert_eq!(nm.len(), 2);

    // Small values: the flat layout wins well before 32 768 keys.
    let mut m: TwoBytesMap<u16, u8> = TwoBytesMap::new();
    for k in 0..20000u16 {
        m.insert(k.wrapping_mul(3), k as u8);
    }
    assert!(m.is_flat());
    assert_eq!(m.len(), 20000);
    assert!((0..20000u16).all(|k| m.get(k.wrapping_mul(3)) == Some(&(k as u8))));
}

// BitList

fn model_of(l: &BitList) -> Vec<bool> {
    (0..l.len()).map(|i| l.get(i)).collect()
}

fn list_of(m: &[bool]) -> BitList {
    m.iter().copied().collect()
}

/// A random bit vector with long runs: leading ones, then runs of ones and zeros.
fn runs(rng: &mut Rng, max_len: u64) -> Vec<bool> {
    let len = rng.below(max_len + 1) as usize;
    let mut v = Vec::with_capacity(len);
    let mut bit = rng.below(2) == 0;
    while v.len() < len {
        let max_run = if rng.below(4) == 0 { 200 } else { 8 };
        let run = 1 + rng.below(max_run) as usize;
        for _ in 0..run.min(len - v.len()) {
            v.push(bit);
        }
        bit = !bit;
    }
    v
}

fn check(l: &BitList, m: &[bool], what: &str) {
    l.assert_canonical();
    assert_eq!(l.len(), m.len(), "{what}: len");
    assert_eq!(model_of(l), m, "{what}: bits");
    assert_eq!(l.iter().collect::<Vec<_>>(), m, "{what}: iter");
}

#[test]
fn bit_list_reads_match_model() {
    let mut rng = Rng::new(42);
    for _ in 0..400 * seeds(1).end {
        let m = runs(&mut rng, 700);
        let l = list_of(&m);
        check(&l, &m, "collect");
        assert_eq!(l, l.clone());
        let ones: Vec<usize> = (0..m.len()).filter(|&i| m[i]).collect();
        assert_eq!(l.ones().collect::<Vec<_>>(), ones);
        assert_eq!(l.count_ones(), ones.len());
        assert_eq!(l.last_one(), ones.last().copied());
        for (n, &i) in ones.iter().enumerate() {
            assert_eq!(l.nth_one(n), Some(i));
        }
        assert_eq!(l.nth_one(ones.len()), None);
        for from in 0..=m.len() + 2 {
            assert_eq!(l.next_one(from), (from..m.len()).find(|&i| m[i]), "next_one {from}");
            assert_eq!(l.next_zero(from), (from..m.len()).find(|&i| !m[i]), "next_zero {from}");
            assert_eq!(l.prev_one(from), (0..from.min(m.len())).rev().find(|&i| m[i]), "prev_one {from}");
            assert_eq!(l.prev_zero(from), (0..from.min(m.len())).rev().find(|&i| !m[i]), "prev_zero {from}");
            if from < m.len() {
                assert_eq!(l.rank(from), m[..=from].iter().filter(|b| **b).count(), "rank {from}");
            }
            let want = (0..64).fold(0u64, |w, j| w | ((from + j < m.len() && m[from + j]) as u64) << j);
            assert_eq!(l.get64(from), want, "get64 {from}");
        }
        assert_eq!(l.leading_ones(), m.iter().rev().take_while(|b| **b).count());
        assert_eq!(l.leading_zeros(), m.iter().rev().take_while(|b| !**b).count());
        assert_eq!(l.trailing_zeros(), m.iter().take_while(|b| !**b).count());
        assert_eq!(l.trailing_ones(), m.iter().take_while(|b| **b).count());

        let m2 = runs(&mut rng, 700);
        let l2 = list_of(&m2);
        let lim = m.len().min(m2.len());
        assert_eq!(l.find_first_difference(&l2), (0..lim).find(|&i| m[i] != m2[i]).unwrap_or(lim));
        assert_eq!(l.intersects(&l2), (0..lim).any(|i| m[i] && m2[i]));

        let mut dst = [0u64; 4];
        let from = rng.below(m.len() as u64 + 1) as usize;
        let to = rng.below(m.len() as u64 + 300) as usize;
        let n = l.get_words(&mut dst, from, to);
        assert_eq!(n, to.min(m.len()).saturating_sub(from).min(256));
        for j in 0..n {
            assert_eq!(dst[j / 64] >> (j % 64) & 1 != 0, m[from + j]);
        }
    }
}

fn grow_to(m: &mut Vec<bool>, len: usize) {
    if len > m.len() {
        m.resize(len, false);
    }
}

#[test]
fn bit_list_writes_match_model() {
    let mut rng = Rng::new(9);
    for round in 0..300 * seeds(1).end {
        let mut m = runs(&mut rng, 400);
        let mut l = list_of(&m);
        for step in 0..60 {
            let bound = m.len() as u64 + 70;
            let a = rng.below(bound) as usize;
            let b = rng.below(bound) as usize;
            let (lo, hi) = (a.min(b), a.max(b));
            let op = rng.below(16);
            let what = format!("round {round} step {step} op {op} a {a} b {b}");
            match op {
                0 => {
                    l.set1(a);
                    grow_to(&mut m, a + 1);
                    m[a] = true;
                }
                1 => {
                    l.set0(a);
                    grow_to(&mut m, a + 1);
                    m[a] = false;
                }
                2 => {
                    let v = rng.below(2) == 0;
                    l.set_range(lo..hi, v);
                    if lo < hi {
                        grow_to(&mut m, hi);
                        m[lo..hi].fill(v);
                    }
                }
                3 => {
                    l.flip(a);
                    grow_to(&mut m, a + 1);
                    m[a] = !m[a];
                }
                4 => {
                    l.flip_range(lo..hi);
                    if lo < hi {
                        grow_to(&mut m, hi);
                        for x in &mut m[lo..hi] {
                            *x = !*x;
                        }
                    }
                }
                5 => {
                    let v = rng.below(2) == 0;
                    l.insert(a, v);
                    if a >= m.len() {
                        m.resize(a, false);
                        m.push(v);
                    } else {
                        m.insert(a, v);
                    }
                }
                6 => {
                    let got = l.remove(a);
                    let want = if a < m.len() { Some(m.remove(a)) } else { None };
                    assert_eq!(got, want, "{what}: remove");
                }
                7 => {
                    let v = rng.below(2) == 0;
                    l.push(v);
                    m.push(v);
                }
                8 => assert_eq!(l.pop(), m.pop(), "{what}: pop"),
                9 => {
                    l.truncate(a);
                    m.truncate(a);
                }
                10 => {
                    l.set_len(a);
                    m.resize(a, false);
                }
                11 => {
                    let n = 1 + rng.below(64) as u32;
                    let src = if rng.below(3) == 0 { u64::MAX } else { rng.next() };
                    l.push_bits(src, n);
                    for j in 0..n {
                        m.push(src >> j & 1 != 0);
                    }
                }
                12..=14 => {
                    let m2 = runs(&mut rng, 450);
                    let l2 = list_of(&m2);
                    let op = rng.below(4);
                    match op {
                        0 => l.and(&l2),
                        1 => l.or(&l2),
                        2 => l.xor(&l2),
                        _ => l.and_not(&l2),
                    }
                    let len = if op == 1 || op == 2 { m.len().max(m2.len()) } else { m.len() };
                    let x = |i: usize| i < m.len() && m[i];
                    let y = |i: usize| i < m2.len() && m2[i];
                    let r: Vec<bool> = (0..len)
                        .map(|i| match op {
                            0 => x(i) && y(i),
                            1 => x(i) || y(i),
                            2 => x(i) ^ y(i),
                            _ => x(i) && !y(i),
                        })
                        .collect();
                    m = r;
                }
                _ => {
                    l.trim();
                    let end = m.iter().rposition(|b| *b).map_or(0, |i| i + 1);
                    m.truncate(end);
                }
            }
            check(&l, &m, &what);
        }
        assert_eq!(l, list_of(&m), "canonical: equal bits, equal lists");
    }
}

// BitsList

#[test]
fn bits_list_matches_model() {
    let mut rng = Rng::new(5);
    for bits in 1..=8u32 {
        let m8 = ((1u32 << bits) - 1) as u8;
        for round in 0..60 * seeds(1).end {
            let default = rng.next() as u8 & m8;
            let mut l: BitsList<u8> = BitsList::with_default(bits, default);
            let mut m: Vec<u8> = Vec::new();
            for step in 0..300 {
                let what = format!("bits {bits} round {round} step {step}");
                let i = rng.below(m.len() as u64 + 20) as usize;
                let v = rng.next() as u8;
                match rng.below(12) {
                    0..=2 => {
                        l.push(v);
                        m.push(v & m8);
                    }
                    3 | 4 => {
                        l.set(i, v);
                        if i >= m.len() {
                            m.resize(i + 1, default);
                        }
                        m[i] = v & m8;
                    }
                    5 | 6 => {
                        l.insert(i, v);
                        if i >= m.len() {
                            m.resize(i, default);
                            m.push(v & m8);
                        } else {
                            m.insert(i, v & m8);
                        }
                    }
                    7 | 8 if !m.is_empty() => {
                        let i = i % m.len();
                        assert_eq!(l.remove(i), m.remove(i), "{what}: remove");
                    }
                    9 => assert_eq!(l.pop(), m.pop(), "{what}: pop"),
                    10 => {
                        let n = rng.below(m.len() as u64 + 10) as usize;
                        l.set_len(n);
                        m.resize(n, default);
                    }
                    11 => {
                        let x = v & m8;
                        let removed = l.remove_all(x);
                        let before = m.len();
                        m.retain(|y| *y != x);
                        assert_eq!(removed, before - m.len(), "{what}: remove_all");
                    }
                    _ => {}
                }
                assert_eq!(l.len(), m.len(), "{what}: len");
                assert_eq!(l.iter().collect::<Vec<_>>(), m, "{what}: items");
            }
            // Raw windows and equality on copies with other capacities.
            let per = (64 / bits) as usize;
            for item in 0..m.len().saturating_sub(per) {
                let want = (0..per).fold(0u64, |w, j| w | (m[item + j] as u64) << (j as u32 * bits));
                assert_eq!(l.raw(item, per), want);
            }
            let mut c = l.clone();
            c.shrink_to_fit();
            assert!(c == l);
            let mut d: BitsList<u8> = BitsList::new(bits);
            d.extend(m.iter().copied());
            assert!(d == l);
        }
    }
}

// lists

#[test]
fn bits_list_bulk_growth_matches_model() {
    for bits in 1..=8u32 {
        let mask = ((1u32 << bits) - 1) as u8;
        for default in [0, 1, mask / 3, mask] {
            for prefix in 0..64 {
                let head: Vec<u8> = (0..prefix).map(|i| (i * 37) as u8 & mask).collect();
                let mut base = BitsList::with_default(bits, default);
                base.extend(head.iter().copied());
                for gap in [0, 1, 8, 9, 63, 64, 65, 257] {
                    let mut list = base.clone();
                    let mut model = head.clone();
                    model.resize(prefix + gap, default);
                    list.set_len(model.len());
                    assert_eq!(list.iter().collect::<Vec<_>>(), model, "bits {bits}, prefix {prefix}, gap {gap}, default {default}");

                    // Equality includes padding bits: growing after a truncate must leave them
                    // zero.
                    list.truncate(prefix);
                    list.set_len(model.len());
                    let mut fresh = BitsList::new(bits);
                    fresh.extend(model.iter().copied());
                    assert_eq!(list, fresh);
                }
                let tail: Vec<u8> = (0..129).map(|i| (i * 53) as u8 & mask).collect();
                for from in [0, prefix / 2, prefix, prefix + 17] {
                    let mut list = base.clone();
                    let mut model = head.clone();
                    model.resize(model.len().max(from + tail.len()), default);
                    model[from..from + tail.len()].copy_from_slice(&tail);
                    list.set_slice(from, &tail);
                    assert_eq!(list.iter().collect::<Vec<_>>(), model);
                    list.set_slice(usize::MAX, &[]);
                    assert_eq!(list.len(), model.len());
                }
            }
        }
    }
}

/// `ops` random operations on a nullable list and on its `Vec<Option<_>>` model.
macro_rules! nullable_list_run {
    ($List:ty, $seed:expr, $threshold:expr, $make:expr, $read:expr, $own:expr) => {{
        let mut rng = Rng::new($seed);
        let mut l: $List = <$List>::new();
        l.set_flat_threshold($threshold);
        let mut m: Vec<Option<u32>> = Vec::new();
        let mut went_flat = false;
        for step in 0..3000 {
            let i = rng.below(m.len() as u64 + 8) as usize;
            let v = if rng.below(3) == 0 { None } else { Some(rng.below(50) as u32) };
            let what = format!("step {step}");
            match rng.below(14) {
                0..=3 => {
                    let old = l.set(i, v.map($make));
                    if i >= m.len() {
                        m.resize(i + 1, None);
                    }
                    let want = std::mem::replace(&mut m[i], v);
                    assert_eq!(old.map($own), want, "{what}: set");
                }
                4..=6 => {
                    l.insert(i, v.map($make));
                    if i >= m.len() {
                        m.resize(i, None);
                        m.push(v);
                    } else {
                        m.insert(i, v);
                    }
                }
                7 | 8 if !m.is_empty() => {
                    let i = i % m.len();
                    assert_eq!(l.remove(i).map($own), m.remove(i), "{what}: remove");
                }
                9 => {
                    l.push(v.map($make));
                    m.push(v);
                }
                10 if !m.is_empty() => {
                    let (a, b) = (i % m.len(), rng.below(m.len() as u64) as usize);
                    l.swap(a, b);
                    m.swap(a, b);
                }
                11 => {
                    let n = rng.below(m.len() as u64 + 5) as usize;
                    l.set_len(n);
                    m.resize(n, None);
                }
                12 if step % 5 == 0 => l.shrink_to_fit(),
                13 => assert_eq!(l.pop().map(|x| x.map($own)), m.pop(), "{what}: pop"),
                _ => {}
            }
            went_flat |= l.is_flat();
            assert_eq!(l.len(), m.len(), "{what}: len");
            assert_eq!(l.cardinality(), m.iter().filter(|x| x.is_some()).count(), "{what}: cardinality");
            if step % 7 == 0 {
                let got: Vec<Option<u32>> = l.iter().map(|x| x.map($read)).collect();
                assert_eq!(got, m, "{what}: items");
                for j in 0..m.len() {
                    assert_eq!(l.get(j).map($read), m[j]);
                }
                let probe = rng.below(m.len() as u64 + 2) as usize;
                assert_eq!(l.next_value_index(probe), (probe..m.len()).find(|&j| m[j].is_some()));
                assert_eq!(l.prev_null_index(probe), (0..probe.min(m.len())).rev().find(|&j| m[j].is_none()));
            }
        }
        assert!(went_flat, "the flat layout was exercised");
        l
    }};
}

#[test]
fn nullable_primitive_list_matches_model() {
    for seed in seeds(6) {
        let l = nullable_list_run!(NullablePrimitiveList<u32>, seed, 12, |x| x, |x: u32| x, |x: u32| x);
        let c = l.clone();
        assert!(c == l);
        let probe = Some(7u32);
        let want = l.iter().position(|x| x == probe);
        assert_eq!(l.index_of(probe), want);
        assert_eq!(l.last_index_of(None), l.iter().collect::<Vec<_>>().iter().rposition(|x| x.is_none()));
    }
}

#[test]
fn nullable_object_list_matches_model() {
    let token = Rc::new(());
    for seed in seeds(6) {
        let t = token.clone();
        let l = nullable_list_run!(NullableObjectList<(u32, Rc<()>)>, seed, 12, |x| (x, t.clone()), |x: &(u32, Rc<()>)| x.0, |x: (u32, Rc<()>)| x.0);
        let values = l.cardinality();
        assert_eq!(Rc::strong_count(&token), 2 + values);
        let owned: Vec<Option<u32>> = l.clone().into_iter().map(|x| x.map(|(v, _)| v)).collect();
        assert_eq!(owned, l.iter().map(|x| x.map(|v| v.0)).collect::<Vec<_>>());
        drop(l);
        drop(t);
        assert_eq!(Rc::strong_count(&token), 1);
    }
    let mut l: NullableObjectList<String> = [Some("a".to_string()), None, Some("b".to_string())].into_iter().collect();
    assert_eq!(l.index_of(Some(&"b".to_string())), Some(2));
    for s in l.iter_mut().flatten() {
        s.push('!');
    }
    assert_eq!(l.get(0).map(String::as_str), Some("a!"));
}

// RingBuffer, Pool

#[test]
fn ring_buffer_single_thread() {
    let q: RingBuffer<String> = RingBuffer::new(2);
    assert_eq!(q.capacity(), 4);
    for i in 0..4 {
        assert_eq!(q.put_position(i.to_string()), Ok(i as u64));
    }
    assert!(q.is_full());
    assert_eq!(q.try_put("x".into()), Err("x".to_string()));
    assert_eq!(q.try_get().as_deref(), Some("0"));
    assert_eq!(q.head(), 1);
    assert!(q.try_put("4".into()).is_ok());
    let mut seen = Vec::new();
    q.clear_with(|s| seen.push(s));
    assert_eq!(seen, ["1", "2", "3", "4"]);
    assert!(q.is_empty() && q.try_get().is_none());
    q.try_put("left behind".into()).unwrap(); // dropped with the queue
}

#[test]
fn ring_buffer_mpmc_delivers_each_value_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    const PRODUCERS: usize = 4;
    const PER: usize = 50_000;
    let q = Arc::new(RingBuffer::<usize>::new(6));
    let taken = Arc::new(AtomicUsize::new(0));
    let sums: Vec<_> = (0..4)
        .map(|_| {
            let (q, taken) = (q.clone(), taken.clone());
            std::thread::spawn(move || {
                let (mut sum, mut n) = (0usize, 0usize);
                while taken.load(Ordering::Relaxed) < PRODUCERS * PER {
                    match q.try_get() {
                        Some(v) => {
                            sum += v;
                            n += 1;
                            taken.fetch_add(1, Ordering::Relaxed);
                        }
                        None => std::thread::yield_now(),
                    }
                }
                (sum, n)
            })
        })
        .collect();
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let q = q.clone();
            std::thread::spawn(move || {
                for i in 0..PER {
                    let mut v = p * PER + i + 1;
                    while let Err(back) = q.try_put(v) {
                        v = back;
                        std::thread::yield_now();
                    }
                }
            })
        })
        .collect();
    for p in producers {
        p.join().unwrap();
    }
    let (sum, n) = sums.into_iter().map(|h| h.join().unwrap()).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    let total = PRODUCERS * PER;
    assert_eq!(n, total);
    assert_eq!(sum, total * (total + 1) / 2);
    assert!(q.is_empty());
}

#[test]
fn pool_reuses_and_bounds() {
    let token = Rc::new(());
    {
        let t = token.clone();
        // capacity 2: magazines of 2, a depot of one full magazine; this thread holds two magazines
        // besides
        let pool = Pool::new(1, move || (Vec::<u8>::with_capacity(64), t.clone()));
        let alive = || Rc::strong_count(&token) - 2; // the token and the factory's
        let mut a = pool.acquire();
        a.0.push(1);
        pool.release(a);
        let b = pool.acquire();
        assert_eq!(b.0, [1], "the released object comes back");
        pool.release(b);
        let held: Vec<_> = (0..7).map(|_| pool.acquire()).collect();
        assert_eq!(alive(), 7, "one reused, six new");
        for x in held {
            pool.release(x);
        }
        assert_eq!(alive(), 6, "two magazines of the thread and one of the depot kept, the seventh dropped");
        assert_eq!(pool.len(), 2);
        {
            let mut g = pool.get();
            g.0.push(9);
        }
        assert_eq!(alive(), 6, "a guard gives back");
        let kept = pool.get().into_inner(); // not released
        let taken: Vec<_> = (0..5).map(|_| pool.acquire()).collect();
        assert_eq!(alive(), 6, "the six kept are taken again: none made");
        drop((kept, taken));
        assert_eq!(alive(), 0);
    }
    assert_eq!(Rc::strong_count(&token), 1, "every object dropped once");
}

/// Objects taken on some threads and given back on others: never given to two at once, given back
/// ones reach the takers, each dropped once - with threads coming and going past `THREADS`, their
/// places and magazines taken over.
#[test]
fn pool_across_threads_gives_each_object_once() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    struct Obj(AtomicBool, Arc<AtomicUsize>);
    impl Drop for Obj {
        fn drop(&mut self) {
            self.1.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let alive = Arc::new(AtomicUsize::new(0));
    let made = Arc::new(AtomicUsize::new(0));
    {
        let (a, m) = (alive.clone(), made.clone());
        let pool = Arc::new(Pool::new(8, move || {
            a.fetch_add(1, Ordering::Relaxed);
            m.fetch_add(1, Ordering::Relaxed);
            Obj(AtomicBool::new(false), a.clone())
        }));
        const PER: usize = 50_000;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Obj>(1024);
        let rx = Arc::new(std::sync::Mutex::new(rx));
        std::thread::scope(|s| {
            for _ in 0..4 {
                let (pool, tx) = (pool.clone(), tx.clone());
                s.spawn(move || {
                    for _ in 0..PER {
                        let o = pool.acquire();
                        assert!(!o.0.swap(true, Ordering::Relaxed), "given to two at once");
                        tx.send(o).unwrap();
                    }
                });
            }
            drop(tx);
            for _ in 0..4 {
                let (pool, rx) = (pool.clone(), rx.clone());
                s.spawn(move || loop {
                    let Ok(o) = rx.lock().unwrap().recv() else { break };
                    o.0.store(false, Ordering::Relaxed);
                    pool.release(o);
                });
            }
        });
        assert!(made.load(Ordering::Relaxed) < PER, "given back on other threads, taken again: {} made", made.load(Ordering::Relaxed));
        // threads one after another, more than there are places: each takes the place and the
        // magazines of one gone
        for i in 0..2 * super::pool::THREADS {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let x: Vec<_> = (0..i % 70).map(|_| pool.acquire()).collect();
                for o in x {
                    pool.release(o);
                }
            })
            .join()
            .unwrap();
        }
    }
    assert_eq!(alive.load(Ordering::Relaxed), 0, "every object dropped once");
}

// Regression coverage for collection invariants:
//
// * Equality and hashing ignore unused words and storage layout.
// * Removing an optional key or clearing releases its value.
// * Sparse flat storage can return to compact storage.
// * Gaps contain the configured default value.
// * `retain` visits entries relocated into a removed slot.

fn hash_of<T: std::hash::Hash>(t: &T) -> u64 {
    let mut s = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(t, &mut s);
    s.finish()
}

#[test]
fn bit_list_equal_content_is_equal() {
    let mut rng = Rng::new(11);
    for round in 0..300 * seeds(1).end {
        let mut l = list_of(&runs(&mut rng, 300));
        for step in 0..30 {
            let n = l.len() as u64;
            let (a, b) = (rng.below(n + 70) as usize, rng.below(n + 70) as usize);
            let (lo, hi) = (a.min(b), a.max(b));
            let v = rng.below(2) == 0;
            match rng.below(16) {
                0 => l.set(a, v),
                1 => l.set_range(lo..hi, v),
                2 => l.flip(a),
                3 => l.flip_range(lo..hi),
                4 => l.insert(a, v),
                5 => {
                    l.remove(a);
                }
                6 => l.push(v),
                7 => {
                    l.pop();
                }
                8 => l.truncate(a),
                9 => l.set_len(a),
                10 => l.push_bits(rng.next(), 1 + rng.below(64) as u32),
                11 => l.trim(),
                12 => l.shrink_to_fit(),
                13 => {
                    let o = list_of(&runs(&mut rng, 300));
                    match rng.below(4) {
                        0 => l.and(&o),
                        1 => l.or(&o),
                        2 => l.xor(&o),
                        _ => l.and_not(&o),
                    }
                }
                14 => l.set1(a),
                _ => l.set0(a),
            }
            let fresh = list_of(&model_of(&l));
            assert!(l == fresh, "round {round} step {step}: equal content compares unequal");
            assert_eq!(hash_of(&l), hash_of(&fresh), "round {round} step {step}: equal content, different hash");
        }
    }
}

#[test]
fn bits_list_all_ops_and_equality() {
    let mut rng = Rng::new(12);
    for bits in 1..=8u32 {
        let m8 = ((1u32 << bits) - 1) as u8;
        for round in 0..40 * seeds(1).end {
            let default = rng.next() as u8 & m8;
            let mut l: BitsList<u8> = BitsList::with_default(bits, default);
            let mut m: Vec<u8> = Vec::new();
            for step in 0..200 {
                let what = format!("bits {bits} round {round} step {step}");
                let i = rng.below(m.len() as u64 + 20) as usize;
                let v = rng.next() as u8;
                match rng.below(15) {
                    0..=2 => {
                        l.push(v);
                        m.push(v & m8);
                    }
                    3 => {
                        l.set(i, v);
                        if i >= m.len() {
                            m.resize(i + 1, default);
                        }
                        m[i] = v & m8;
                    }
                    4 => {
                        l.insert(i, v);
                        if i >= m.len() {
                            m.resize(i, default);
                            m.push(v & m8);
                        } else {
                            m.insert(i, v & m8);
                        }
                    }
                    5 if !m.is_empty() => {
                        let i = i % m.len();
                        assert_eq!(l.remove(i), m.remove(i), "{what}: remove");
                    }
                    6 => assert_eq!(l.pop(), m.pop(), "{what}: pop"),
                    7 => {
                        let n = rng.below(m.len() as u64 + 10) as usize;
                        l.truncate(n);
                        m.truncate(n);
                    }
                    8 => {
                        let n = rng.below(m.len() as u64 + 10) as usize;
                        l.set_len(n);
                        m.resize(n, default);
                    }
                    9 => {
                        let r = rng.next() as u8;
                        l.retain(|x| (x ^ r) % 3 != 0);
                        m.retain(|x| (x ^ r) % 3 != 0);
                    }
                    10 => {
                        let count = rng.below(12) as usize;
                        let src: Vec<u8> = (0..count).map(|_| rng.next() as u8).collect();
                        l.set_slice(i, &src);
                        if !src.is_empty() {
                            if i + src.len() > m.len() {
                                m.resize(i + src.len(), default);
                            }
                            for (j, x) in src.iter().enumerate() {
                                m[i + j] = x & m8;
                            }
                        }
                    }
                    11 => {
                        let items = rng.below(64 / bits as u64 + 1) as usize;
                        let src = rng.next();
                        l.set_raw_items(i, items, src);
                        if items > 0 {
                            if i + items > m.len() {
                                m.resize(i + items, 0); // documented: the gap is zeros, not the default
                            }
                            for j in 0..items {
                                m[i + j] = (src >> (j as u32 * bits)) as u8 & m8;
                            }
                        }
                    }
                    12 => l.shrink_to_fit(),
                    13 => {
                        l.clear();
                        m.clear();
                    }
                    _ => {
                        // whole bytes: 8 items are `bits` bytes
                        let item = i / 8 * 8;
                        let items = 8 * rng.below(3) as usize;
                        let fill: Vec<u8> = (0..items * bits as usize / 8).map(|_| rng.next() as u8).collect();
                        l.bytes_mut(item, items).copy_from_slice(&fill);
                        if item + items > m.len() {
                            m.resize(item + items, 0); // the gap is zeros, as with set_raw_items
                        }
                        for j in 0..items {
                            let bit = j * bits as usize;
                            let w = fill[bit / 8] as u16 | (*fill.get(bit / 8 + 1).unwrap_or(&0) as u16) << 8;
                            m[item + j] = (w >> (bit % 8)) as u8 & m8;
                        }
                    }
                }
                assert_eq!(l.len(), m.len(), "{what}: len");
                assert_eq!(l.iter().collect::<Vec<_>>(), m, "{what}: items");
                // the default value does not count for equality
                let mut fresh: BitsList<u8> = BitsList::with_default(bits, default.wrapping_add(1) & m8);
                fresh.extend(m.iter().copied());
                assert!(l == fresh, "{what}: equal content compares unequal");
                assert_eq!(hash_of(&l), hash_of(&fresh), "{what}: equal content, different hash");
            }
        }
    }
}

#[test]
fn nullable_primitive_list_ops_and_equality() {
    let mut rng = Rng::new(14);
    for round in 0..200 * seeds(1).end {
        let mut l: NullablePrimitiveList<u32> = NullablePrimitiveList::new();
        l.set_flat_threshold(rng.below(40) as usize);
        let mut m: Vec<Option<u32>> = Vec::new();
        for step in 0..300 {
            let what = format!("round {round} step {step}");
            let i = rng.below(m.len() as u64 + 8) as usize;
            let v = if rng.below(3) == 0 { None } else { Some(rng.below(30) as u32) };
            match rng.below(15) {
                0..=2 => {
                    let old = l.set(i, v);
                    if i >= m.len() {
                        m.resize(i + 1, None);
                    }
                    assert_eq!(old, std::mem::replace(&mut m[i], v), "{what}: set");
                }
                3 | 4 => {
                    l.insert(i, v);
                    if i >= m.len() {
                        m.resize(i, None);
                        m.push(v);
                    } else {
                        m.insert(i, v);
                    }
                }
                5 if !m.is_empty() => {
                    let i = i % m.len();
                    assert_eq!(l.remove(i), m.remove(i), "{what}: remove");
                }
                6 => {
                    l.push(v);
                    m.push(v);
                }
                7 => assert_eq!(l.pop(), m.pop(), "{what}: pop"),
                8 if !m.is_empty() => {
                    let (a, b) = (i % m.len(), rng.below(m.len() as u64) as usize);
                    l.swap(a, b);
                    m.swap(a, b);
                }
                9 => {
                    let n = rng.below(m.len() as u64 + 5) as usize;
                    l.set_len(n);
                    m.resize(n, None);
                }
                10 => {
                    let n = rng.below(m.len() as u64 + 5) as usize;
                    l.truncate(n);
                    m.truncate(n);
                }
                11 => {
                    l.trim();
                    while m.last() == Some(&None) {
                        m.pop();
                    }
                }
                12 => {
                    let want = match m.iter().position(|x| *x == v) {
                        Some(p) => {
                            m.remove(p);
                            true
                        }
                        None => false,
                    };
                    assert_eq!(l.remove_item(v), want, "{what}: remove_item");
                }
                13 => l.set_flat_threshold(rng.below(40) as usize),
                _ => {
                    if rng.below(3) == 0 {
                        l.clear();
                        m.clear();
                    } else {
                        l.shrink_to_fit();
                    }
                }
            }
            assert_eq!(l.len(), m.len(), "{what}: len");
            assert_eq!(l.cardinality(), m.iter().filter(|x| x.is_some()).count(), "{what}: cardinality");
            assert_eq!(l.iter().collect::<Vec<_>>(), m, "{what}: items");
            assert_eq!(l.index_of(v), m.iter().position(|x| *x == v), "{what}: index_of");
            assert_eq!(l.last_index_of(v), m.iter().rposition(|x| *x == v), "{what}: last_index_of");
            // the same content in the other layout
            let mut fresh: NullablePrimitiveList<u32> = NullablePrimitiveList::new();
            fresh.set_flat_threshold(if l.is_flat() { usize::MAX } else { 0 });
            fresh.extend(m.iter().copied());
            assert!(l == fresh, "{what}: equal content compares unequal (flat {} vs {})", l.is_flat(), fresh.is_flat());
            assert_eq!(hash_of(&l), hash_of(&fresh), "{what}: equal content, different hash");
        }
    }
}

// nullable maps

type Val = (u64, Rc<()>);

/// One face for the four nullable maps, so one loop drives them against a `HashMap`.
trait NullMap: Sized {
    type K: Copy + Eq + std::hash::Hash + Ord + std::fmt::Debug;
    fn fresh() -> Self;
    fn ins(&mut self, k: Option<Self::K>, v: Val) -> Option<Val>;
    fn rem(&mut self, k: Option<Self::K>) -> Option<Val>;
    fn get_(&self, k: Option<Self::K>) -> Option<&Val>;
    fn get_mut_(&mut self, k: Option<Self::K>) -> Option<&mut Val>;
    fn goiw(&mut self, k: Option<Self::K>, v: Val) -> &mut Val;
    fn has(&self, k: Option<Self::K>) -> bool;
    fn retain_(&mut self, f: &mut dyn FnMut(&Val) -> bool);
    fn shrink(&mut self);
    fn reserve_(&mut self, n: usize);
    fn clear_(&mut self);
    fn len_(&self) -> usize;
    fn items(&self) -> Vec<(Option<Self::K>, u64)>;
    fn same(&self, o: &Self) -> bool;
    fn clone_(&self) -> Self;
}

macro_rules! null_map_by_value_key {
    ($T:ty, $K:ty, $fresh:expr, $reserve:expr, $same:expr) => {
        impl NullMap for $T {
            type K = $K;
            fn fresh() -> Self {
                $fresh
            }
            fn ins(&mut self, k: Option<$K>, v: Val) -> Option<Val> {
                self.insert(k, v)
            }
            fn rem(&mut self, k: Option<$K>) -> Option<Val> {
                self.remove(k)
            }
            fn get_(&self, k: Option<$K>) -> Option<&Val> {
                self.get(k)
            }
            fn get_mut_(&mut self, k: Option<$K>) -> Option<&mut Val> {
                self.get_mut(k)
            }
            fn goiw(&mut self, k: Option<$K>, v: Val) -> &mut Val {
                self.get_or_insert_with(k, || v)
            }
            fn has(&self, k: Option<$K>) -> bool {
                self.contains_key(k)
            }
            fn retain_(&mut self, f: &mut dyn FnMut(&Val) -> bool) {
                self.retain(|_, v| f(v));
            }
            fn shrink(&mut self) {
                self.shrink_to_fit();
            }
            fn reserve_(&mut self, n: usize) {
                #[allow(clippy::redundant_closure_call)]
                ($reserve)(self, n);
            }
            fn clear_(&mut self) {
                self.clear();
            }
            fn len_(&self) -> usize {
                self.len()
            }
            fn items(&self) -> Vec<(Option<$K>, u64)> {
                self.iter().map(|(k, v)| (k, v.0)).collect()
            }
            fn same(&self, o: &Self) -> bool {
                #[allow(clippy::redundant_closure_call)]
                ($same)(self, o)
            }
            fn clone_(&self) -> Self {
                self.clone()
            }
        }
    };
}

fn same_items<M: NullMap>(a: &M, b: &M) -> bool {
    let (mut x, mut y) = (a.items(), b.items());
    x.sort();
    y.sort();
    x == y
}

null_map_by_value_key!(NullablePrimitiveMap<u32, Val, Weak>, u32, NullablePrimitiveMap::with_hasher(Weak), |m: &mut Self, n| m.reserve(n), |a: &Self, b: &Self| a == b && same_items(a, b));
null_map_by_value_key!(NullablePrimitiveMap<u32, Val>, u32, NullablePrimitiveMap::new(), |m: &mut Self, n| m.reserve(n), |a: &Self, b: &Self| a == b && same_items(a, b));
null_map_by_value_key!(NullableTwoBytesMap<u16, Val>, u16, NullableTwoBytesMap::new(), |m: &mut Self, n| m.reserve(n), |a: &Self, b: &Self| a == b && same_items(a, b));
null_map_by_value_key!(NullableByteMap<u8, Val>, u8, NullableByteMap::new(), |_: &mut Self, _| {}, |a: &Self, b: &Self| a == b && same_items(a, b));

impl<S: BuildHasher + Default + Clone> NullMap for NullableMap<u32, Val, S> {
    type K = u32;
    fn fresh() -> Self {
        NullableMap::with_hasher(S::default())
    }
    fn ins(&mut self, k: Option<u32>, v: Val) -> Option<Val> {
        self.insert(k, v)
    }
    fn rem(&mut self, k: Option<u32>) -> Option<Val> {
        self.remove(k.as_ref())
    }
    fn get_(&self, k: Option<u32>) -> Option<&Val> {
        self.get(k.as_ref())
    }
    fn get_mut_(&mut self, k: Option<u32>) -> Option<&mut Val> {
        self.get_mut(k.as_ref())
    }
    fn goiw(&mut self, k: Option<u32>, v: Val) -> &mut Val {
        self.get_or_insert_with(k, || v)
    }
    fn has(&self, k: Option<u32>) -> bool {
        self.contains_key(k.as_ref())
    }
    fn retain_(&mut self, f: &mut dyn FnMut(&Val) -> bool) {
        self.retain(|_, v| f(v));
    }
    fn shrink(&mut self) {
        self.shrink_to_fit();
    }
    fn reserve_(&mut self, n: usize) {
        self.reserve(n);
    }
    fn clear_(&mut self) {
        self.clear();
    }
    fn len_(&self) -> usize {
        self.len()
    }
    fn items(&self) -> Vec<(Option<u32>, u64)> {
        self.iter().map(|(k, v)| (k.copied(), v.0)).collect()
    }
    fn same(&self, o: &Self) -> bool {
        self == o && same_items(self, o)
    }
    fn clone_(&self) -> Self {
        self.clone()
    }
}

/// `ops` random operations on a nullable map and on its `HashMap<Option<K>, _>` model;
/// `max_reserve` large enough makes a two-bytes map go flat with only a few keys.
fn null_map_run<M: NullMap>(seed: u64, key: impl Fn(&mut Rng) -> Option<M::K>, ops: usize, max_reserve: u64, what: &str) {
    let token = Rc::new(());
    let mut rng = Rng::new(seed);
    let mut m = M::fresh();
    let mut model: HashMap<Option<M::K>, u64> = HashMap::new();
    for step in 0..ops {
        let k = key(&mut rng);
        match rng.below(100) {
            0..=34 => {
                let v = rng.next();
                assert_eq!(m.ins(k, (v, token.clone())).map(|x| x.0), model.insert(k, v), "{what} step {step}: insert {k:?}");
            }
            35..=59 => assert_eq!(m.rem(k).map(|x| x.0), model.remove(&k), "{what} step {step}: remove {k:?}"),
            60..=71 => {
                assert_eq!(m.get_(k).map(|x| x.0), model.get(&k).copied(), "{what} step {step}: get {k:?}");
                assert_eq!(m.has(k), model.contains_key(&k), "{what} step {step}: contains {k:?}");
            }
            72..=75 => {
                if let Some(x) = m.get_mut_(k) {
                    x.0 ^= 1;
                }
                if let Some(x) = model.get_mut(&k) {
                    *x ^= 1;
                }
            }
            76..=79 => {
                let v = rng.next();
                let got = m.goiw(k, (v, token.clone())).0;
                assert_eq!(got, *model.entry(k).or_insert(v), "{what} step {step}: get_or_insert_with {k:?}");
            }
            80..=82 => {
                let r = rng.next() | 1;
                m.retain_(&mut |v| (v.0 ^ r) % 3 != 0);
                model.retain(|_, v| (*v ^ r) % 3 != 0);
            }
            83..=84 => m.shrink(),
            85..=86 => m.reserve_(rng.below(max_reserve) as usize),
            87 => {
                let c = m.clone_();
                assert!(c.same(&m) && m.same(&c), "{what} step {step}: clone");
                m = c;
            }
            88 if rng.below(8) == 0 => {
                m.clear_();
                model.clear();
            }
            _ => {}
        }
        assert_eq!(m.len_(), model.len(), "{what} step {step}: len");
        if step % 31 == 0 || model.len() < 3 {
            let mut got = m.items();
            got.sort();
            let mut want: Vec<_> = model.iter().map(|(k, v)| (*k, *v)).collect();
            want.sort();
            assert_eq!(got, want, "{what} step {step}: items");
            // the same content, inserted from scratch
            let mut fresh = M::fresh();
            for (k, v) in &model {
                fresh.ins(*k, (*v, token.clone()));
            }
            assert!(m.same(&fresh) && fresh.same(&m), "{what} step {step}: equal content compares unequal");
        }
    }
    assert_eq!(Rc::strong_count(&token), 1 + m.len_(), "{what}: leaked or double-dropped values");
    drop(m);
    assert_eq!(Rc::strong_count(&token), 1, "{what}: values not dropped with the map");
}

#[test]
fn nullable_maps_match_std() {
    let small = |r: &mut Rng| if r.below(12) == 0 { None } else { Some(r.below(300) as u32) };
    for seed in seeds(4) {
        null_map_run::<NullablePrimitiveMap<u32, Val>>(seed, small, 5000, 600, "NullablePrimitiveMap");
        null_map_run::<NullablePrimitiveMap<u32, Val, Weak>>(seed, small, 3000, 600, "NullablePrimitiveMap weak");
        null_map_run::<NullableMap<u32, Val, rustc_hash::FxBuildHasher>>(seed, small, 5000, 600, "NullableMap");
        null_map_run::<NullableMap<u32, Val, Weak>>(seed, small, 3000, 600, "NullableMap weak");
        null_map_run::<NullableByteMap<u8, Val>>(seed, |r| if r.below(20) == 0 { None } else { Some(r.next() as u8) }, 5000, 1, "NullableByteMap");
        // two-bytes keys: few keys in a flat layout (reserve), many keys, and back
        let two = |r: &mut Rng| match r.below(20) {
            0 => None,
            1..=9 => Some(r.below(200) as u16),
            _ => Some(r.next() as u16),
        };
        null_map_run::<NullableTwoBytesMap<u16, Val>>(seed, two, 8000, 70_000, "NullableTwoBytesMap");
        null_map_run::<NullableTwoBytesMap<u16, Val>>(seed, |r| Some(r.next() as u16), 60_000, 70_000, "NullableTwoBytesMap big");
    }
}

// two-bytes sets

#[test]
fn two_bytes_sets_match_std() {
    for seed in seeds(4) {
        let mut rng = Rng::new(seed);
        let mut s: TwoBytesSet<u16> = TwoBytesSet::new();
        let mut n: NullableTwoBytesSet<u16> = NullableTwoBytesSet::new();
        let mut model: HashSet<u16> = HashSet::new();
        let mut null = false;
        let mut was_flat = false;
        for step in 0..40_000 {
            let what = format!("seed {seed} step {step}");
            // phases: grow past the flat threshold, shrink, grow again
            let k = if (step / 10_000) % 2 == 0 { rng.next() as u16 } else { rng.below(400) as u16 };
            let grow = (step / 10_000) % 2 == 0;
            match rng.below(100) {
                0..=39 if grow => {
                    assert_eq!(s.insert(k), model.insert(k), "{what}: insert");
                    n.insert(Some(k));
                }
                0..=69 => {
                    let key = if grow { k } else { *model.iter().next().unwrap_or(&k) };
                    assert_eq!(s.remove(key), model.remove(&key), "{what}: remove");
                    n.remove(Some(key));
                }
                70..=79 => {
                    assert_eq!(s.contains(k), model.contains(&k), "{what}: contains");
                    assert_eq!(n.contains(Some(k)), model.contains(&k), "{what}: nullable contains");
                }
                80 => {
                    let v = rng.below(2) == 0;
                    if v {
                        n.insert(None);
                    } else {
                        n.remove(None);
                    }
                    null = v;
                }
                81 => {
                    let r = rng.below(7) as u16;
                    s.retain(|x| x % 7 != r);
                    n.retain(|x| x.map_or(true, |x| x % 7 != r));
                    model.retain(|x| x % 7 != r);
                }
                82 => {
                    s.shrink_to_fit();
                    n.shrink_to_fit();
                }
                83 => {
                    s.reserve(rng.below(70_000) as usize);
                    n.reserve(rng.below(70_000) as usize);
                }
                84 => {
                    // set algebra against a random other set
                    let other: HashSet<u16> = (0..rng.below(300)).map(|_| if rng.below(2) == 0 { rng.below(400) as u16 } else { rng.next() as u16 }).collect();
                    let mut o: TwoBytesSet<u16> = TwoBytesSet::new();
                    for x in &other {
                        o.insert(*x);
                    }
                    assert_eq!(s.is_subset(&o), model.is_subset(&other), "{what}: is_subset");
                    assert_eq!(s.is_superset(&o), model.is_superset(&other), "{what}: is_superset");
                    assert_eq!(s.is_disjoint(&o), model.is_disjoint(&other), "{what}: is_disjoint");
                    let mut no: NullableTwoBytesSet<u16> = NullableTwoBytesSet::new();
                    for x in &other {
                        no.insert(Some(*x));
                    }
                    match rng.below(3) {
                        0 => {
                            s.intersect_with(&o);
                            n.intersect_with(&no);
                            model.retain(|x| other.contains(x));
                            null = false;
                        }
                        1 => {
                            s.difference_with(other.iter().copied());
                            n.difference_with(other.iter().map(|x| Some(*x)));
                            model.retain(|x| !other.contains(x));
                        }
                        _ => {
                            s.symmetric_difference_with(other.iter().copied());
                            n.symmetric_difference_with(other.iter().map(|x| Some(*x)));
                            for x in &other {
                                if !model.remove(x) {
                                    model.insert(*x);
                                }
                            }
                        }
                    }
                }
                85 => {
                    let c = s.clone();
                    assert!(c == s, "{what}: clone");
                    s = c;
                    let c = n.clone();
                    assert!(c == n, "{what}: nullable clone");
                    n = c;
                }
                86 if rng.below(50) == 0 => {
                    s.clear();
                    n.clear();
                    model.clear();
                    null = false;
                }
                _ => {}
            }
            was_flat |= s.is_flat();
            assert_eq!(s.len(), model.len(), "{what}: len");
            assert_eq!(n.len(), model.len() + null as usize, "{what}: nullable len");
            assert_eq!(n.contains(None), null, "{what}: null key");
            if step % 997 == 0 || model.len() < 3 {
                let mut got: Vec<u16> = s.iter().collect();
                got.sort();
                let mut want: Vec<u16> = model.iter().copied().collect();
                want.sort();
                assert_eq!(got, want, "{what}: items (flat {})", s.is_flat());
                let mut got_n: Vec<Option<u16>> = n.iter().collect();
                got_n.sort();
                let mut want_n: Vec<Option<u16>> = want.iter().map(|x| Some(*x)).collect();
                if null {
                    want_n.push(None);
                }
                want_n.sort();
                assert_eq!(got_n, want_n, "{what}: nullable items");
                let mut fresh: TwoBytesSet<u16> = TwoBytesSet::new();
                for x in &want {
                    fresh.insert(*x);
                }
                assert!(s == fresh, "{what}: equal content compares unequal (flat {} vs {})", s.is_flat(), fresh.is_flat());
            }
        }
        assert!(was_flat, "the flat layout was exercised");
    }
}

/// The hash of a map or a set does not depend on the order of the entries, as its equality does
/// not; equal pairs, and a key equal to its value, do not cancel out.
#[test]
fn unordered_collections_hash_as_they_compare() {
    let (mut a, mut b) = (Map::<u32, u32>::new(), Map::<u32, u32>::new());
    for i in 0..100 {
        a.insert(i, i * 3);
        b.insert(99 - i, (99 - i) * 3);
    }
    assert!(a == b);
    assert_eq!(hash_of(&a), hash_of(&b));
    let (mut c, mut d) = (Map::<u32, u32>::new(), Map::<u32, u32>::new());
    c.insert(1, 1);
    c.insert(2, 2);
    d.insert(3, 3);
    d.insert(4, 4);
    assert_ne!(hash_of(&c), hash_of(&d));

    let (mut m, mut n) = (NullableMap::<u32, u32>::new(), NullableMap::<u32, u32>::new());
    m.insert(None, 7);
    m.insert(Some(1), 2);
    n.insert(Some(1), 2);
    n.insert(None, 7);
    assert_eq!(hash_of(&m), hash_of(&n));
    n.insert(None, 8);
    assert_ne!(hash_of(&m), hash_of(&n));

    let (mut s, mut t) = (NullableSet::<u32>::new(), NullableSet::<u32>::new());
    s.insert(5);
    s.insert(6);
    t.insert(6);
    t.insert(5);
    assert_eq!(hash_of(&s), hash_of(&t));
}

/// The pass of the generated sending, key after key by number with the place in the slot: the keys
/// of `iter`, in its order, in every layout (compressed and flat `ByteMap`, hash and flat
/// `TwoBytesMap`); the same number twice, a new pass from 0 and a jump give the right key too.
#[test]
fn a_pass_by_number_gives_the_keys_in_order() {
    fn check<K: Copy + PartialEq + std::fmt::Debug>(keys: Vec<K>, at: impl Fn(usize, &mut token::Seq) -> K) {
        let mut seq = token::Seq::default();
        for _ in 0..2 {
            for (n, k) in keys.iter().enumerate() {
                assert_eq!(at(n, &mut seq), *k, "key {n}");
                assert_eq!(at(n, &mut seq), *k, "key {n} again");
            }
        }
        for n in (0..keys.len()).rev().step_by(3) {
            assert_eq!(at(n, &mut seq), keys[n], "a jump to {n}");
        }
    }
    let mut set = ByteSet::<u8>::new();
    let mut map = ByteMap::<u8, [u64; 4]>::new(); // a large value: the compressed layout
    let mut flat = ByteMap::<u8, u32>::new();
    let mut nset = NullableByteSet::<u8>::new();
    let mut nmap = NullableByteMap::<u8, u32>::new();
    for k in (0..256u32).step_by(7).map(|k| (k * 37 % 256) as u8) {
        set.insert(k);
        map.insert(k, [k as u64; 4]);
        flat.insert(k, k as u32);
        nset.insert(Some(k));
        nmap.insert(Some(k), k as u32);
    }
    nset.insert(None);
    nmap.insert(None, 7);
    check(set.iter().collect(), |n, s| unsafe { set.key_seq(n, s) });
    check(map.iter().map(|(k, _)| k).collect(), |n, s| unsafe { *map.entry_seq(n, s).0 });
    check(map.iter().map(|(_, v)| v[0]).collect(), |n, s| unsafe { map.entry_seq(n, s).1[0] });
    check(flat.iter().map(|(k, v)| (k, *v)).collect(), |n, s| unsafe {
        let (k, v) = flat.entry_seq(n, s);
        (*k, *v)
    });
    check(nset.iter().flatten().collect(), |n, s| unsafe { nset.key_seq(n, s) });
    check(nmap.iter().filter_map(|(k, v)| k.map(|k| (k, *v))).collect(), |n, s| unsafe {
        let (k, v) = nmap.entry_seq(n, s);
        (*k, *v)
    });
    assert_eq!(nmap.null_value(), Some(&7));
    for count in [40u32, 3000] {
        // hash, then flat
        let mut set = TwoBytesSet::<u16>::new();
        let mut map = TwoBytesMap::<u16, u32>::new();
        let mut nset = NullableTwoBytesSet::<u16>::new();
        let mut nmap = NullableTwoBytesMap::<u16, u32>::new();
        for k in (0..count).map(|i| (i * 7919 % 65521) as u16) {
            set.insert(k);
            map.insert(k, k as u32 + 1);
            nset.insert(Some(k));
            nmap.insert(Some(k), k as u32);
        }
        nset.insert(None);
        assert!(nset.contains_null());
        check(set.iter().collect(), |n, s| unsafe { set.key_seq(n, s) });
        check(map.iter().map(|(k, v)| (k, *v)).collect(), |n, s| unsafe {
            let (k, v) = map.entry_seq(n, s);
            (*k, *v)
        });
        check(nset.iter().flatten().collect(), |n, s| unsafe { nset.key_seq(n, s) });
        check(nmap.iter().filter_map(|(k, v)| k.map(|k| (k, *v))).collect(), |n, s| unsafe {
            let (k, v) = nmap.entry_seq(n, s);
            (*k, *v)
        });
        assert_eq!(nmap.null_value(), None);
    }
}

/// A table that grew large and holds a few: `clear` zeroes only their buckets. Nothing of the
/// cleared keys is found after, the keys put after are found, the values dropped once - with a fair
/// hash and with chains of 8 classes.
#[test]
fn a_few_entries_in_a_large_table_clear_cleanly() {
    fn run<S: BuildHasher + Default + Clone>() {
        let token = Rc::new(());
        let mut m: PrimitiveMap<u32, Rc<()>, S> = PrimitiveMap::with_hasher(S::default());
        let mut s: PrimitiveSet<u32, S> = PrimitiveSet::with_hasher(S::default());
        for k in 0..4096u32 {
            m.insert(k, token.clone());
            s.insert(k);
        }
        m.clear(); // the whole table
        s.clear();
        for round in 0..20u32 {
            let keys: Vec<u32> = (0..(round % 7 + 1)).map(|i| round * 1000 + i * 37).collect();
            for &k in &keys {
                m.insert(k, token.clone());
                s.insert(k);
            }
            assert_eq!(Rc::strong_count(&token), 1 + keys.len());
            m.clear(); // a few: their buckets only
            s.clear();
            assert_eq!(Rc::strong_count(&token), 1, "every value dropped once");
            assert!(m.is_empty() && s.is_empty());
            for &k in &keys {
                assert!(m.get(k).is_none() && !s.contains(k), "a cleared key is gone");
            }
            let fresh: Vec<u32> = keys.iter().map(|k| k + 1).chain(keys.iter().copied()).collect();
            for &k in &fresh {
                m.insert(k, token.clone());
                s.insert(k);
            }
            for &k in &fresh {
                assert!(m.get(k).is_some() && s.contains(k), "a key put after the clear is found");
            }
            assert_eq!(m.len(), fresh.len());
            m.clear();
            s.clear();
        }
    }
    run::<rustc_hash::FxBuildHasher>();
    run::<Weak>();
    let mut t = TwoBytesMap::<u16, u32>::with_capacity(2000);
    for k in 0..2000u16 {
        t.insert(k, k as u32);
    }
    t.clear();
    for k in [7u16, 300, 4000] {
        t.insert(k, 1);
    }
    t.clear();
    assert!(t.is_empty() && t.get(7).is_none() && t.get(300).is_none());
    t.insert(300, 2);
    assert_eq!(t.get(300), Some(&2));
}

#[test]
fn bit_list_bytes_match_model() {
    let mut rng = Rng::new(7);
    let byte = |m: &[bool], at: usize| (0..8).fold(0u8, |b, k| b | ((m.get(at + k).copied().unwrap_or(false) as u8) << k));
    for _ in 0..600 {
        let m = runs(&mut rng, 900);
        let l = list_of(&m);
        // read: any start, any length, past the end too
        let from = rng.below(m.len() as u64 + 70) as usize;
        let n = rng.below(140) as usize;
        let mut got = vec![0xA5u8; n];
        l.get_bytes(from, &mut got);
        let want: Vec<u8> = (0..n).map(|i| byte(&m, from + i * 8)).collect();
        assert_eq!(got, want, "get_bytes from {from}, {n} bytes, len {}, run {}", m.len(), m.iter().take_while(|b| **b).count());
        assert_eq!(l.iter().fold(0usize, |c, b| c + b as usize), m.iter().filter(|b| **b).count(), "fold");

        // append: onto a list in any state, in pieces of any length
        let mut a = list_of(&m[..rng.below(m.len() as u64 + 1) as usize]);
        let mut model: Vec<bool> = model_of(&a);
        for _ in 0..3 {
            let src: Vec<u8> = (0..rng.below(40))
                .map(|_| match rng.below(4) {
                    0 => 0xFF,
                    1 => 0,
                    _ => rng.next() as u8,
                })
                .collect();
            a.push_bytes(&src);
            for b in &src {
                model.extend((0..8).map(|k| b >> k & 1 == 1));
            }
            check(&a, &model, "push_bytes");
        }
    }
}

#[test]
fn bit_list_bools_match_model() {
    let mut rng = Rng::new(11);
    for _ in 0..400 {
        let m = runs(&mut rng, 700);
        let start = list_of(&m[..rng.below(m.len() as u64 + 1) as usize]);
        let long = if rng.below(8) == 0 { 2040 + rng.below(4200) } else { rng.below(300) };
        let src: Vec<bool> = (0..long).map(|_| rng.below(3) != 0).collect();
        let mut model = model_of(&start);
        model.extend_from_slice(&src);
        let mut a = start.clone();
        a.push_bools(&src);
        check(&a, &model, "push_bools");
        let mut b = start.clone();
        b.extend(src.iter().copied());
        check(&b, &model, "extend");
        let from = rng.below(model.len() as u64 + 80) as usize;
        let mut got = vec![true; if rng.below(8) == 0 { 2040 + rng.below(4200) } else { rng.below(200) } as usize];
        a.get_bools(from, &mut got);
        let want: Vec<bool> = (0..got.len()).map(|i| model.get(from + i).copied().unwrap_or(false)).collect();
        assert_eq!(got, want, "get_bools from {from}");
    }
}

#[test]
fn bit_list_set_bools_match_model() {
    let mut rng = Rng::new(13);
    for _ in 0..500 {
        let m = runs(&mut rng, 3000);
        let mut l = list_of(&m);
        let mut model = m.clone();
        let run = m.iter().take_while(|b| **b).count();
        let index = if rng.below(3) == 0 { run } else { rng.below(m.len() as u64 + 100) as usize };
        let n = if rng.below(6) == 0 { 2040 + rng.below(3000) } else { rng.below(200) } as usize;
        let src: Vec<bool> = (0..n)
            .map(|_| match rng.below(5) {
                0 => false,
                1 | 2 => true,
                _ => rng.below(2) == 1,
            })
            .collect();
        l.set_bools(index, &src);
        if model.len() < index + n {
            model.resize(index + n, false);
        }
        model[index..index + n].copy_from_slice(&src);
        check(&l, &model, "set_bools");
    }
}

#[test]
fn bit_list_built_any_way_is_equal() {
    use std::hash::{BuildHasher, BuildHasherDefault};
    let h = |l: &BitList| BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default().hash_one(l);
    let mut rng = Rng::new(17);
    for _ in 0..400 {
        // a run, often of whole words, then anything
        let run = if rng.below(2) == 0 { 64 * rng.below(4) as usize } else { rng.below(200) as usize };
        let mut m = vec![true; run];
        m.extend(runs(&mut rng, 300));
        let a = list_of(&m);
        let mut b = BitList::new();
        for &x in &m {
            b.push(x);
        }
        let mut c = BitList::new();
        c.push_bools(&m);
        let mut d = BitList::new();
        for (i, &x) in m.iter().enumerate() {
            d.set(i, x);
        }
        d.set_len(m.len());
        let want = m.iter().take_while(|x| **x).count();
        for (l, how) in [(&b, "push"), (&c, "push_bools"), (&d, "set")] {
            check(l, &m, how);
            assert_eq!(l, &a, "{how}: equal");
            assert_eq!(h(l), h(&a), "{how}: hash");
            assert_eq!(l.trailing_ones(), want, "{how}: trailing_ones");
            assert_eq!(l.find_first_difference(&a), m.len(), "{how}: no difference");
        }
    }
}

#[test]
fn bits_list_bulk_matches_model() {
    for plain in [false, true] {
        super::bits_list::NO_BMI2.store(plain, std::sync::atomic::Ordering::Relaxed);
        bits_list_bulk(plain);
    }
}

fn bits_list_bulk(plain: bool) {
    let mut rng = Rng::new(19 + plain as u64);
    for _ in 0..1500 {
        let bits = 1 + rng.below(8) as u32;
        let m = ((1u32 << bits) - 1) as u8;
        let len = rng.below(700) as usize;
        let mut model: Vec<u8> = (0..len).map(|_| rng.next() as u8 & m).collect();
        let mut l = BitsList::<u8>::new(bits);
        l.push_slice(&model);
        assert_eq!(l.iter().collect::<Vec<_>>(), model, "push_slice, {bits} bits");
        // overwrite inside: the items after the slice must stay
        let from = rng.below(len as u64 + 1) as usize;
        let n = rng.below((len - from) as u64 + 1) as usize;
        let src: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect(); // high bits set: must be masked
        l.set_slice(from, &src);
        for (k, &v) in src.iter().enumerate() {
            model[from + k] = v & m;
        }
        assert_eq!(l.iter().collect::<Vec<_>>(), model, "set_slice {from}+{n}, {bits} bits");
        // read any piece
        let a = rng.below(len as u64 + 1) as usize;
        let mut got = vec![0xA5u8; rng.below((len - a) as u64 + 1) as usize];
        l.get_slice(a, &mut got);
        assert_eq!(got, model[a..a + got.len()], "get_slice {a}, {bits} bits");
        assert_eq!(
            l.iter().fold(0u64, |s, v| s.wrapping_mul(31).wrapping_add(v as u64)),
            model.iter().fold(0u64, |s, &v| s.wrapping_mul(31).wrapping_add(v as u64)),
            "fold, {bits} bits"
        );
        // extend, then the zeros past the end hold: equality with a list built item by item
        let more: Vec<u8> = (0..rng.below(600)).map(|_| rng.next() as u8 & m).collect();
        l.extend(more.iter().copied());
        model.extend_from_slice(&more);
        let mut one = BitsList::<u8>::new(bits);
        for &v in &model {
            one.push(v);
        }
        assert_eq!(l, one, "extend, {bits} bits");
    }
}

#[test]
fn nullable_object_list_ops_match_model() {
    let mut rng = Rng::new(29);
    for round in 0..300 {
        let mut l = NullableObjectList::<String>::new();
        let mut m: Vec<Option<String>> = Vec::new();
        l.set_flat_threshold([0usize, 5, 40, 1024][round % 4]);
        for step in 0..400 {
            let v = if rng.below(4) == 0 { None } else { Some(format!("{}", rng.next() % 1000)) };
            let len = m.len();
            match rng.below(10) {
                0 | 1 | 2 => {
                    l.push(v.clone());
                    m.push(v);
                }
                3 | 4 => {
                    let i = rng.below(len as u64 + 70) as usize;
                    let old = l.set(i, v.clone());
                    if i >= m.len() {
                        m.resize(i + 1, None);
                    }
                    assert_eq!(old, std::mem::replace(&mut m[i], v), "round {round} step {step}: set {i} old");
                }
                5 => {
                    let i = rng.below(len as u64 + 70) as usize;
                    l.insert(i, v.clone());
                    if i >= m.len() {
                        m.resize(i + 1, None);
                        m[i] = v;
                    } else {
                        m.insert(i, v);
                    }
                }
                6 if len > 0 => {
                    let i = rng.below(len as u64) as usize;
                    assert_eq!(l.remove(i), m.remove(i), "round {round} step {step}: remove {i}");
                }
                7 => {
                    let n = rng.below(len as u64 + 130) as usize;
                    l.set_len(n);
                    m.resize(n, None);
                }
                8 if len > 1 => {
                    let (i, j) = (rng.below(len as u64) as usize, rng.below(len as u64) as usize);
                    l.swap(i, j);
                    m.swap(i, j);
                }
                9 if rng.below(10) == 0 => {
                    l.clear();
                    m.clear();
                }
                _ => l.set_flat_threshold([0usize, 3, 50, usize::MAX][rng.below(4) as usize]),
            }
            l.assert_ranks();
            assert_eq!(l.len(), m.len(), "round {round} step {step}: len");
            assert_eq!(l.cardinality(), m.iter().filter(|x| x.is_some()).count(), "round {round} step {step}: cardinality");
            for (i, x) in m.iter().enumerate() {
                assert_eq!(l.get(i), x.as_ref(), "round {round} step {step}: get {i}");
            }
            assert_eq!(l.iter().collect::<Vec<_>>(), m.iter().map(Option::as_ref).collect::<Vec<_>>(), "round {round} step {step}: iter");
            let skip = rng.below(m.len() as u64 + 1) as usize;
            let mut it = l.iter();
            let mut got: Vec<Option<&String>> = (0..skip).map(|_| it.next().unwrap()).collect();
            got = it.fold(got, |mut g, x| {
                g.push(x);
                g
            });
            assert_eq!(got, m.iter().map(Option::as_ref).collect::<Vec<_>>(), "round {round} step {step}: fold after {skip}");
        }
    }
}

#[test]
fn nullable_primitive_list_ops_match_model() {
    let mut rng = Rng::new(23);
    for round in 0..300 {
        let mut l = NullablePrimitiveList::<u32>::new();
        let mut m: Vec<Option<u32>> = Vec::new();
        let th = [0usize, 5, 40, 1024][round % 4];
        l.set_flat_threshold(th);
        for step in 0..400 {
            let v = if rng.below(4) == 0 { None } else { Some(rng.next() as u32) };
            let len = m.len();
            match rng.below(9) {
                0 | 1 | 2 => {
                    l.push(v);
                    m.push(v);
                }
                3 | 4 => {
                    let i = rng.below(len as u64 + 70) as usize;
                    let old = l.set(i, v);
                    if i >= m.len() {
                        m.resize(i + 1, None);
                    }
                    assert_eq!(old, std::mem::replace(&mut m[i], v), "round {round} step {step}: set {i} old");
                }
                5 => {
                    let i = rng.below(len as u64 + 70) as usize;
                    l.insert(i, v);
                    if i >= m.len() {
                        m.resize(i + 1, None);
                        m[i] = v;
                    } else {
                        m.insert(i, v);
                    }
                }
                6 if len > 0 => {
                    let i = rng.below(len as u64) as usize;
                    assert_eq!(l.remove(i), m.remove(i), "round {round} step {step}: remove {i}");
                }
                7 => {
                    let n = rng.below(len as u64 + 130) as usize;
                    l.set_len(n);
                    m.resize(n, None);
                }
                8 if len > 1 => {
                    let (i, j) = (rng.below(len as u64) as usize, rng.below(len as u64) as usize);
                    l.swap(i, j);
                    m.swap(i, j);
                }
                _ => {
                    let t = [0usize, 3, 50, usize::MAX][rng.below(4) as usize];
                    l.set_flat_threshold(t);
                }
            }
            l.assert_ranks();
            assert_eq!(l.len(), m.len(), "round {round} step {step}: len");
            assert_eq!(l.cardinality(), m.iter().filter(|x| x.is_some()).count(), "round {round} step {step}: cardinality");
            for (i, x) in m.iter().enumerate() {
                assert_eq!(l.get(i), *x, "round {round} step {step}: get {i}");
            }
            assert_eq!(l.iter().collect::<Vec<_>>(), m, "round {round} step {step}: iter");
            let skip = rng.below(m.len() as u64 + 1) as usize;
            let mut it = l.iter();
            let mut got: Vec<Option<u32>> = (0..skip).map(|_| it.next().unwrap()).collect();
            got = it.fold(got, |mut g, x| {
                g.push(x);
                g
            });
            assert_eq!(got, m, "round {round} step {step}: fold after {skip}");
        }
    }
}

#[test]
fn clear_keys_to_gives_each_key_and_keeps_the_table() {
    let token = Rc::new(());
    let mut m: Map<String, Rc<()>> = Map::new();
    let mut n: NullableMap<String, Rc<()>> = NullableMap::new();
    let mut s: NullableSet<String> = NullableSet::new();
    for k in 0..4096 {
        m.insert(k.to_string(), token.clone());
    }
    m.clear();
    // a few keys in a large table, then as many as the table holds: both ways of zeroing the buckets
    for size in [1usize, 3, 7, 300, 2000, 4096] {
        for round in 0..3 {
            let keys: Vec<String> = (0..size).map(|i| (i * 31 + round).to_string()).collect();
            for k in &keys {
                m.insert(k.clone(), token.clone());
                n.insert_key(k.clone(), token.clone());
                s.insert(k.clone());
            }
            n.insert_null_key(token.clone());
            s.insert_null();
            for (mut given, is) in [
                (
                    {
                        let mut g = Vec::new();
                        m.clear_keys_to(|k| g.push(k));
                        g
                    },
                    m.is_empty() && m.len() == 0,
                ),
                (
                    {
                        let mut g = Vec::new();
                        n.clear_keys_to(|k| g.push(k));
                        g
                    },
                    n.is_empty() && n.len() == 0,
                ),
                (
                    {
                        let mut g = Vec::new();
                        s.clear_keys_to(|k| g.push(k));
                        g
                    },
                    s.is_empty() && s.len() == 0,
                ),
            ] {
                assert!(is);
                given.sort();
                let mut want = keys.clone();
                want.sort();
                assert_eq!(given, want);
            }
            assert_eq!(Rc::strong_count(&token), 1);
            for k in &keys {
                assert!(m.get(k).is_none() && n.get_key(k).is_none() && !s.contains(k));
            }
            assert!(!s.contains_null());
        }
    }
    m.clear_keys_to(|_| panic!("no keys"));
    m.insert("a".to_string(), token.clone());
    assert!(m.get("a").is_some());
}
