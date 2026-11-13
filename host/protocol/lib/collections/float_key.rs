//! Floating-point wrappers with consistent `Eq` and `Hash` implementations.
//!
//! [`F32`] and [`F64`] treat positive and negative zero as equal and normalize all NaN
//! representations to one equality and hash value. Other values compare by their bits.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

macro_rules! float_key {
    ($name:ident, $t:ty, $bits:ty) => {
        /// A float as a key: equality and hash by the bits, `-0.0` is `0.0`, every NaN is one.
        #[derive(Copy, Clone, Default)]
        #[repr(transparent)]
        pub struct $name(pub $t);

        impl $name {
            /// The bits that decide equality and hash.
            #[inline(always)]
            pub fn key_bits(self) -> $bits {
                if self.0 == 0.0 {
                    0
                } else if self.0.is_nan() {
                    <$t>::NAN.to_bits()
                } else {
                    self.0.to_bits()
                }
            }
        }

        impl PartialEq for $name {
            #[inline(always)]
            fn eq(&self, other: &Self) -> bool {
                self.key_bits() == other.key_bits()
            }
        }

        impl Eq for $name {}

        impl Hash for $name {
            #[inline(always)]
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.key_bits().hash(state)
            }
        }

        impl Deref for $name {
            type Target = $t;
            #[inline(always)]
            fn deref(&self) -> &$t {
                &self.0
            }
        }

        impl From<$t> for $name {
            #[inline(always)]
            fn from(v: $t) -> Self {
                Self(v)
            }
        }

        impl From<$name> for $t {
            #[inline(always)]
            fn from(k: $name) -> Self {
                k.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.0, f)
            }
        }
    };
}

float_key!(F32, f32, u32);
float_key!(F64, f64, u64);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::collections::PrimitiveSet;

    #[test]
    fn zero_and_nan_are_one_key() {
        let mut s = PrimitiveSet::<F32>::new();
        s.insert(F32(0.0));
        s.insert(F32(-0.0));
        s.insert(F32(f32::NAN));
        s.insert(F32(-f32::NAN));
        s.insert(F32(1.5));
        assert_eq!(s.len(), 3);
        assert!(s.contains(F32(-0.0)));
    }
}
