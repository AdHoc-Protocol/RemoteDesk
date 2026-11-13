// Copyright 2025 Chikirev Sirguy, Unirail Group
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Compact collections, queues, and reusable storage for protocol state.
//!
//! | Types | Storage and use |
//! |-------|-----------------|
//! | [`PrimitiveMap`], [`PrimitiveSet`] | Split key/value hash tables for cheap `Copy` keys |
//! | [`Map`], [`NullableSet`] | Cached hashes and borrowed-key lookups |
//! | [`ByteMap`], [`ByteSet`] | One-byte key domain with bitset presence |
//! | [`TwoBytesMap`], [`TwoBytesSet`] | Hash tables that switch to direct indexing when dense |
//! | [`BitList`], [`BitsList`] | Packed bits and fixed-width integers |
//! | [`NullablePrimitiveList`], [`NullableObjectList`] | Sparse or flat optional values |
//! | [`RingBuffer`] | Bounded queue with sequence-numbered slots |
//! | [`Pool`] | Thread-local magazines backed by a shared depot |
//!
//! Nullable collection variants reserve separate storage for the `None` key. Hash collections
//! accept a `BuildHasher` and default to `rustc_hash::FxBuildHasher`; key equality uses `Eq`.
//! [`ByteKey`] and [`TwoBytesKey`] define the key conversions for bounded key domains. Shared and
//! mutable access follow Rust borrowing rules. [`Token`] provides resumable slot access with
//! explicit invalidation rules.

mod lohi;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod token_tests;

pub mod bit_list;
pub mod bits_list;
pub mod byte_map;
pub mod byte_set;
pub mod float_key;
pub mod keys;
pub mod map;
pub mod nullable_byte_map;
pub mod nullable_byte_set;
pub mod nullable_map;
pub mod nullable_object_list;
pub mod nullable_primitive_list;
pub mod nullable_primitive_map;
pub mod nullable_primitive_set;
pub mod nullable_set;
pub mod nullable_two_bytes_map;
pub mod nullable_two_bytes_set;
pub mod pool;
pub mod primitive_map;
pub mod primitive_set;
pub mod ring_buffer;
pub mod token;
pub mod two_bytes_map;
pub mod two_bytes_set;

pub use bit_list::BitList;
pub use bits_list::BitsList;
pub use byte_map::ByteMap;
pub use byte_set::ByteSet;
pub use float_key::{F32, F64};
pub use keys::{ByteKey, TwoBytesKey};
pub use map::Map;
pub use nullable_byte_map::NullableByteMap;
pub use nullable_byte_set::NullableByteSet;
pub use nullable_map::NullableMap;
pub use nullable_object_list::NullableObjectList;
pub use nullable_primitive_list::NullablePrimitiveList;
pub use nullable_primitive_map::NullablePrimitiveMap;
pub use nullable_primitive_set::NullablePrimitiveSet;
pub use nullable_set::NullableSet;
pub use nullable_two_bytes_map::NullableTwoBytesMap;
pub use nullable_two_bytes_set::NullableTwoBytesSet;
pub use pool::Pool;
pub use primitive_map::PrimitiveMap;
pub use primitive_set::PrimitiveSet;
pub use ring_buffer::RingBuffer;
pub use token::{Token, NULL_TOKEN};
pub use two_bytes_map::TwoBytesMap;
pub use two_bytes_set::TwoBytesSet;

/// The hash of one entry of an unordered collection, alone: the collection sums them, so the order
/// does not count.
#[inline]
pub(crate) fn entry_hash<T: core::hash::Hash + ?Sized>(entry: &T) -> u64 {
    use core::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    entry.hash(&mut h);
    h.finish()
}
