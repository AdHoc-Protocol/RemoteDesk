// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Protocol connections and the state shared by their transport sessions.
//!
//! Receivers and transmitters perform incremental serialization. External connections expose
//! transport controls; keyrings, groups, control packets, and tunnels manage session behavior.

pub mod control;
pub mod external;
pub mod group;
pub mod keyring;
pub mod receiver;
pub mod sessions;
pub mod transmitter;
pub mod tunnel;
