// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Incremental protocol encoding, decoding, and connection state.
//!
//! * [`connection`] contains receivers, transmitters, resumable sessions, tunnels, and keyrings.
//! * [`BytesSrc`] and [`BytesDst`] connect protocol state machines to transports.
//! * [`chain`] composes transforms for packet fields and chunked streams.
//! * [`ascii`] and [`varint_text`] encode and decode text on the wire.
//! * [`helpers`] and [`utils`] provide numeric codecs, checksums, and buffer utilities.
//! * [`handlers`], [`spin_lock`], and [`dns`] provide event dispatch, synchronization, and TXT
//!   lookup.

pub mod ascii;
mod bytes;
pub use bytes::{BytesDst, BytesSrc, Spill};
pub mod chain;
pub mod connection;
pub mod dns;
pub mod handlers;
pub mod helpers;
pub mod spin_lock;
pub mod utils;
pub mod varint_text;
