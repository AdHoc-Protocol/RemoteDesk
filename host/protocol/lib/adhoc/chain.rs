// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Transforms applied to packet fields and chunked streams.
//!
//! The transmitter runs a chain from the field toward the wire; the receiver runs it in the
//! opposite direction. The connection selects stages by the chain identifier. Stage state is reused
//! across chunks and reset when a new stream starts.

use crate::lib::adhoc::connection::receiver::Rx;
use crate::lib::adhoc::connection::transmitter::Tx;

/// What is done with the sink or the source of a stream of bytes when the stream ends: `true` — it
/// is whole, `false` — torn (the connection lost, the pack dropped). Armed right before the stream
/// starts (`Rx::on_stream_complete`, `Tx::on_stream_complete`), it runs instead of the default -
/// the sink flushed, the source dropped - once.
pub type StreamComplete = Box<dyn FnOnce(bool) + Send>;

/// A stage of a chain with its design-time parameters (leaf-first order in a chain).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// A run of built-in stages - `[Zstd(level)]`, `[ChaCha20]`, `[Envelope]` - one after another:
    /// the native library runs them in one call a window ( [`crate::lib::stages`] ). `[Zstd]` - one
    /// zstd frame per stream; `[ChaCha20]` on a pack or a field - the newest key of the session, 12
    /// bytes in clear ahead of each pack (the id of the key and the sequence), so that every pack
    /// decrypts alone; `[Envelope]` - the pack sealed for one reader and signed by its author.
    Stages(crate::lib::stages::Run),
    /// Application-defined transform stage. The connection constructs its sending and receiving
    /// implementations and supplies the configured stage arguments.
    Custom(CustomStage),
}

/// The two sides of a custom stage with the parameters of its use: made once per level of nesting,
/// armed again for every stream.
#[derive(Clone, Copy, Debug)]
pub struct CustomStage {
    pub name: &'static str,
    /// The parameters, in the order the attribute of the stage declares them.
    pub args: &'static [Arg],
    pub tx: fn(&'static [Arg]) -> Box<dyn CustomTx>,
    pub rx: fn(&'static [Arg]) -> Box<dyn CustomRx>,
}

/// A parameter of a custom stage as the description gives it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arg {
    Int(i64),
    Float(f64),
    Str(&'static str),
    /// An array of integers ( any integer type, bool, char ) valued in the description.
    Ints(&'static [i64]),
    /// An array of float / double valued in the description.
    Floats(&'static [f64]),
    /// An array of strings valued in the description.
    Strs(&'static [&'static str]),
    /// Not valued in the description (a key, a nonce): the code of the stage gives it at run time.
    Runtime,
}

impl PartialEq for CustomStage {
    /// One stage, one use: the stages of a chain are compared to reuse what is made.
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.args == other.args
    }
}

impl Eq for CustomStage {}

/// The sending side of a custom stage.
pub trait CustomTx: Send {
    /// A new stream begins.
    fn rearm(&mut self) {}
    /// The window `dst` with the bytes of the stream, this stage applied: `inner` fills a window
    /// with the bytes of the stages nearer the leaf. `true` — the stream is out. Passing `dst` to
    /// `inner` as it is — the stage changes nothing.
    fn get_bytes(&mut self, dst: &mut Tx<'_>, inner: &mut dyn FnMut(&mut Tx<'_>) -> bool) -> bool;
}

/// The receiving side of a custom stage.
pub trait CustomRx: Send {
    /// A new stream begins.
    fn rearm(&mut self) {}
    /// The bytes of the window `src`, this stage undone, to `inner` — the stages nearer the leaf. A
    /// window with nothing in it asks to flush what the stage holds.
    fn put_bytes(&mut self, src: &mut Rx<'_>, inner: &mut dyn FnMut(&mut Rx<'_>));
}

/// The stages of a connection that has no chains.
pub fn no_stages(_chain: u32) -> &'static [Stage] {
    &[]
}

// stream packs
