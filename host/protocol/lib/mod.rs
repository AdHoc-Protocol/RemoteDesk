// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Rust runtime for AdHoc protocol connections.
//!
//! [`adhoc`] provides incremental encoding, decoding, transform chains, and session management.
//! [`collections`] contains compact collections and reusable storage. [`network`] supplies TCP,
//! WebSocket, datagram, and in-process transports. [`chacha20`], [`noise`], [`x25519`] and
//! [`chain`] provide encryption and key agreement, [`stages`] the built-in stages of a chain (Zstd, ChaCha20, Envelope), in the
//! native library [`crypto`].

pub mod adhoc;
pub mod chacha20;
pub mod chain;
pub mod collections;
pub mod crypto;
pub mod network;
pub mod noise;
pub mod stages;
pub mod x25519;
