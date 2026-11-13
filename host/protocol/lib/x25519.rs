// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! X25519 key generation and agreement in the native library (RFC 7748).
//!
//! Keys and shared secrets are 32-byte arrays. Scalar clamping is performed by the library.
//! [`agree`] rejects non-contributory shared secrets.

use crate::lib::crypto::api;

/// Key length in bytes (`X25519.KEY`).
pub const KEY: usize = 32;

/// Generates a random secret and its corresponding public key. Returns `(secret, public)`, each as
/// a 32-byte array.
pub fn keypair() -> ([u8; KEY], [u8; KEY]) {
    let (mut secret, mut public) = ([0u8; KEY], [0u8; KEY]);
    // SAFETY: the sizes the call takes
    unsafe { (api().x25519_keypair)(secret.as_mut_ptr(), public.as_mut_ptr()) };
    (secret, public)
}

/// The public key of `secret`: its product with the base point 9.
pub fn public_key(secret: &[u8; KEY]) -> [u8; KEY] {
    let mut public = [0u8; KEY];
    // SAFETY: the sizes the call takes
    unsafe { (api().x25519_public)(public.as_mut_ptr(), secret.as_ptr()) };
    public
}

/// Computes the raw shared secret for `secret` and the peer public key. Returns `None` for a
/// non-contributory result, including low-order peer keys.
pub fn agree(secret: &[u8; KEY], peer: &[u8; KEY]) -> Option<[u8; KEY]> {
    let mut shared = [0u8; KEY];
    // SAFETY: the sizes the call takes
    (unsafe { (api().x25519)(shared.as_mut_ptr(), secret.as_ptr(), peer.as_ptr()) } != 0).then_some(shared)
}

// Tests — RFC 7748 §5.2 vectors

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    /// RFC 7748 §5.2, test vector 1: the public key of the scalar.
    #[test]
    fn rfc7748_public_key() {
        let scalar = hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let expected = hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        assert_eq!(public_key(&scalar), expected);
    }

    /// RFC 7748 §5.2: the shared secret of the two test scalars.
    #[test]
    fn rfc7748_agree() {
        let a = hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let a_pub = hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        let b = hex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        let b_pub = hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let expected = hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        assert_eq!(agree(&a, &b_pub).unwrap(), expected);
        assert_eq!(agree(&b, &a_pub).unwrap(), expected, "agreement is symmetric");
    }

    /// RFC 7748 §6.1: a low-order peer key (all zero) is refused.
    #[test]
    fn low_order_peer_is_refused() {
        let (secret, _) = keypair();
        assert_eq!(agree(&secret, &[0u8; 32]), None);
    }

    #[test]
    fn keypair_roundtrip() {
        let (a_secret, a_pub) = keypair();
        let (b_secret, b_pub) = keypair();
        assert_eq!(agree(&a_secret, &b_pub).unwrap(), agree(&b_secret, &a_pub).unwrap());
    }
}
