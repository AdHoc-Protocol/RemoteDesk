use super::*;
use std::{
    collections::HashSet,
    hash::{BuildHasher, Hasher},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

macro_rules! owned_key {
    (plain, $key:expr) => {
        Some($key as u64)
    };
    (borrow, $key:expr) => {
        Some(*$key as u64)
    };
    (nullable, $key:expr) => {
        $key.map(|k| k as u64)
    };
    (nullable_borrow, $key:expr) => {
        $key.map(|k| *k as u64)
    };
}

macro_rules! walk_map {
    ($map:ident, $kind:ident) => {{
        let mut expected: Vec<_> = $map.iter().map(|(k, v)| (owned_key!($kind, k), *v)).collect();
        expected.sort_by_key(|(key, _)| key.is_none()); // Nullable token traversal visits keyed entries before the optional key.
        let mut entries = Vec::new();
        let mut tokens = HashSet::new();
        let mut previous_values = Vec::new();
        let mut next = $map.token();
        while let Some(token) = next {
            assert!(tokens.insert(token), "token cycle");
            assert!($map.contains_token(token));
            let (key, value) = $map.entry_by_token(token).unwrap();
            assert_eq!($map.token_of(key), Some(token));
            assert_eq!($map.key_by_token(token), Some(key));
            assert_eq!($map.value_by_token(token), Some(value));
            entries.push((owned_key!($kind, key), *value));
            previous_values.push((token, *value));
            // A saved scalar token allows mutation and continuation without an iterator borrow.
            *$map.value_by_token_mut(token).unwrap() += 1;
            next = $map.next_token(token);
            assert!(entries.len() <= $map.len());
        }
        assert_eq!(entries, expected);
        assert_eq!(entries.len(), $map.len());
        for (token, before) in previous_values {
            assert_eq!($map.value_by_token(token), Some(&(before + 1)));
        }
        let invalid = usize::MAX - 1;
        assert!(!$map.contains_token(invalid));
        assert_eq!($map.entry_by_token(invalid), None);
        assert_eq!($map.value_by_token(invalid), None);
        assert_eq!($map.value_by_token_mut(invalid), None);
        assert_eq!($map.next_token(invalid), None);
    }};
}

macro_rules! insert_null_map {
    (plain, $m:ident) => {};
    (borrow, $m:ident) => {};
    (nullable, $m:ident) => {
        $m.insert(None, 999);
    };
    (nullable_borrow, $m:ident) => {
        $m.insert(None, 999);
    };
}
macro_rules! insert_map {
    (plain, $m:ident, $k:expr, $v:expr) => {
        $m.insert($k, $v)
    };
    (borrow, $m:ident, $k:expr, $v:expr) => {
        $m.insert($k, $v)
    };
    (nullable, $m:ident, $k:expr, $v:expr) => {
        $m.insert(Some($k), $v)
    };
    (nullable_borrow, $m:ident, $k:expr, $v:expr) => {
        $m.insert(Some($k), $v)
    };
}
macro_rules! remove_map {
    (plain, $m:ident, $k:expr) => {
        $m.remove($k)
    };
    (borrow, $m:ident, $k:expr) => {
        $m.remove(&$k)
    };
    (nullable, $m:ident, $k:expr) => {
        $m.remove(Some($k))
    };
    (nullable_borrow, $m:ident, $k:expr) => {
        $m.remove(Some(&$k))
    };
}
macro_rules! exercise_map {
    ($new:expr, $key:ty, $kind:ident) => {{
        let mut map = $new;
        walk_map!(map, $kind);
        insert_null_map!($kind, map);
        walk_map!(map, $kind); // Includes null-only maps without allocated keyed storage.
        for i in 0..30 {
            insert_map!($kind, map, (i * 193 % 256) as $key, i as i64);
        }
        walk_map!(map, $kind);
        for i in 30..256 {
            insert_map!($kind, map, (i * 193 % 256) as $key, i as i64);
        }
        walk_map!(map, $kind);
        for i in 0..246 {
            remove_map!($kind, map, (i * 193 % 256) as $key);
        }
        walk_map!(map, $kind);
        map.shrink_to_fit();
        walk_map!(map, $kind);
        let mut clone = map.clone();
        walk_map!(clone, $kind);
        map.clear();
        walk_map!(map, $kind);
        insert_map!($kind, map, 255u16 as $key, 1);
        walk_map!(map, $kind);
    }};
}

#[test]
fn primitive_map_token_access() {
    exercise_map!(PrimitiveMap::<u64, i64>::new(), u64, plain);
}
#[test]
fn map_token_access() {
    exercise_map!(Map::<u64, i64>::new(), u64, borrow);
}
#[test]
fn nullable_map_token_access() {
    exercise_map!(NullableMap::<u64, i64>::new(), u64, nullable_borrow);
}
#[test]
fn nullable_primitive_map_token_access() {
    exercise_map!(NullablePrimitiveMap::<u64, i64>::new(), u64, nullable);
}
#[test]
fn byte_map_token_access_in_both_layouts() {
    exercise_map!(ByteMap::<u8, i64>::new(), u8, plain);
}
#[test]
fn nullable_byte_map_token_access_in_both_layouts() {
    exercise_map!(NullableByteMap::<u8, i64>::new(), u8, nullable);
}
#[test]
fn two_bytes_map_token_access() {
    exercise_map!(TwoBytesMap::<u16, i64>::new(), u16, plain);
}
#[test]
fn nullable_two_bytes_map_token_access() {
    exercise_map!(NullableTwoBytesMap::<u16, i64>::new(), u16, nullable);
}
#[test]
fn two_bytes_map_flat_then_hash_token_access() {
    exercise_map!(TwoBytesMap::<u16, i64>::with_capacity(65536), u16, plain);
}
#[test]
fn nullable_two_bytes_map_flat_then_hash_token_access() {
    exercise_map!(NullableTwoBytesMap::<u16, i64>::with_capacity(65536), u16, nullable);
}

macro_rules! walk_set {
    ($set:ident, $kind:ident, $iter:ident, $lookup:ident) => {{
        let mut expected: Vec<_> = $set.$iter().map(|k| owned_key!($kind, k)).collect();
        expected.sort_by_key(|key| key.is_none());
        let mut keys = Vec::new();
        let mut tokens = HashSet::new();
        let mut next = $set.token();
        while let Some(token) = next {
            assert!(tokens.insert(token), "token cycle");
            assert!($set.contains_token(token));
            let key = $set.key_by_token(token).unwrap();
            assert_eq!($set.$lookup(key), Some(token));
            keys.push(owned_key!($kind, key));
            next = $set.next_token(token);
            assert!(keys.len() <= $set.len());
        }
        assert_eq!(keys, expected);
        assert_eq!(keys.len(), $set.len());
        let invalid = usize::MAX - 1;
        assert!(!$set.contains_token(invalid));
        assert_eq!($set.key_by_token(invalid), None);
        assert_eq!($set.next_token(invalid), None);
    }};
}
macro_rules! insert_null_set {
    (plain, $s:ident) => {};
    (nullable, $s:ident) => {
        $s.insert(None);
    };
    (nullable_borrow, $s:ident) => {
        $s.insert_null();
    };
}
macro_rules! insert_set {
    (plain, $s:ident, $k:expr) => {
        $s.insert($k)
    };
    (nullable, $s:ident, $k:expr) => {
        $s.insert(Some($k))
    };
    (nullable_borrow, $s:ident, $k:expr) => {
        $s.insert($k)
    };
}
macro_rules! remove_set {
    (plain, $s:ident, $k:expr) => {
        $s.remove($k)
    };
    (nullable, $s:ident, $k:expr) => {
        $s.remove(Some($k))
    };
    (nullable_borrow, $s:ident, $k:expr) => {
        $s.remove(&$k)
    };
}
macro_rules! exercise_set {
    ($new:expr, $key:ty, $kind:ident, $iter:ident, $lookup:ident) => {{
        let mut set = $new;
        walk_set!(set, $kind, $iter, $lookup);
        insert_null_set!($kind, set);
        walk_set!(set, $kind, $iter, $lookup);
        for i in 0..256 {
            insert_set!($kind, set, (i * 193 % 256) as $key);
        }
        walk_set!(set, $kind, $iter, $lookup);
        for i in 0..246 {
            remove_set!($kind, set, (i * 193 % 256) as $key);
        }
        walk_set!(set, $kind, $iter, $lookup);
        let clone = set.clone();
        walk_set!(clone, $kind, $iter, $lookup);
        set.clear();
        walk_set!(set, $kind, $iter, $lookup);
        insert_set!($kind, set, 255u16 as $key);
        walk_set!(set, $kind, $iter, $lookup);
    }};
}

#[test]
fn primitive_set_token_access() {
    exercise_set!(PrimitiveSet::<u64>::new(), u64, plain, iter, token_of);
}
#[test]
fn nullable_primitive_set_token_access() {
    exercise_set!(NullablePrimitiveSet::<u64>::new(), u64, nullable, iter, token_of);
}
#[test]
fn nullable_set_token_access() {
    exercise_set!(NullableSet::<u64>::new(), u64, nullable_borrow, iter_opt, token_of_opt);
}
#[test]
fn byte_set_token_access() {
    exercise_set!(ByteSet::<u8>::new(), u8, plain, iter, token_of);
}
#[test]
fn nullable_byte_set_token_access() {
    exercise_set!(NullableByteSet::<u8>::new(), u8, nullable, iter, token_of);
}
#[test]
fn two_bytes_set_token_access() {
    exercise_set!(TwoBytesSet::<u16>::new(), u16, plain, iter, token_of);
}
#[test]
fn nullable_two_bytes_set_token_access() {
    exercise_set!(NullableTwoBytesSet::<u16>::new(), u16, nullable, iter, token_of);
}

#[test]
fn two_bytes_sets_flat_then_hash_token_access() {
    let mut set = TwoBytesSet::<u16>::with_capacity(65536);
    let mut nullable = NullableTwoBytesSet::<u16>::with_capacity(65536);
    nullable.insert(None);
    for key in [0, 1, 63, 64, 65, 127, 128, 32767, 32768, 65534, 65535] {
        set.insert(key);
        nullable.insert(Some(key));
    }
    assert!(set.is_flat() && nullable.is_flat());
    walk_set!(set, plain, iter, token_of);
    walk_set!(nullable, nullable, iter, token_of);
    set.shrink_to_fit();
    nullable.shrink_to_fit();
    assert!(!set.is_flat() && !nullable.is_flat());
    walk_set!(set, plain, iter, token_of);
    walk_set!(nullable, nullable, iter, token_of);
    for key in 0..600 {
        set.insert(key);
        nullable.insert(Some(key));
    }
    assert!(set.is_flat() && nullable.is_flat());
    walk_set!(set, plain, iter, token_of);
    walk_set!(nullable, nullable, iter, token_of);
}

#[derive(Clone)]
struct HashGate(Arc<AtomicBool>);
struct GatedHasher(Arc<AtomicBool>);
impl BuildHasher for HashGate {
    type Hasher = GatedHasher;
    fn build_hasher(&self) -> GatedHasher {
        assert!(!self.0.load(Ordering::Relaxed), "token access hashed a key");
        GatedHasher(self.0.clone())
    }
}
impl Hasher for GatedHasher {
    fn finish(&self) -> u64 {
        assert!(!self.0.load(Ordering::Relaxed));
        0
    } // Force a long chain.
    fn write(&mut self, _: &[u8]) {
        assert!(!self.0.load(Ordering::Relaxed));
    }
}

#[test]
fn collision_tokens_skip_the_gap_and_do_not_hash() {
    let gate = Arc::new(AtomicBool::new(false));
    let mut map = PrimitiveMap::<u64, i64, _>::with_capacity_and_hasher(64, HashGate(gate.clone()));
    let mut object = Map::<u64, i64, _>::with_capacity_and_hasher(64, HashGate(gate.clone()));
    let mut nullable = NullablePrimitiveMap::<u64, i64, _>::with_capacity_and_hasher(64, HashGate(gate.clone()));
    let mut set = PrimitiveSet::<u64, _>::with_capacity_and_hasher(64, HashGate(gate.clone()));
    let mut object_set = NullableSet::<u64, _>::with_capacity_and_hasher(64, HashGate(gate.clone()));
    for key in 0..20 {
        map.insert(key, key as i64);
        object.insert(key, key as i64);
        nullable.insert(Some(key), key as i64);
        set.insert(key);
        object_set.insert(key);
    }
    nullable.insert(None, 99);
    object_set.insert_null();
    let target = map.token_of(10).unwrap();
    gate.store(true, Ordering::Relaxed);
    macro_rules! read_map_without_hashing {
        ($m:ident) => {{
            let expected: i64 = $m.iter().map(|(_, v)| *v).sum();
            let mut sum = 0;
            let mut count = 0;
            let mut token = $m.token();
            while let Some(t) = token {
                let (key, value) = $m.entry_by_token(t).unwrap();
                assert_eq!($m.key_by_token(t), Some(key));
                sum += *value;
                *$m.value_by_token_mut(t).unwrap() += 1;
                count += 1;
                token = $m.next_token(t);
            }
            assert_eq!(count, $m.len());
            assert_eq!(sum, expected);
        }};
    }
    read_map_without_hashing!(map);
    read_map_without_hashing!(object);
    read_map_without_hashing!(nullable);
    for t in [20, 21, 62, 64, usize::MAX - 1, NULL_TOKEN] {
        assert_eq!(map.entry_by_token(t), None); // lo is 0..19, hi is 63..64.
        assert_eq!(map.next_token(t), None);
    }
    assert_eq!(map.next_token(18), Some(63));
    macro_rules! read_set_without_hashing {
        ($s:ident) => {{
            let mut count = 0;
            let mut token = $s.token();
            while let Some(t) = token {
                assert!($s.key_by_token(t).is_some());
                count += 1;
                token = $s.next_token(t);
            }
            assert_eq!(count, $s.len());
        }};
    }
    read_set_without_hashing!(set);
    read_set_without_hashing!(object_set);
    assert_eq!(map.key_by_token(target), Some(10));
    // Structural operations are followed by obtaining fresh tokens.
    gate.store(false, Ordering::Relaxed);
    for key in [0, 10, 19, 1, 18] {
        map.remove(key);
    }
    map.reserve(300);
    walk_map!(map, plain);
    map.shrink_to_fit();
    walk_map!(map, plain);
}

#[test]
fn borrowed_strings_nullable_values_and_zero_sized_entries() {
    let mut map = Map::<String, Option<i64>>::new();
    map.insert("hello".to_owned(), None);
    let token = map.token_of("hello").unwrap();
    assert_eq!(map.entry_by_token(token), Some((&"hello".to_owned(), &None)));
    *map.value_by_token_mut(token).unwrap() = Some(42);
    assert_eq!(map.get("hello"), Some(&Some(42)));
    assert_eq!(map.token_of("absent"), None);
    let mut nullable = NullableMap::<String, Option<i64>>::new();
    nullable.insert(None, None);
    nullable.insert(Some("hello".to_owned()), Some(1));
    assert_eq!(nullable.token_of(None::<&str>), Some(NULL_TOKEN));
    let first = nullable.token_of(Some("hello")).unwrap();
    assert_eq!(nullable.next_token(first), Some(NULL_TOKEN));
    assert_eq!(nullable.key_by_token(NULL_TOKEN), Some(None));
    assert_eq!(nullable.value_by_token(NULL_TOKEN), Some(&None));
    let mut set = NullableSet::<String>::new();
    set.insert("hello".to_owned());
    set.insert_null();
    assert_eq!(set.key_by_token(set.token_of("hello").unwrap()).unwrap().unwrap(), "hello");
    assert_eq!(set.token_of_opt(None::<&str>), Some(NULL_TOKEN));
    let mut zst = NullablePrimitiveMap::<(), ()>::new();
    zst.insert(Some(()), ());
    zst.insert(None, ());
    let first = zst.token().unwrap();
    assert_eq!(zst.entry_by_token(first), Some((Some(()), &())));
    *zst.value_by_token_mut(first).unwrap() = ();
    assert_eq!(zst.next_token(first), Some(NULL_TOKEN));
    assert_eq!(zst.entry_by_token(NULL_TOKEN), Some((None, &())));
    assert_eq!(zst.next_token(NULL_TOKEN), None);
}

#[test]
fn signed_keys_and_flat_map_last_slot() {
    exercise_map!(ByteMap::<i8, i64>::new(), i8, plain);
    exercise_set!(NullableByteSet::<i8>::new(), i8, nullable, iter, token_of);
    let mut map = TwoBytesMap::<i16, i64>::with_capacity(65536);
    for (key, value) in [(-32768, 1), (-1, 2), (0, 3), (32767, 4)] {
        map.insert(key, value);
    }
    assert_eq!(map.token_of(-1), Some(65535));
    assert_eq!(map.entry_by_token(65535), Some((-1, &2)));
    assert_eq!(map.next_token(65535), None);
    assert_eq!(map.entry_by_token(65536), None);
    assert_eq!(map.value_by_token_mut(65536), None);
    walk_map!(map, plain);
    map.shrink_to_fit();
    assert!(!map.is_flat());
    walk_map!(map, plain);
}
