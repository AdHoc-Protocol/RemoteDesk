// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! TCP, WebSocket, reliable datagram, and in-process byte transports.
//!
//! [`engine`] runs TCP I/O loops on `mio`, with one thread and poller per loop. [`TCP`] provides
//! clients, servers, connection slots, and WebSockets; [`UDP`] provides a datagram engine with
//! acknowledgement and replay. [`Wire`] connects a source and sink in process.
//!
//! [`buf`] manages transport buffers, [`admission`] limits accepted connections, and [`failure`]
//! routes errors. [`mux`] selects protocol connections on a shared port, [`reseat`] moves sessions
//! between transports, and [`noise_adapter`] applies encrypted records.
//!
//! A connection's serialization, I/O, and deadlines run on its assigned loop. Other threads submit
//! requests through the loop inbox and wake the poller when necessary.

#[allow(non_snake_case)]
pub mod TCP;
#[allow(non_snake_case)]
pub mod UDP;
pub mod admission;
pub mod buf;
pub mod engine;
pub mod failure;
pub mod mux;
pub mod noise_adapter;
pub mod reseat;
pub mod wire;

pub use wire::Wire;
