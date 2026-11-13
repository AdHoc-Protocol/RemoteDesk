// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Incremental packet decoding over successive byte windows.
//!
//! [`RxState`] retains partially decoded values, strings, and nested packet slots between calls.
//! [`Rx`] borrows that state together with the current input window and dereferences to it for
//! field access. Packet implementations consume the window through [`BytesDst::put_bytes`].
//!
//! [`RxState::write`] reads a packet identifier, asks [`RxHost`] for its destination, and resumes
//! decoding. Nested packets are entered through [`Rx::try_get_bytes`], which continues the saved
//! slot at the next nesting level. Decoding can pause at any byte without collecting the entire
//! packet first.

use std::ops::{Deref, DerefMut};

use crate::lib::adhoc::chain::{self, Stage};
use crate::lib::adhoc::connection::receiver::chunked::{RxChain, RxLeaf};

/// What the receiver does with the next bytes.
pub mod mode {
    pub const OK: u8 = 0;
    pub const RETRY: u8 = 1;
    pub const VAL4: u8 = 2;
    pub const VAL8: u8 = 3;
    pub const INT1: u8 = 4;
    pub const INT2: u8 = 5;
    pub const INT4: u8 = 6;
    pub const LEN0: u8 = 7;
    pub const LEN1: u8 = 8;
    pub const LEN2: u8 = 9;
    pub const VARINT: u8 = 10;
    pub const STR: u8 = 11;
    /// A 16-byte value the window cut: the low half gathers in `u8`, the high one in `u8_`.
    pub const VAL16: u8 = 12;
}

/// Why the stream of a connection cannot be parsed any further.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RxError {
    /// The pack id is of no pack this side receives.
    InvalidId(u32),
    /// A length, a count or a dimension above what the protocol allows.
    Overflow { got: u64, max: u64 },
    /// The bytes of a stream do not make what they carry: a stage could not take them, the stream
    /// was voided (`ABORT`) or ended before its pack.
    Broken(&'static str),
    /// A pack or an actor against the roles: the connection told the failure handler and reset.
    Denied,
}

impl std::fmt::Display for RxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RxError::InvalidId(id) => write!(f, "unknown pack id {id}"),
            RxError::Overflow { got, max } => write!(f, "overflow: {got} > {max}"),
            RxError::Broken(why) => write!(f, "broken stream: {why}"),
            RxError::Denied => write!(f, "against the roles"),
        }
    }
}

impl std::error::Error for RxError {}

impl RxError {
    /// What the handler of the errors is told ([`on_error`]).
    pub fn code(&self) -> crate::lib::adhoc::connection::receiver::on_error::RxOnError {
        match self {
            RxError::InvalidId(_) => crate::lib::adhoc::connection::receiver::on_error::RxOnError::InvalidId,
            RxError::Overflow { .. } => crate::lib::adhoc::connection::receiver::on_error::RxOnError::Overflow,
            RxError::Broken(_) => crate::lib::adhoc::connection::receiver::on_error::RxOnError::BytesDistortion,
            RxError::Denied => crate::lib::adhoc::connection::receiver::on_error::RxOnError::Rejected,
        }
    }

    /// To the handler of the errors of the receive: where - the receive `src`, the pack in flight
    /// in it.
    #[cold]
    pub fn report(&self, src: &RxState) {
        if matches!(self, RxError::Denied) {
            return; // the failure handler has heard it
        }
        crate::lib::adhoc::connection::receiver::on_error::rx_at(self.code(), src, self, None);
    }
}

/// A pack on the heap as `_Allocator::DEFAULT.new_P` gives it.
#[inline]
pub fn leak<T>(pack: Box<T>) -> std::ptr::NonNull<T> {
    // SAFETY: `Box::into_raw` is never null
    unsafe { std::ptr::NonNull::new_unchecked(Box::into_raw(pack)) }
}

/// The end of a pack [`leak`] gave.
#[inline]
pub fn unleak<T>(pack: std::ptr::NonNull<T>) {
    // SAFETY: `leak` made it, the end is the only one
    drop(unsafe { Box::from_raw(pack.as_ptr()) });
}

/// The place of a received pack: where its bytes go, and how it ends. Dropped, it ends.
pub struct Ref<T> {
    pack: std::ptr::NonNull<T>,
    end: fn(std::ptr::NonNull<T>),
}

// SAFETY: the place is owned by this reference, as a `Box` owns its value
unsafe impl<T: Send> Send for Ref<T> {}

impl<T> Ref<T> {
    /// `pack` lives until `end` is called with it, by this reference only.
    #[inline]
    pub fn new(pack: std::ptr::NonNull<T>, end: fn(std::ptr::NonNull<T>)) -> Self {
        Self { pack, end }
    }
}

impl<T> std::ops::Deref for Ref<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        // SAFETY: alive until the end, which only drop calls
        unsafe { self.pack.as_ref() }
    }
}

impl<T> std::ops::DerefMut for Ref<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: alive until the end, and this reference owns it
        unsafe { self.pack.as_mut() }
    }
}

impl<T> Drop for Ref<T> {
    #[inline]
    fn drop(&mut self) {
        (self.end)(self.pack)
    }
}

/// The field `new_P` of `_Allocator::DEFAULT`: what gives the place of a received pack. One load a pack.
pub struct Allocate<T> {
    set: std::sync::atomic::AtomicPtr<()>,
    default: fn(&dyn std::any::Any) -> std::ptr::NonNull<T>,
}

impl<T> Allocate<T> {
    pub const fn new(default: fn(&dyn std::any::Any) -> std::ptr::NonNull<T>) -> Self {
        Self { set: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()), default }
    }

    /// Assigns what gives the place: `scope` is the connection, `downcast_ref` gets it.
    pub fn set(&self, make: fn(&dyn std::any::Any) -> std::ptr::NonNull<T>) {
        self.set.store(make as *mut (), std::sync::atomic::Ordering::Release);
    }

    /// The place of a pack about to be received.
    #[inline]
    pub fn make(&self, scope: &dyn std::any::Any) -> std::ptr::NonNull<T> {
        let p = self.set.load(std::sync::atomic::Ordering::Acquire);
        if p.is_null() {
            return (self.default)(scope);
        }
        // SAFETY: only `set` stores here, a function of this type
        let make = unsafe { std::mem::transmute::<*mut (), fn(&dyn std::any::Any) -> std::ptr::NonNull<T>>(p) };
        make(scope)
    }
}

/// The field `free_P` of `_Allocator::DEFAULT`: the end of the place `new_P` gave.
pub struct Free<T> {
    set: std::sync::atomic::AtomicPtr<()>,
    default: fn(std::ptr::NonNull<T>),
}

impl<T> Free<T> {
    pub const fn new(default: fn(std::ptr::NonNull<T>)) -> Self {
        Self { set: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()), default }
    }

    /// Assigns the end of the place.
    pub fn set(&self, free: fn(std::ptr::NonNull<T>)) {
        self.set.store(free as *mut (), std::sync::atomic::Ordering::Release);
    }

    /// The end of the place given now: it stays with the place.
    #[inline]
    pub fn get(&self) -> fn(std::ptr::NonNull<T>) {
        let p = self.set.load(std::sync::atomic::Ordering::Acquire);
        if p.is_null() {
            return self.default;
        }
        // SAFETY: only `set` stores here, a function of this type
        unsafe { std::mem::transmute::<*mut (), fn(std::ptr::NonNull<T>)>(p) }
    }
}

/// The default of `new_P` and `free_P`: the places of packs that ended, each one serves the next pack with its memory. A
/// receive sets every field of a pack, a null one too. A lock a pack, so that a place may end on any thread: an arena of a
/// thread, of a batch, needs none - assign one of your own.
pub struct Arena<T> {
    free: parking_lot::Mutex<Vec<std::ptr::NonNull<T>>>,
}

// SAFETY: the places are owned by the arena until given out
unsafe impl<T: Send> Send for Arena<T> {}
unsafe impl<T: Send> Sync for Arena<T> {}

impl<T> Arena<T> {
    pub const fn new() -> Self {
        Self { free: parking_lot::Mutex::new(Vec::new()) }
    }

    /// A place that ended, or a new one.
    #[inline]
    pub fn take(&self, make: impl FnOnce() -> Box<T>) -> std::ptr::NonNull<T> {
        if let Some(p) = self.free.lock().pop() {
            return p;
        }
        leak(make())
    }

    /// The place serves the next pack.
    #[inline]
    pub fn give(&self, pack: std::ptr::NonNull<T>) {
        self.free.lock().push(pack);
    }
}

/// Where a pack being received goes: the place the application gives once the pack is to be read, or, when nobody is to
/// get it, a place of the library - the application is not asked. What is still here when the receive of the pack ends is
/// dropped.
pub enum Slot<T> {
    Empty,
    Held(Ref<T>),
    Skip(Box<T>),
}

impl<T> Default for Slot<T> {
    #[inline(always)]
    fn default() -> Self {
        Slot::Empty
    }
}

impl<T: Default> Slot<T> {
    /// The place the bytes of the pack go to: made at the first call.
    #[inline]
    pub fn dst(&mut self, dropping: bool, make: impl FnOnce() -> Ref<T>) -> &mut T {
        if let Slot::Empty = self {
            *self = if dropping { Slot::Skip(Box::default()) } else { Slot::Held(make()) };
        }
        match self {
            Slot::Held(r) => &mut **r,
            Slot::Skip(b) => &mut **b,
            Slot::Empty => unreachable!(),
        }
    }
}

impl<T> Slot<T> {
    /// The pack received, unless it was taken.
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut T> {
        match self {
            Slot::Held(r) => Some(&mut **r),
            _ => None,
        }
    }

    /// The pack is the application's from now on.
    #[inline]
    pub fn take(&mut self) -> Option<Ref<T>> {
        match std::mem::take(self) {
            Slot::Held(r) => Some(r),
            _ => None,
        }
    }
}

/// Incrementally decodes a packet from [`Rx`]. Returns `true` when complete or `false` when input
/// runs out; saved state continues with the next window.
pub trait BytesDst {
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool;
}

/// A pack in the heap: a field of a pack in a cycle (`A` in `B`, `B` in `A`) - Rust has no type of
/// infinite size.
impl<T: BytesDst + ?Sized> BytesDst for Box<T> {
    #[inline(always)]
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        (**self).put_bytes(src)
    }
}

/// What the application hears of the packs a receiver receives: `P` — the packs of the connection.
/// Set on the generated receiver (its `handler`); the receiving thread calls it.
pub trait EventsHandler<P: ?Sized>: Send + Sync {
    /// The id of a pack is read, the pack is to be received. A [`BytesCopy`] set in
    /// `src.bytes_copy` now gets the bytes of its body.
    fn on_receiving(&self, _src: &mut Rx<'_>, _pack: &P) {}
    /// The pack is received whole.
    fn on_received(&self, _st: &mut RxState, _pack: &P) {}
}

/// The bytes of the pack being received as the receiver takes them: to send them on as they came,
/// to log them. Set in `RxState::bytes_copy` for one pack ([`EventsHandler::on_receiving`]); it
/// gets the body, behind the pack id and the headers of the connection - the id is the pack's.
/// Taken away when the pack is whole or dropped.
pub trait BytesCopy: Send + Sync {
    /// The bytes of the pack this window held - more follow in the next one.
    fn pass(&self, bytes: &[u8]);
    /// The last bytes of the pack: it is whole.
    fn last(&self, bytes: &[u8]);
    /// The pack was dropped before it was whole, or its body never began: what was copied of it is
    /// void.
    fn fail(&self);
}

/// The generated receiver of a connection: where a pack goes once its id is read, and what is done
/// with it once whole.
pub trait RxHost {
    /// The id of a pack is read (`_OnReceiving`): the host prepares the pack it goes into. `false`
    /// — no such pack.
    fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool;
    /// Continues the pack being received; its state is in the slots of `src`.
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool;
    /// The pack is received whole and verified (`_OnReceived`).
    fn on_received(&mut self, st: &mut RxState);
    /// The pack is received whole and is not dispatched (`drop_received`): in place of
    /// [`on_received`](Self::on_received). Default: nothing.
    #[inline(always)]
    fn on_dropped(&mut self, _st: &mut RxState) {}
    /// What the transport handed over is parsed: a host that counts guaranteed packs reports its
    /// position here, once a receive call.
    fn on_window(&mut self, _st: &mut RxState) {}
    /// The pack in flight is dropped - a framing error, or the stream jumped: what the host fed
    /// with it ends too - a relayed frame (`Relay::inbound_lost`), a frame of a tunnel
    /// (`In::abandon`).
    fn on_abandon(&mut self) {}
    /// The stream goes on past bytes this receiver never got - the peer's journal does not reach
    /// its position: after `on_abandon`, the session hears it.
    fn on_lost(&mut self) {}
    /// The position of this receiver in the peer's journal, when the connection counts its
    /// guaranteed packs: a field of the session its receiver alone touches - on the thread that
    /// runs the connection, or before the session carries anything ( no lock, as the parse
    /// itself ). Default: none.
    fn position(&self) -> Option<&crate::lib::adhoc::utils::OwnCell<Position>> {
        None
    }
    /// The pack `id` is one of the packs the two sides agree by
    /// ([`crate::lib::adhoc::connection::control`]): on a new socket the receiver reads these only,
    /// before the guaranteed pack in progress goes on. Default: none is.
    fn service(&self, _id: u32) -> bool {
        false
    }
    /// The pack `id` is a frame of a tunnel with a Noise of its own ( `[Noise]` of its
    /// description ): it may come in a record in the clear of the Noise of the socket. Default: none
    /// is.
    #[inline(always)]
    fn clear(&self, _id: u32) -> bool {
        false
    }
    /// The pack of the agreement just received whole goes to the control
    /// ([`crate::lib::adhoc::connection::control::Control::heard`]): what it means to the receiver.
    /// Default: nothing.
    fn on_service(&mut self) -> crate::lib::adhoc::connection::control::Heard {
        crate::lib::adhoc::connection::control::Heard::Nothing
    }
    /// What failed in the session - a guaranteed pack dropped half-way -, to the handler of its
    /// transport. Default: nothing.
    fn failure(&self, _e: &(dyn std::error::Error + 'static)) {}
    /// Where the speed the receiver takes bytes at is kept, bytes a second ([`Receiver`] measures
    /// it once a receive call): a field of the generated connection. Default: none, not measured.
    fn speed(&self) -> Option<&std::sync::atomic::AtomicI64> {
        None
    }
    /// The stages of the chain `id` this host runs receiving by this connection.
    fn chain(_id: u32) -> &'static [Stage]
    where
        Self: Sized,
    {
        &[]
    }
}

macro_rules! slot_index {
    ($index:ident, $index_max:ident, $nulls:ident, $index_max_:ident, $next_index:ident, $next_index_case:ident, $null_at_index:ident, $nulls_:ident) => {
        #[inline(always)]
        pub fn $index_max_(&mut self, max: i32) -> i32 {
            self.$index = 0;
            self.$index_max = max;
            max
        }

        #[inline(always)]
        pub fn $next_index(&mut self) -> bool {
            self.$index += 1;
            self.$index < self.$index_max
        }

        /// The next index, and on it `next_ok_case` becomes the state.
        #[inline(always)]
        pub fn $next_index_case(&mut self, next_ok_case: u32) -> bool {
            self.$index += 1;
            if self.$index < self.$index_max {
                self.state = next_ok_case;
                return true;
            }
            false
        }

        #[inline(always)]
        pub fn $null_at_index(&self) -> bool {
            self.$nulls & 1 << (self.$index & 7) == 0
        }

        #[inline(always)]
        pub fn $nulls_(&mut self, nulls: u32, index: i32) {
            self.$index = index + trailing8zeros(nulls);
            self.$nulls = nulls;
        }
    };
}

/// Reusable decoding state for one nesting level, including the root packet.
///
/// Frequently accessed field state precedes the map key. `repr(C)` preserves the declared field
/// order. Array storage is resized by the packet decoder before elements are assigned.
#[derive(Debug, Default, Clone)]
#[repr(C)]
pub struct RxSlot {
    pub state: u32,
    pub fields_nulls: i32,

    pub index0: i32,
    pub index_max0: i32,
    pub nulls0: u32,

    pub index1: i32,
    pub index_max1: i32,
    pub nulls1: u32,

    pub index2: i32,
    pub index_max2: i32,
    pub nulls2: u32,

    pub index3: i32,
    pub index_max3: i32,
    pub nulls3: u32,

    pub max_items: i32,
    pub tmp: i32,
    /// The key of a map item received while its value is still on the way: a number or a string
    /// (`_string`).
    pub key: u64,
    /// The high half of an `Int128` / `UInt128` key; the low one is in `key`.
    pub key_: u64,
    pub key_str: String,
}

impl RxSlot {
    slot_index!(index0, index_max0, nulls0, index_max_0, next_index0, next_index0_case, null_at_index0, nulls_0);
    slot_index!(index1, index_max1, nulls1, index_max_1, next_index1, next_index1_case, null_at_index1, nulls_1);
    slot_index!(index2, index_max2, nulls2, index_max_2, next_index2, next_index2_case, null_at_index2, nulls_2);
    slot_index!(index3, index_max3, nulls3, index_max_3, next_index3, next_index3_case, null_at_index3, nulls_3);
}

/// The trailing zero bits of the low byte of `i`, 8 for 0 (`trailing8Zeros`).
#[inline(always)]
pub fn trailing8zeros(i: u32) -> i32 {
    ((i & 0xFF) | 0x100).trailing_zeros() as i32
}

/// What a receiver keeps from window to window.
pub struct RxState {
    pub mode: u8,
    pub u4: u32,
    pub u8: u64,
    pub u8_: u64,
    pub bytes_left: i32,
    pub bytes_max: i32,
    pub bits: u32,
    pub bit: i32,
    /// The pack being received is not to be dispatched when it ends (`drop_received`).
    pub drop_received: bool,
    /// What stopped the stream; set, the receiver parses nothing more until [`RxState::close`].
    pub error: Option<RxError>,
    /// The bytes of the pack id (1..=4).
    pub id_bytes: i32,
    /// The received string, taken by the pack with [`RxState::get_string`].
    str: Option<String>,
    /// A string cut by the end of a window and all ASCII so far: its bytes.
    bys: Vec<u8>,
    /// The memory of a string a field gave up ([`RxState::get_string_into`]): the next string
    /// received is made in it.
    spare: Vec<u8>,
    /// The memory of the strings collections gave up ([`RxState::give_string`]): the keys of a `Set` / `Map` received
    /// again give their memory to the keys that replace them. Of one pack only: what it did not take goes when it ends -
    /// a connection keeps nothing between packs.
    spares: Vec<Vec<u8>>,
    /// A string cut by the end of a window that met a character above 0x7F: its UTF-16 units. A
    /// high surrogate waiting for its low one: the pair is one character.
    hi: u16,
    wide: bool,
    dims: Vec<i32>,
    /// A file field the window cut (`get_file`): 0 none, 1 its length is coming, 2 its bytes; the
    /// sink and the bytes left.
    file_phase: u8,
    file_dst: Option<Box<dyn std::io::Write + Send>>,
    file_rem: u64,
    slots: Vec<RxSlot>,
    /// The level the code being run is at.
    level: usize,
    /// The levels a parked pack has open: the driver resumes them all.
    depth: usize,
    /// A pack is being received.
    in_pack: bool,
    /// The bytes being parsed are of a record in the clear ([`Receiver::on_clear`]).
    clear: bool,
    /// The pack being received - its id too - began in a record in the clear: its id is checked
    /// ([`RxHost::clear`]); it goes on so, or sealed - a replay.
    pack_clear: bool,
    /// The stages of the chains of the connection by their numbers ([`RxHost::chain`]).
    stages: fn(u32) -> &'static [Stage],
    /// The streams of chains in flight, by the level of the pack each one carries.
    pub(crate) chains: Vec<RxChain>,
    /// The bytes of the stream taken before the current window.
    consumed: i64,
    /// The keys of the session: the ChaCha20 stage of a pack chain finds the key a pack names.
    pub keys: std::sync::Arc<crate::lib::adhoc::connection::keyring::Keyring>,
    /// The keys of the Envelope stages of the connection ( [`crate::lib::chain::Envelope`] ): what the packs it takes are opened and checked with.
    pub envelope: Option<std::sync::Arc<crate::lib::chain::Envelope>>,
    /// Receive buffer backing the current window, or a null pointer for independent byte slices. A
    /// relay may transfer its allocation through [`Rx::take_storage`].
    owner: usize,
    /// The pack being received is kept as its bytes as well: its id and what of its body came so
    /// far.
    capture: Option<crate::lib::adhoc::connection::receiver::bytes_copy::Capture>,
    /// Where the bytes kept begin in the window being parsed.
    capture_from: usize,
    /// Gets the bytes of the pack being received: set for one pack, in
    /// [`EventsHandler::on_receiving`]; taken away when the pack is whole ([`BytesCopy::last`]) or
    /// dropped ([`BytesCopy::fail`]).
    pub bytes_copy: Option<std::sync::Arc<dyn BytesCopy>>,
    /// Where the bytes of the copy begin in the window being parsed; negative — not yet: the
    /// headers of the connection go first.
    copy_from: isize,
    /// The hook for the stream about to start ([`Rx::on_stream_complete`]): the stream takes it.
    pub(crate) stream_complete: Option<crate::lib::adhoc::chain::StreamComplete>,
}

/// A pack the copy follows dropped with the receiver: the copy hears it.
impl Drop for RxState {
    fn drop(&mut self) {
        if let Some(c) = self.bytes_copy.take() {
            c.fail();
        }
        for c in &mut self.chains {
            if c.complete.is_some() {
                c.end(false); // a stream torn with the receiver
            }
        }
    }
}

/// Where an error of a receive happened, for its handler: a receive, and the pack in flight in it.
impl std::fmt::Display for RxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a receive")?;
        if self.in_pack {
            write!(f, ", the pack in flight:
{self:?}")?;
        }
        Ok(())
    }
}

/// The pack in flight, level by level: a line a level, indented by its depth, with the state of its
/// slot - `<` marks the level being parsed; then the mode and what a value the window cut has left
/// to read. Empty between packs.
impl std::fmt::Debug for RxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.in_pack {
            return Ok(());
        }
        for (i, s) in self.slots[..self.depth.min(self.slots.len())].iter().enumerate() {
            writeln!(f, "{:\t<i$}level {i}\t{}{}", "", s.state, if i == self.level { " <" } else { "" })?;
        }
        write!(f, "mode {} bytes_left {} of {}", self.mode, self.bytes_left, self.bytes_max)?;
        match &self.error {
            Some(e) => write!(f, " error {e:?}"),
            None => Ok(()),
        }
    }
}

impl RxState {
    pub fn new(id_bytes: i32) -> Self {
        Self {
            mode: mode::OK,
            u4: 0,
            u8: 0,
            u8_: 0,
            bytes_left: id_bytes,
            bytes_max: id_bytes,
            bits: 0,
            bit: 0,
            drop_received: false,
            error: None,
            id_bytes,
            str: None,
            bys: Vec::new(),
            spare: Vec::new(),
            spares: Vec::new(),
            hi: 0,
            wide: false,
            dims: Vec::new(),
            file_phase: 0,
            file_dst: None,
            file_rem: 0,
            slots: vec![RxSlot::default()],
            level: 0,
            depth: 1,
            in_pack: false,
            clear: false,
            pack_clear: false,
            stages: chain::no_stages,
            chains: Vec::new(),
            consumed: 0,
            keys: Default::default(),
            envelope: None,
            owner: 0,
            capture: None,
            capture_from: 0,
            bytes_copy: None,
            copy_from: 0,
            stream_complete: None,
        }
    }

    /// Between packs: no pack, nor a pack id, is received in part (`idle`). A pack id the window
    /// cut counts - a new frame there tears it, a reseat there would graft the next connection's
    /// bytes onto it.
    #[inline]
    pub fn idle(&self) -> bool {
        !self.in_pack && self.bytes_left == self.id_bytes
    }

    /// Forgets the pack in progress — a pack id read in part too — and waits for a pack id
    /// (`Abandon`).
    pub fn abandon(&mut self) {
        self.in_pack = false;
        self.level = 0;
        self.depth = 1;
        self.mode = mode::OK;
        self.bytes_left = self.id_bytes;
        self.bytes_max = self.id_bytes;
        self.u4 = 0;
        self.str = None;
        self.drop_spares();
        self.capture = None;
        if let Some(c) = self.bytes_copy.take() {
            c.fail();
        }
        // the sinks of the streams and the file in flight go now, unflushed: the stream was torn
        self.file_phase = 0;
        self.file_dst = None;
        for c in &mut self.chains {
            c.end(false); // the hook of a stream torn hears it
        }
        self.stream_complete = None;
    }

    /// The pack received last, kept as its bytes ([`Rx::capture`]): to be sent on as it came, by
    /// many connections.
    #[inline]
    pub fn take_capture(&mut self) -> Option<crate::lib::adhoc::connection::receiver::bytes_copy::Capture> {
        self.capture.take()
    }

    /// The bytes of the window up to `to` go to the pack being captured.
    #[inline]
    fn keep(&mut self, src: &[u8], to: usize) {
        if let Some(c) = &mut self.capture {
            c.body_mut().extend_from_slice(&src[self.capture_from..to]);
        }
    }

    /// Ends the session (`Close`): nothing of it stays, and the error is gone.
    pub fn close(&mut self) {
        self.abandon();
        self.u8_ = 0;
        self.drop_received = false;
        self.error = None;
        for s in &mut self.slots {
            *s = RxSlot::default();
        }
    }

    #[inline(always)]
    pub fn get_bool_(&self) -> bool {
        self.u4 == 1
    }
    #[inline(always)]
    pub fn get_sbyte_(&self) -> i8 {
        self.u4 as i8
    }
    #[inline(always)]
    pub fn get_byte_(&self) -> u8 {
        self.u4 as u8
    }
    #[inline(always)]
    pub fn get_short_(&self) -> i16 {
        self.u4 as i16
    }
    #[inline(always)]
    pub fn get_ushort_(&self) -> u16 {
        self.u4 as u16
    }
    #[inline(always)]
    pub fn get_char_(&self) -> u16 {
        self.u4 as u16
    }
    #[inline(always)]
    pub fn get_int_(&self) -> i32 {
        self.u4 as i32
    }
    #[inline(always)]
    pub fn get_uint_(&self) -> u32 {
        self.u4
    }
    #[inline(always)]
    pub fn get_long_(&self) -> i64 {
        self.u8 as i64
    }
    #[inline(always)]
    pub fn get_ulong_(&self) -> u64 {
        self.u8
    }
    #[inline(always)]
    pub fn get_int128_(&self) -> i128 {
        self.get_uint128_() as i128
    }
    #[inline(always)]
    pub fn get_uint128_(&self) -> u128 {
        (self.u8_ as u128) << 64 | self.u8 as u128
    }
    #[inline(always)]
    pub fn get_float_(&self) -> f32 {
        f32::from_bits(self.u4)
    }
    #[inline(always)]
    pub fn get_double_(&self) -> f64 {
        f64::from_bits(self.u8)
    }
    /// The bits `try_get_bits` read.
    #[inline(always)]
    pub fn get_bits_(&self) -> u32 {
        self.u4
    }

    /// The dimension at `index` (`dim(int)`).
    #[inline]
    pub fn dim(&self, index: usize) -> i32 {
        self.dims[index]
    }

    /// Decodes ZigZag (`zig_zag`).
    #[inline(always)]
    pub fn zig_zag(src: u64) -> i64 {
        (-((src & 1) as i64)) ^ ((src >> 1) as i64)
    }

    /// `Receiver.Write`: parses `src` as far as it goes. Returns the bytes taken and whether a pack
    /// ended right there — then the caller dispatches it (or the framing checks its CRC first) and
    /// calls again with the rest. Negative — the stream is broken: [`RxState::error`] says why.
    pub fn write<H: RxHost>(&mut self, src: &[u8], host: &mut H) -> (isize, bool) {
        if self.error.is_some() {
            return (-1, false);
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.write_in(src, host))) {
            Ok(r) => r,
            Err(_) => {
                // a pack body panicked: every byte offset after it is untrustworthy - the pack in
                // flight is dropped and the stream broken, as by a parse error; the default hook
                // printed the panic already
                self.abandon();
                self.error = Some(RxError::Broken("a pack body panicked"));
                (-1, false)
            }
        }
    }

    fn write_in<H: RxHost>(&mut self, src: &[u8], host: &mut H) -> (isize, bool) {
        if self.idle() {
            self.pack_clear = self.clear;
        } else if self.clear && !self.pack_clear {
            // a pack begun sealed goes on in the clear: spliced. The other way is a replay of the journal, sealed, from the
            // middle of a frame the peer had in the clear
            self.error.get_or_insert(RxError::Broken("a pack begun sealed goes on in a record in the clear"));
            self.abandon();
            return (-1, false);
        }
        self.stages = H::chain;
        self.capture_from = 0;
        if 0 <= self.copy_from {
            self.copy_from = 0;
        }
        let mut rx = Rx { buffer: src, byte_: 0, byte_max: src.len(), st: self };
        while rx.byte_ < rx.byte_max {
            if !rx.st.in_pack {
                if rx.st.capture.is_some() {
                    rx.st.capture = None; // the pack before was captured and not taken
                }
                if rx.not_get4() {
                    break; // the id goes on in the next window
                }
                let id = rx.st.u4;
                rx.st.u4 = 0;
                rx.st.drop_received = false;
                rx.st.slots[0].state = 0;
                rx.st.u8_ = 0;
                rx.st.level = 0;
                rx.st.depth = 1;
                rx.st.copy_from = rx.byte_ as isize; // the id is read: the copy starts here, unless the host defers it
                if rx.st.pack_clear && !host.clear(id) {
                    // only frames of tunnels with a Noise of their own go in the clear: anything else there is forged
                    rx.st.error.get_or_insert(RxError::Broken("a record in the clear carries what is not a frame of a tunnel with a Noise of its own"));
                    rx.st.abandon();
                    return (-1, false);
                }
                if !host.on_receiving(&mut rx, id) {
                    rx.st.error.get_or_insert(RxError::InvalidId(id));
                    rx.st.abandon();
                    return (-1, false);
                }
                rx.st.in_pack = true;
            } else {
                match rx.resume() {
                    1 => {}
                    0 => break,
                    _ => return (-1, false),
                }
            }
            rx.st.mode = mode::OK;
            rx.st.level = 0;
            if !host.put_bytes(&mut rx) {
                if rx.st.error.is_some() {
                    return (-1, false);
                }
                break;
            }
            if rx.st.error.is_some() {
                return (-1, false); // whole, but a value out of its limits broke it: not delivered
            }
            // the pack is whole
            rx.st.drop_spares();
            rx.st.keep(rx.buffer, rx.byte_);
            if let Some(c) = rx.st.bytes_copy.take() {
                match usize::try_from(rx.st.copy_from) {
                    Ok(from) => c.last(&rx.buffer[from..rx.byte_]),
                    Err(_) => c.fail(), // its body never began
                }
            }
            rx.st.in_pack = false;
            rx.st.depth = 1;
            rx.st.u4 = 0;
            rx.st.bytes_left = rx.st.id_bytes;
            rx.st.bytes_max = rx.st.id_bytes;
            rx.st.consumed += rx.byte_ as i64;
            return (rx.byte_ as isize, true);
        }
        let n = rx.byte_ as isize;
        if rx.st.error.is_some() {
            return (-1, false);
        }
        rx.st.keep(rx.buffer, rx.byte_);
        if let (Some(c), Ok(from)) = (&rx.st.bytes_copy, usize::try_from(rx.st.copy_from)) {
            c.pass(&rx.buffer[from..rx.byte_]);
        }
        rx.st.consumed += n as i64;
        (n, false)
    }

    /// The bytes of the stream taken so far: the place of a pack in it. Behind the pack once it is
    /// whole.
    #[inline]
    pub fn consumed(&self) -> i64 {
        self.consumed
    }

    /// [`write_all_until`](Self::write_all_until) over `[at, end)` of the receive buffer of the
    /// connection. A relay may take its storage over ([`Rx::take_storage`]).
    pub fn write_all_block<H: RxHost>(&mut self, block: &crate::lib::network::buf::RxBuf, at: usize, end: usize, host: &mut H, stop: Option<&std::sync::atomic::AtomicBool>) -> isize {
        // SAFETY: `[at, end)` are the connection's plain bytes; nobody writes them while they are
        // parsed
        let src = unsafe { block.slice(at, end) };
        self.owner = block as *const crate::lib::network::buf::RxBuf as usize;
        let n = self.write_all_until(src, host, stop);
        self.owner = 0;
        n
    }

    /// Parses `src` to its end without framing, dispatching each pack the moment it is whole.
    /// Negative — broken.
    #[inline]
    pub fn write_all<H: RxHost>(&mut self, src: &[u8], host: &mut H) -> isize {
        self.write_all_until(src, host, None)
    }

    /// As [`write_all`](Self::write_all), and `stop` set by the handler of a pack stops it right
    /// behind that pack: the rest is left to the caller (the session moved, its receiver takes the
    /// rest — [`crate::lib::network::reseat`]).
    pub fn write_all_until<H: RxHost>(&mut self, mut src: &[u8], host: &mut H, stop: Option<&std::sync::atomic::AtomicBool>) -> isize {
        let all = src.len();
        loop {
            let (n, done) = self.write(src, host);
            if n < 0 {
                if let Some(e) = &self.error {
                    e.report(self);
                }
                return n;
            }
            if done {
                if self.drop_received {
                    self.drop_received = false;
                    host.on_dropped(self);
                } else {
                    host.on_received(self);
                }
                // a plain load a pack: the flag is up only after a move, the swap that takes it down
                // only then
                if stop.is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed) && s.swap(false, std::sync::atomic::Ordering::AcqRel)) {
                    src = &src[n as usize..];
                    host.on_window(self);
                    return (all - src.len()) as isize; // the session moved: the rest goes to its receiver
                }
            }
            src = &src[n as usize..];
            if src.is_empty() || !done {
                host.on_window(self);
                return (all - src.len()) as isize; // a pack that waits (a relay) leaves the rest to the transport
            }
        }
    }
}

/// A receiver over one window of bytes.
pub struct Rx<'a> {
    pub buffer: &'a [u8],
    pub byte_: usize,
    pub byte_max: usize,
    st: &'a mut RxState,
}

impl Deref for Rx<'_> {
    type Target = RxState;
    #[inline(always)]
    fn deref(&self) -> &RxState {
        self.st
    }
}

impl DerefMut for Rx<'_> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut RxState {
        self.st
    }
}

impl<'a> Rx<'a> {
    /// The pack whose id was just read is kept as its bytes as well, in the region `OnReceiving`:
    /// received whole, it is taken by [`RxState::take_capture`] and sent on as it came - by many
    /// connections ([`crate::lib::adhoc::connection::transmitter::broadcaster::Broadcaster`]).
    pub fn capture(&mut self, id: u32) {
        self.st.capture = Some(crate::lib::adhoc::connection::receiver::bytes_copy::Capture::start(id)); // the pack before, captured and not taken, goes back to the pool
        self.st.capture_from = self.byte_;
    }

    /// The body of a pack captured begins here: what was kept of it before - the headers of the
    /// connection - is dropped. The copy of [`RxState::bytes_copy`] begins here too.
    #[inline]
    pub fn capture_body(&mut self) {
        if let Some(c) = &mut self.st.capture {
            c.body_mut().clear();
            self.st.capture_from = self.byte_;
        }
        self.st.copy_from = self.byte_ as isize;
    }

    /// The copy of the pack whose id was just read waits for its body: the headers of the
    /// connection go first, it begins at [`capture_body`](Self::capture_body).
    #[inline]
    pub fn defer_copy(&mut self) {
        self.st.copy_from = -1;
    }

    /// What is done with the sink of the stream about to start: `hook(true)` when it is whole,
    /// `hook(false)` when it is torn - instead of the default, the sink flushed. The code that
    /// gives the sink calls it right before; the stream takes it once.
    #[inline]
    pub fn on_stream_complete(&mut self, hook: crate::lib::adhoc::chain::StreamComplete) {
        self.st.stream_complete = Some(hook);
    }

    /// Nothing is done with the sink of the stream about to start, not even the flush.
    #[inline]
    pub fn on_stream_complete_none(&mut self) {
        self.st.stream_complete = Some(Box::new(|_| {}));
    }

    /// A receiver of `st` over one `buffer`; a pack from pieces one after another - [`Virtual`].
    pub fn new(buffer: &'a [u8], st: &'a mut RxState) -> Self {
        let n = buffer.len();
        Self { buffer, byte_: 0, byte_max: n, st }
    }

    #[inline(always)]
    pub fn remaining(&self) -> usize {
        self.byte_max - self.byte_
    }

    /// `dst` from its start, on the level the code is at: true — it is received whole in this
    /// window.
    #[inline]
    pub fn get_bytes_dst<D: BytesDst + ?Sized>(&mut self, dst: &mut D) -> bool {
        self.slot().state = 0;
        dst.put_bytes(self)
    }

    /// The window lies in the receive buffer of the connection and runs to the end of the bytes
    /// received: a relay may take the storage over whole.
    #[inline(always)]
    pub fn can_take(&self) -> bool {
        // SAFETY: set by `write_all_block` for the length of the parse
        self.st.owner != 0 && unsafe { &*(self.st.owner as *const crate::lib::network::buf::RxBuf) }.ends_at(self.buffer.as_ptr().wrapping_add(self.byte_max))
    }

    /// The length of the receive buffer the window lies in ([`can_take`](Self::can_take)).
    #[inline(always)]
    pub fn storage_len(&self) -> usize {
        // SAFETY: as in `can_take`
        unsafe { &*(self.st.owner as *const crate::lib::network::buf::RxBuf) }.len()
    }

    /// Takes the receive buffer over ([`can_take`](Self::can_take)): `with`, of its length, goes in
    /// its place and the storage the window lies in comes out - its bytes stay where they are, the
    /// window's slice stays valid. The connection reads on into `with`.
    #[inline]
    pub fn take_storage(&self, with: crate::lib::network::buf::Buf) -> crate::lib::network::buf::Buf {
        debug_assert!(self.can_take());
        // SAFETY: as in `can_take`
        unsafe { &*(self.st.owner as *const crate::lib::network::buf::RxBuf) }.take(with)
    }

    /// The slot of the level the code being run is at.
    #[inline(always)]
    pub fn slot(&mut self) -> &mut RxSlot {
        let l = self.st.level;
        // SAFETY: `level < slots.len()` - `try_get_bytes` grows `slots` before it goes down a
        // level.
        unsafe { self.st.slots.get_unchecked_mut(l) }
    }

    /// The outer index of the level (`slot().index0`) through a shared borrow: an item of a
    /// multi-dimensional field is taken with it inside the arguments of a call on the same
    /// transmitter / receiver.
    #[inline(always)]
    pub fn index0(&self) -> usize {
        // SAFETY: as in `slot`
        unsafe { self.st.slots.get_unchecked(self.st.level).index0 as usize }
    }

    #[inline(always)]
    pub fn state(&self) -> u32 {
        // SAFETY: as in `slot`
        unsafe { self.st.slots.get_unchecked(self.st.level).state }
    }

    #[inline(always)]
    pub fn set_state(&mut self, state: u32) {
        self.slot().state = state;
    }

    /// The window ran out: the pack goes on at `the_case` (`retry_at`).
    #[inline(always)]
    pub fn retry_at(&mut self, the_case: u32) {
        self.slot().state = the_case;
        self.st.mode = mode::RETRY;
    }

    #[inline(always)]
    pub fn has_bytes(&mut self, next_case: u32) -> bool {
        if self.byte_ < self.byte_max {
            return true;
        }
        self.retry_at(next_case);
        false
    }

    /// Completes the value the previous window parked (`mode`): 1 — done, 0 — this window ran out
    /// too, -1 — broken (the error is set).
    pub(crate) fn resume(&mut self) -> i8 {
        match self.st.mode {
            mode::INT1 => {
                if self.not_get4() {
                    return 0;
                }
                self.st.u8 = self.st.u4 as i8 as u64;
            }
            mode::INT2 => {
                if self.not_get4() {
                    return 0;
                }
                self.st.u8 = self.st.u4 as i16 as u64;
            }
            mode::INT4 => {
                if self.not_get4() {
                    return 0;
                }
                self.st.u8 = self.st.u4 as i32 as u64;
            }
            mode::VAL4 => {
                if self.not_get4() {
                    return 0;
                }
            }
            mode::VAL8 => {
                let r = self.remaining().min(i32::MAX as usize) as i32; // a window past 2 GB: more than any value needs
                if r < self.st.bytes_left {
                    let shift = (self.st.bytes_max - self.st.bytes_left) * 8;
                    self.st.u8 |= self.get8(r as usize) << shift;
                    self.st.bytes_left -= r;
                    return 0;
                }
                let (left, shift) = (self.st.bytes_left, (self.st.bytes_max - self.st.bytes_left) * 8);
                self.st.u8 |= self.get8(left as usize) << shift;
            }
            mode::VAL16 => {
                let r = self.remaining().min(16) as i32;
                let at = 16 - self.st.bytes_left;
                if r < self.st.bytes_left {
                    self.get16_at(at, r);
                    self.st.bytes_left -= r;
                    return 0;
                }
                let left = self.st.bytes_left;
                self.get16_at(at, left);
            }
            mode::LEN0 | mode::LEN1 | mode::LEN2 => {
                if self.not_get4() {
                    return 0;
                }
                let len = self.st.u4 as i32;
                let level = self.st.depth - 1;
                let ok = match self.st.mode {
                    mode::LEN0 => self.st.slots[level].check_len0(len),
                    mode::LEN1 => self.st.slots[level].check_len1(len),
                    _ => self.st.slots[level].check_len2(len),
                };
                if let Err(e) = ok {
                    self.st.error = Some(e);
                    return -1;
                }
            }
            mode::VARINT => {
                if !self.varint() {
                    return 0;
                }
            }
            mode::STR => {
                if self.st.u8_ == u64::MAX {
                    // the length is still unfinished
                    if !self.varint() {
                        return 0;
                    }
                    match self.check_length_and_getting_string() {
                        Ok(true) => {}
                        Ok(false) => return 0,
                        Err(e) => {
                            self.st.error = Some(e);
                            return -1;
                        }
                    }
                } else if !self.getting_string() {
                    return 0;
                }
            }
            _ => {}
        }
        1
    }

    // nested

    /// A pack nested in the one being received (`try_get_bytes`): goes one level down and receives
    /// it there. `this_case` — the case of the pack that calls it: a parked nested pack is resumed
    /// from it.
    pub fn try_get_bytes<D: BytesDst + ?Sized>(&mut self, dst: &mut D, this_case: u32) -> bool {
        let parent = self.st.level;
        let level = parent + 1;
        if self.st.depth <= level {
            // a fresh nested pack
            if self.st.slots.len() <= level {
                self.st.slots.push(RxSlot::default());
            }
            self.st.slots[level].state = 0;
            self.st.u8_ = 0;
            self.st.depth = level + 1;
        }
        self.st.level = level;
        if dst.put_bytes(self) {
            self.st.level = parent;
            self.st.depth = level;
            return true;
        }
        self.st.slots[parent].state = this_case;
        false
    }

    /// The bytes of the stream taken so far, up to where this window is read: what
    /// [`RxState::consumed`] says once the call is over, the window's own part counted already.
    #[inline]
    pub fn consumed(&self) -> i64 {
        self.st.consumed + self.byte_ as i64
    }

    /// The place of the pack that starts in the stream: before its id.
    #[inline]
    pub fn pack_at(&self) -> i64 {
        self.st.consumed + self.byte_ as i64 - self.st.id_bytes as i64
    }

    // chains

    /// The same receiver over another window: a stage of a chain hands its bytes to the stages
    /// nearer the leaf there.
    #[inline]
    pub fn sub<'b>(&'b mut self, buffer: &'b [u8]) -> Rx<'b> {
        Rx { buffer, byte_: 0, byte_max: buffer.len(), st: &mut *self.st }
    }

    // nulls

    /// The byte of the null fields of the current pack (`get_fields_nulls`).
    #[inline]
    pub fn get_fields_nulls(&mut self, this_case: u32) -> bool {
        if self.byte_ < self.byte_max {
            let b = self.take::<1>()[0] as i32;
            self.slot().fields_nulls = b;
            return true;
        }
        self.retry_at(this_case);
        false
    }

    /// The field is null: its bit is not set (`is_null`).
    #[inline(always)]
    pub fn is_null(&mut self, field: i32) -> bool {
        self.slot().fields_nulls & field == 0
    }

    #[inline(always)]
    pub fn byte_nulls_shift(&mut self, shift: u8) -> bool {
        self.st.u4 = self.get_byte() as u32;
        if self.st.u4 == 0 {
            return false;
        }
        self.st.u8_ |= (self.st.u4 as u64) << shift;
        self.st.u8 = self.st.u8_;
        true
    }

    #[inline(always)]
    pub fn byte_nulls_value(&mut self, null_value: u64) -> bool {
        self.st.u4 = self.get_byte() as u32;
        if self.st.u4 == 0 {
            return false;
        }
        self.st.u8_ |= null_value;
        self.st.u8 = self.st.u8_;
        true
    }

    #[inline(always)]
    pub fn byte_nulls(&mut self, shift: u8, null_value: u64) -> bool {
        self.st.u4 = self.get_byte() as u32;
        if self.st.u4 == 0 {
            return false;
        }
        self.st.u8_ |= if self.st.u4 == 0xFF { null_value } else { (self.st.u4 as u64) << shift };
        self.st.u8 = self.st.u8_;
        true
    }

    // fixed width

    #[inline(always)]
    pub fn has_1bytes(&mut self, get_case: u32) -> bool {
        0 < self.byte_max - self.byte_ || self.retry_get4(1, get_case)
    }
    #[inline(always)]
    pub fn has_2bytes(&mut self, get_case: u32) -> bool {
        1 < self.byte_max - self.byte_ || self.retry_get4(2, get_case)
    }
    #[inline(always)]
    pub fn has_4bytes(&mut self, get_case: u32) -> bool {
        3 < self.byte_max - self.byte_ || self.retry_get4(4, get_case)
    }
    #[inline(always)]
    pub fn has_8bytes(&mut self, get_case: u32) -> bool {
        7 < self.byte_max - self.byte_ || self.retry_get8(8, get_case)
    }
    #[inline(always)]
    pub fn has_16bytes(&mut self, get_case: u32) -> bool {
        15 < self.byte_max - self.byte_ || self.retry_get16(get_case)
    }

    #[inline(always)]
    pub fn get_bool(&mut self) -> bool {
        self.get_byte() == 1
    }
    #[inline(always)]
    pub fn get_sbyte(&mut self) -> i8 {
        self.get_byte() as i8
    }
    /// The next `N` bytes: the room is checked already ( `has_Nbytes`, `remaining` ).
    #[inline(always)]
    fn take<const N: usize>(&mut self) -> [u8; N] {
        debug_assert!(self.byte_ + N <= self.byte_max && self.byte_max <= self.buffer.len());
        // SAFETY: the caller checked `byte_ + N <= byte_max`; `byte_max` is never past the end of
        // `buffer` - it is set from its length ( `new`, `sub`, `write_in` ) and only narrowed
        // ( a stream, a tunnel )
        let v = unsafe { (self.buffer.as_ptr().add(self.byte_) as *const [u8; N]).read_unaligned() };
        self.byte_ += N;
        v
    }

    /// The fixed-width getters below read what `has_Nbytes` found room for: no second check.
    #[inline(always)]
    pub fn get_byte(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    #[inline(always)]
    pub fn get_short(&mut self) -> i16 {
        self.get_ushort() as i16
    }
    #[inline(always)]
    pub fn get_ushort(&mut self) -> u16 {
        u16::from_le_bytes(self.take())
    }
    #[inline(always)]
    pub fn get_char(&mut self) -> u16 {
        self.get_ushort()
    }
    #[inline(always)]
    pub fn get_int(&mut self) -> i32 {
        self.get_uint() as i32
    }
    #[inline(always)]
    pub fn get_uint(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    #[inline(always)]
    pub fn get_long(&mut self) -> i64 {
        self.get_ulong() as i64
    }
    #[inline(always)]
    pub fn get_ulong(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
    #[inline(always)]
    pub fn get_int128(&mut self) -> i128 {
        self.get_uint128() as i128
    }
    #[inline(always)]
    pub fn get_uint128(&mut self) -> u128 {
        u128::from_le_bytes(self.take())
    }
    #[inline(always)]
    pub fn get_float(&mut self) -> f32 {
        f32::from_bits(self.get_uint())
    }
    #[inline(always)]
    pub fn get_double(&mut self) -> f64 {
        f64::from_bits(self.get_ulong())
    }

    // into u8

    pub fn get_sbyte_u8(&mut self, get_case: u32) -> bool {
        if 0 < self.remaining() {
            self.st.u8 = self.get_byte() as i8 as u64; // signed, as the resume of a byte the window cut (`mode::INT1`)
            return true;
        }
        self.retry_get4(1, get_case);
        self.st.mode = mode::INT1;
        false
    }

    pub fn get_byte_u8(&mut self, get_case: u32) -> bool {
        if self.remaining() == 0 {
            return self.retry_get8(1, get_case);
        }
        self.st.u8 = self.get_byte() as u64;
        true
    }

    pub fn get_short_u8(&mut self, get_case: u32) -> bool {
        if 1 < self.remaining() {
            self.st.u8 = self.get_short() as u64;
            return true;
        }
        self.retry_get4(2, get_case);
        self.st.mode = mode::INT2;
        false
    }

    pub fn get_ushort_u8(&mut self, get_case: u32) -> bool {
        if self.remaining() < 2 {
            return self.retry_get8(2, get_case);
        }
        self.st.u8 = self.get_ushort() as u64;
        true
    }

    pub fn get_int_u8(&mut self, get_case: u32) -> bool {
        if 3 < self.remaining() {
            self.st.u8 = self.get_int() as u64;
            return true;
        }
        self.retry_get4(4, get_case);
        self.st.mode = mode::INT4;
        false
    }

    pub fn get_uint_u8(&mut self, get_case: u32) -> bool {
        if self.remaining() < 4 {
            return self.retry_get8(4, get_case);
        }
        self.st.u8 = self.get_uint() as u64;
        true
    }

    pub fn get_long_u8(&mut self, get_case: u32) -> bool {
        if self.remaining() < 8 {
            return self.retry_get8(8, get_case);
        }
        self.st.u8 = self.get_ulong();
        true
    }

    pub fn get_ulong_u8(&mut self, get_case: u32) -> bool {
        self.get_long_u8(get_case)
    }

    // 8

    /// `bytes` (1..=8) into `u8` (`try_get8`).
    #[inline(always)]
    pub fn try_get8(&mut self, bytes: i32, get8_case: u32) -> bool {
        if self.remaining() < bytes as usize {
            return self.retry_get8(bytes, get8_case);
        }
        self.st.u8 = self.get8(bytes as usize);
        true
    }

    /// `bytes_left` bytes into `u8` (`try_get8(uint)`).
    #[inline(always)]
    pub fn try_get8_left(&mut self, next_case: u32) -> bool {
        let b = self.st.bytes_left;
        self.try_get8(b, next_case)
    }

    /// Takes what the window holds of a value of `bytes` bytes and parks the rest (`retry_get8`).
    /// Always false.
    #[cold]
    #[inline(never)]
    pub fn retry_get8(&mut self, bytes: i32, get8_case: u32) -> bool {
        let r = self.remaining();
        self.st.bytes_max = bytes;
        self.st.bytes_left = bytes - r as i32;
        self.st.u8 = self.get8(r);
        self.slot().state = get8_case;
        self.st.mode = mode::VAL8;
        false
    }

    // 16

    /// Takes what the window holds of an `Int128` / `UInt128` and parks the rest (`retry_get16`).
    /// Always false.
    #[cold]
    #[inline(never)]
    pub fn retry_get16(&mut self, get16_case: u32) -> bool {
        let r = self.remaining() as i32; // fewer than 16
        self.st.bytes_max = 16;
        self.st.bytes_left = 16 - r;
        self.st.u8 = 0;
        self.st.u8_ = 0;
        self.get16_at(0, r);
        self.slot().state = get16_case;
        self.st.mode = mode::VAL16;
        false
    }

    /// The bytes `at..at + bytes` (of 0..16) of a 16-byte little-endian value into `u8` (the low
    /// 8) and `u8_` (the high 8).
    #[inline]
    fn get16_at(&mut self, at: i32, bytes: i32) {
        let lo = (8 - at).clamp(0, bytes);
        if 0 < lo {
            self.st.u8 |= self.get8(lo as usize) << (at * 8);
        }
        let hi = bytes - lo;
        if 0 < hi {
            self.st.u8_ |= self.get8(hi as usize) << ((at + lo - 8) * 8);
        }
    }

    /// `bytes` (0..=8) little-endian.
    #[inline(always)]
    pub fn get8(&mut self, bytes: usize) -> u64 {
        let at = self.byte_;
        self.byte_ = at + bytes;
        if at + 8 <= self.buffer.len() {
            // one load of the whole word: the bytes past `bytes` are cut off
            let w = u64::from_le_bytes(self.buffer[at..at + 8].try_into().unwrap());
            // the mask of `bytes` (0..=8) bytes with no branch: the shift of 8 bytes wraps to 0, the
            // second half makes it all ones
            return w & ((1u64 << (bytes * 8 & 63)).wrapping_sub(1) | ((bytes >> 3) as u64).wrapping_neg());
        }
        let mut b = [0u8; 8];
        b[..bytes].copy_from_slice(&self.buffer[at..at + bytes]);
        u64::from_le_bytes(b)
    }

    // 4

    /// `bytes_left` bytes into `u4` (`try_get4(uint)`).
    #[inline(always)]
    pub fn try_get4_left(&mut self, next_case: u32) -> bool {
        let b = self.st.bytes_left;
        self.try_get4(b, next_case)
    }

    #[inline(always)]
    pub fn try_get4(&mut self, bytes: i32, next_case: u32) -> bool {
        if self.remaining() < bytes as usize {
            return self.retry_get4(bytes, next_case);
        }
        self.st.u4 = self.get4(bytes as usize);
        true
    }

    #[cold]
    #[inline(never)]
    pub fn retry_get4(&mut self, bytes: i32, get4_case: u32) -> bool {
        let r = self.remaining();
        self.st.bytes_max = bytes;
        self.st.bytes_left = bytes - r as i32;
        self.st.u4 = self.get4(r);
        self.slot().state = get4_case;
        self.st.mode = mode::VAL4;
        false
    }

    /// `bytes` (0..=4) little-endian.
    #[inline(always)]
    pub fn get4(&mut self, bytes: usize) -> u32 {
        self.get8(bytes) as u32
    }

    /// Completes the value parked in `u4`: true — the window ran out before it (`not_get4`).
    fn not_get4(&mut self) -> bool {
        let r = self.remaining().min(i32::MAX as usize) as i32; // a window past 2 GB: more than any value needs
        let shift = (self.st.bytes_max - self.st.bytes_left) * 8;
        if r < self.st.bytes_left {
            self.st.u4 |= self.get4(r as usize) << shift;
            self.st.bytes_left -= r;
            return true;
        }
        let left = self.st.bytes_left;
        self.st.u4 |= self.get4(left as usize) << shift;
        false
    }

    // bits

    #[inline(always)]
    pub fn init_bits(&mut self) {
        self.st.bits = 0;
        self.st.bit = 8;
    }

    /// `len_bits` (1..=8) bits: the window holds them (`get_bits<T>(int)`).
    #[inline(always)]
    pub fn get_bits(&mut self, len_bits: i32) -> u32 {
        let st = &mut *self.st;
        if st.bit + len_bits < 9 {
            let ret = st.bits >> st.bit & 0xFF >> (8 - len_bits);
            st.bit += len_bits;
            ret
        } else {
            let b = self.buffer[self.byte_] as u32;
            self.byte_ += 1;
            let ret = (st.bits >> st.bit | b << (8 - st.bit)) & 0xFF >> (8 - len_bits);
            st.bits = b;
            st.bit = st.bit + len_bits - 8;
            ret
        }
    }

    /// The bit stream holds `len_bits` more bits (`has_bits`).
    #[inline(always)]
    pub fn has_bits(&self, len_bits: i32) -> bool {
        len_bits as i64 <= (8 - self.st.bit) as i64 + (((self.byte_max - self.byte_) as i64) << 3) // a window past 256 MB: its bits do not fit an i32
    }

    /// `len_bits` bits into `u4`, or the retry at `this_case` (`try_get_bits`).
    #[inline(always)]
    pub fn try_get_bits(&mut self, len_bits: i32, this_case: u32) -> bool {
        if self.st.bit + len_bits < 9 || self.byte_ < self.byte_max {
            self.st.u4 = self.get_bits(len_bits);
            return true;
        }
        self.retry_at(this_case);
        false
    }

    #[inline(always)]
    pub fn try_get_varint_bits1(&mut self, bits: i32, this_case: u32) -> bool {
        if !self.try_get_bits(bits, this_case) {
            return false;
        }
        self.st.bytes_left = self.st.u4 as i32 + 1;
        self.st.bytes_max = self.st.bytes_left;
        true
    }

    #[inline(always)]
    pub fn try_get_varint_bits(&mut self, bits: i32, this_case: u32) -> bool {
        if !self.try_get_bits(bits, this_case) {
            return false;
        }
        self.st.bytes_left = self.st.u4 as i32;
        self.st.bytes_max = self.st.bytes_left;
        true
    }

    /// Items of a bit array straight into its list, while the stream is on a byte boundary: whole
    /// groups of 8.
    pub fn get_bits_list<T>(&mut self, dst: &mut crate::lib::collections::bits_list::BitsList<T>, item: i32, items: i32) -> i32 {
        if self.st.bit != 8 {
            return 0;
        }
        let bl = dst.bits_per_item() as i32;
        let gl = ((items >> 3) as i64).min((self.remaining() / bl as usize) as i64) as i32;
        if gl == 0 {
            return 0;
        }
        let n = (gl * bl) as usize;
        #[cfg(target_endian = "little")]
        dst.bytes_mut(item as usize, (gl << 3) as usize).copy_from_slice(&self.buffer[self.byte_..self.byte_ + n]);
        #[cfg(not(target_endian = "little"))]
        for g in 0..gl {
            let v = crate::lib::adhoc::helpers::read_uint_le(self.buffer, self.byte_ + (g * bl) as usize, bl as usize);
            dst.set_raw_items((item + (g << 3)) as usize, 8, v);
        }
        self.byte_ += n;
        gl << 3
    }

    /// One-bit items appended to their list, every whole byte the buffer has at once, while the
    /// stream is on a byte boundary.
    pub fn get_bits_bitlist(&mut self, dst: &mut crate::lib::collections::bit_list::BitList, _item: i32, items: i32) -> i32 {
        if self.st.bit != 8 || items < 8 {
            return 0;
        }
        let g = ((items >> 3) as usize).min(self.remaining());
        dst.push_bytes(&self.buffer[self.byte_..self.byte_ + g]);
        self.byte_ += g;
        (g << 3) as i32
    }

    /// Long arrays of bit items into a slice, whole groups of 8 through `value`, while the stream is
    /// on a byte boundary: one kernel over all of them. A short array goes by `bits8`.
    #[inline]
    pub fn get_bits_items<V>(&mut self, dst: &mut [V], item: i32, items: i32, bits_per_item: i32, value: impl Fn(u8) -> V) -> i32 {
        #[cfg(target_endian = "little")]
        {
            if items < crate::lib::collections::bits_list::KERNEL_ITEMS || self.st.bit != 8 {
                return 0;
            }
            // the kernel loads 8 bytes a group: those in the last 8 bytes of the window go by `bits8`
            let g = ((items >> 3) as usize).min(self.remaining().saturating_sub(8) / bits_per_item as usize);
            if g == 0 {
                return 0;
            }
            // SAFETY: `g * bits_per_item + 8` bytes from `byte_` are inside `byte_max`
            unsafe { crate::lib::collections::bits_list::unpack_items(bits_per_item as usize, self.buffer.as_ptr().add(self.byte_), &mut dst[item as usize..item as usize + (g << 3)], &value) };
            self.byte_ += g * bits_per_item as usize;
            (g << 3) as i32
        }
        #[cfg(not(target_endian = "little"))]
        0
    }

    /// A chunk of bit items into `u8`, item 0 lowest, while the stream is on a byte boundary
    /// (`bits8`).
    #[inline(always)]
    pub fn bits8(&mut self, items: i32, bits_per_item: i32) -> i32 {
        if self.st.bit != 8 {
            return 0;
        }
        let mut n = (items.min(64 / bits_per_item) as i64).min(((self.remaining() as i64) << 3) / bits_per_item as i64) as i32;
        n -= n % (8 >> bits_per_item.trailing_zeros());
        if n <= 0 {
            return 0;
        }
        let bytes = (n * bits_per_item >> 3) as usize;
        self.st.u8 = self.get8(bytes);
        n
    }

    // file

    // varint

    /// A varint into `u8` (`try_get_varint`). Whole in the window: one write of the state, `u8` -
    /// the zeroed `u8` and `bytes_left` are for a varint cut by the window only.
    pub fn try_get_varint(&mut self, next_case: u32) -> bool {
        if let Some(v) = self.varint_whole() {
            self.st.u8 = v;
            return true;
        }
        self.st.u8 = 0;
        self.st.bytes_left = 0;
        if self.varint_cut() {
            return true;
        }
        self.slot().state = next_case;
        self.st.mode = mode::VARINT;
        false
    }

    /// A varint the window holds whole, from its start: the value, `byte_` past it; `None` - it is
    /// not whole here.
    #[inline(always)]
    fn varint_whole(&mut self) -> Option<u64> {
        let p = self.byte_;
        if p < self.byte_max && self.buffer[p] < 0x80 {
            self.byte_ = p + 1; // the common case: one byte
            return Some(self.buffer[p] as u64);
        }
        if p + 1 < self.byte_max && self.buffer[p + 1] < 0x80 {
            self.byte_ = p + 2; // two bytes: up to 16 383
            return Some((self.buffer[p] & 0x7F) as u64 | (self.buffer[p + 1] as u64) << 7);
        }
        if p + 8 <= self.byte_max {
            // up to 8 bytes at once: the stop byte found in the word, the 7-bit groups compacted
            // without a loop
            let w = u64::from_le_bytes(self.buffer[p..p + 8].try_into().unwrap());
            let stop = !w & 0x8080_8080_8080_8080;
            if stop != 0 {
                let n = (stop.trailing_zeros() as usize >> 3) + 1;
                let mut x = if n == 8 { w } else { w & ((1u64 << (n * 8)) - 1) } & 0x7F7F_7F7F_7F7F_7F7F;
                x = (x & 0x007F_007F_007F_007F) | ((x & 0x7F00_7F00_7F00_7F00) >> 1);
                x = (x & 0x0000_3FFF_0000_3FFF) | ((x & 0x3FFF_0000_3FFF_0000) >> 2);
                x = (x & 0x0FFF_FFFF) | ((x & 0x0FFF_FFFF_0000_0000) >> 4);
                self.byte_ = p + n;
                return Some(x);
            }
        }
        None
    }

    /// Goes on with the varint in `u8`, its shift in `bytes_left`: true — it is whole.
    pub fn varint(&mut self) -> bool {
        if self.st.bytes_left == 0 {
            if let Some(v) = self.varint_whole() {
                self.st.u8 |= v;
                return true;
            }
        }
        self.varint_cut()
    }

    /// [`Self::varint`] byte by byte: a varint the window cuts, or ends.
    fn varint_cut(&mut self) -> bool {
        while self.byte_ < self.byte_max {
            let b = self.buffer[self.byte_] as u64;
            self.byte_ += 1;
            // past 64 bits a malformed varint adds nothing, and does not panic (`helpers::varint_decode`)
            if b < 0x80 {
                if self.st.bytes_left < 64 {
                    self.st.u8 |= b << self.st.bytes_left;
                }
                return true;
            }
            if self.st.bytes_left < 64 {
                self.st.u8 |= (b & 0x7F) << self.st.bytes_left;
                self.st.bytes_left += 7;
            }
        }
        false
    }

    // dims

    pub fn init_dims(&mut self, size: usize) {
        self.st.u8 = 1;
        if self.st.dims.len() < size {
            self.st.dims.resize(size, 0);
        }
    }

    /// A dimension from `u4`, checked against `max`. Over it the peer broke the protocol: it is
    /// told (OVERFLOW), the stream is broken - `false` - and the connection goes; reading on would
    /// let the peer size the array.
    pub fn dim_read(&mut self, max: i32, index: usize) -> bool {
        let dim = self.st.u4 as i32;
        if max < dim {
            crate::lib::adhoc::connection::receiver::on_error::rx_at(crate::lib::adhoc::connection::receiver::on_error::RxOnError::Overflow, &*self.st, &format_args!("In dim(int max, int index){{}} max < dim : {max} < {dim}"), None);
            self.st.error = Some(RxError::Overflow { got: dim as u32 as u64, max: max as u64 });
            return false;
        }
        self.st.u8 = self.st.u8.wrapping_mul(dim as u64);
        self.st.dims[index] = dim;
        true
    }

    /// A length from `u4`, checked against `max`. Over it the peer broke the protocol: it is told
    /// (OVERFLOW), the stream is broken and the connection goes - the pack is not delivered; the
    /// length is 0 for the rest of this call.
    pub fn length(&mut self, max: i64) -> i64 {
        let len = self.st.u4 as i32 as i64;
        if len <= max {
            return len;
        }
        crate::lib::adhoc::connection::receiver::on_error::rx_at(crate::lib::adhoc::connection::receiver::on_error::RxOnError::Overflow, &*self.st, &format_args!("In length(long max){{}} max < len : {max} < {len}"), None);
        self.st.error = Some(RxError::Overflow { got: len as u64, max: max as u64 });
        self.st.u8 = 0;
        0
    }

    // lengths

    #[inline(always)]
    pub fn get_len0(&mut self, max_items: i32, bytes: i32, next_case: u32) -> bool {
        self.get_len(0, max_items, bytes, next_case)
    }
    #[inline(always)]
    pub fn get_len1(&mut self, max_items: i32, bytes: i32, next_case: u32) -> bool {
        self.get_len(1, max_items, bytes, next_case)
    }
    #[inline(always)]
    pub fn get_len2(&mut self, max_items: i32, bytes: i32, next_case: u32) -> bool {
        self.get_len(2, max_items, bytes, next_case)
    }

    #[inline(always)]
    fn get_len(&mut self, which: u8, max_items: i32, bytes: i32, next_case: u32) -> bool {
        if bytes == 0 {
            let s = self.slot();
            match which {
                0 => s.index_max_0(0),
                1 => s.index_max_1(0),
                _ => s.index_max_2(0),
            };
            return true;
        }
        self.slot().max_items = max_items;
        if self.remaining() < bytes as usize {
            self.retry_get4(bytes, next_case);
            self.st.mode = mode::LEN0 + which;
            return false;
        }
        let len = self.get4(bytes as usize) as i32;
        let ok = match which {
            0 => self.slot().check_len0(len),
            1 => self.slot().check_len1(len),
            _ => self.slot().check_len2(len),
        };
        if let Err(e) = ok {
            self.st.error = Some(e);
            return false;
        }
        true
    }

    /// The window holds nothing more: the item at `index` is waited for at `no_index_case`
    /// (`no_index0`).
    #[inline(always)]
    pub fn no_index(&mut self, which: u8, no_index_case: u32, no_index_index: i32) -> bool {
        if 0 < self.remaining() {
            return false;
        }
        self.retry_at(no_index_case);
        let s = self.slot();
        match which {
            0 => s.index0 = no_index_index,
            1 => s.index1 = no_index_index,
            2 => s.index2 = no_index_index,
            _ => s.index3 = no_index_index,
        }
        true
    }

    /// The next byte of nulls: false — all null; else the index of the first non-null
    /// (`find_exist0`).
    #[inline(always)]
    pub fn find_exist(&mut self, which: u8, index: i32) -> bool {
        let nulls = self.get_byte() as u32;
        if nulls == 0 {
            return false;
        }
        let at = index + trailing8zeros(nulls);
        let s = self.slot();
        let max = match which {
            0 => s.index_max0,
            1 => s.index_max1,
            2 => s.index_max2,
            _ => s.index_max3,
        };
        match which {
            0 => s.nulls_0(nulls, index),
            1 => s.nulls_1(nulls, index),
            2 => s.nulls_2(nulls, index),
            _ => s.nulls_3(nulls, index),
        }
        if max <= at {
            self.st.error = Some(RxError::Overflow { got: at as u64, max: max as u64 });
        }
        true
    }

    // bytes

    /// The items of an array of numbers, little-endian: one copy on a little-endian machine
    /// (`get_short` … in a loop). The window holds them: `size_of_val(dst) <= remaining()`.
    #[inline]
    pub fn get_le<T: crate::lib::adhoc::connection::transmitter::Le>(&mut self, dst: &mut [T]) {
        let n = std::mem::size_of_val(dst);
        let src = &self.buffer[self.byte_..self.byte_ + n];
        #[cfg(target_endian = "little")]
        // SAFETY: `Le` - every bit pattern is a value; `src` is `n` bytes, apart from `dst`.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr() as *mut u8, n)
        };
        #[cfg(not(target_endian = "little"))]
        for (v, s) in dst.iter_mut().zip(src.chunks_exact(std::mem::size_of::<T>())) {
            *v = T::from_le_slice(s);
        }
        self.byte_ += n;
    }

    /// Up to `dst.len()` bytes into `dst`: as many as the window holds; fewer — the retry at
    /// `retry_case`.
    pub fn get_bytes(&mut self, dst: &mut [u8], retry_case: u32) -> usize {
        let mut n = dst.len();
        if self.remaining() < n {
            n = self.remaining();
            self.retry_at(retry_case);
        }
        dst[..n].copy_from_slice(&self.buffer[self.byte_..self.byte_ + n]);
        self.byte_ += n;
        n
    }

    /// The bytes of the window, up to `bytes`: the caller takes them in place, then closes with
    /// `window_done`.
    #[inline]
    pub fn window(&self, bytes: usize) -> &'a [u8] {
        let n = bytes.min(self.remaining());
        &self.buffer[self.byte_..self.byte_ + n]
    }

    /// `taken` bytes of the window are taken; fewer than `asked` — the retry at `retry_case`.
    #[inline]
    pub fn window_done(&mut self, taken: usize, asked: usize, retry_case: u32) -> usize {
        self.byte_ += taken;
        if taken < asked {
            self.retry_at(retry_case);
        }
        taken
    }

    // string

}

impl RxSlot {
    pub fn check_len0(&mut self, len: i32) -> Result<(), RxError> {
        if self.max_items < self.index_max_0(len) {
            return Err(RxError::Overflow { got: len as u64, max: self.max_items as u64 });
        }
        Ok(())
    }
    pub fn check_len1(&mut self, len: i32) -> Result<(), RxError> {
        if self.max_items < self.index_max_1(len) {
            return Err(RxError::Overflow { got: len as u64, max: self.max_items as u64 });
        }
        Ok(())
    }
    pub fn check_len2(&mut self, len: i32) -> Result<(), RxError> {
        if self.max_items < self.index_max_2(len) {
            return Err(RxError::Overflow { got: len as u64, max: self.max_items as u64 });
        }
        Ok(())
    }
}

/// A one-shot receiver of one pack (`Receiver.Virtual`): it parses the pack set by
/// [`set_bytes_dst`](Self::set_bytes_dst) from the pieces [`write`](Self::write) is given, one
/// after another, of any size - from a file, a byte array, a test - with no connection.
pub struct Virtual<D: BytesDst> {
    st: RxState,
    one: One<D>,
}

/// The pack of a [`Virtual`], the id it is to come with, and whether it is whole.
struct One<D> {
    dst: Option<D>,
    id: u32,
    whole: bool,
}

impl<D: BytesDst> RxHost for One<D> {
    #[inline]
    fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
        self.dst.is_some() && !self.whole && id == self.id
    }
    #[inline]
    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        self.dst.as_mut().is_none_or(|d| d.put_bytes(src))
    }
    #[inline]
    fn on_received(&mut self, _st: &mut RxState) {
        self.whole = true;
    }
}

impl<D: BytesDst> Virtual<D> {
    /// `id_bytes` - the bytes of a pack id of the protocol.
    pub fn new(id_bytes: i32) -> Self {
        Self { st: RxState::new(id_bytes), one: One { dst: None, id: 0, whole: false } }
    }

    /// The pack to fill, and its id: before [`write`](Self::write). A pack in progress is dropped.
    pub fn set_bytes_dst(&mut self, dst: D, id: u32) {
        self.st.abandon();
        self.st.error = None;
        self.one = One { dst: Some(dst), id, whole: false };
    }

    /// Parses `src` from where the previous piece stopped. Returns the bytes taken - behind the end
    /// of the pack none is; negative - the bytes are not of the pack set (another id, a value out
    /// of its limits): [`error`](Self::error) says why.
    pub fn write(&mut self, src: &[u8]) -> isize {
        if self.one.whole {
            return 0;
        }
        let (n, done) = self.st.write(src, &mut self.one);
        if done {
            self.one.on_received(&mut self.st);
        }
        n
    }

    /// Why [`write`](Self::write) gave up.
    #[inline]
    pub fn error(&self) -> Option<&RxError> {
        self.st.error.as_ref()
    }

    /// The pack, once it is whole.
    pub fn take(&mut self) -> Option<D> {
        if !self.one.whole {
            return None;
        }
        self.one.whole = false;
        self.one.dst.take()
    }
}

/// The bytes of the network straight into the receiver `H` of a connection, with no framing: a
/// stream transport (TCP, a WebSocket) keeps the bytes whole and in order, the packs delimit
/// themselves. The bytes are parsed where they lie.
pub struct Receiver<H: RxHost> {
    host: std::mem::ManuallyDrop<H>,
    rx: RxState,
    /// Requests stopping immediately after the current packet so [`crate::lib::network::reseat`]
    /// can transfer trailing bytes to another session.
    stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// A new socket of the session, and a guaranteed pack in progress: the packs the two sides
    /// agree by come first ([`crate::lib::adhoc::connection::control`]), read by a parse of their
    /// own until the answer to the position named.
    greeting: bool,
    /// That parse: the pack in progress waits in `rx` meanwhile.
    greet_rx: Option<RxState>,
    /// The bytes taken and the clock when the speed was measured last ([`RxHost::speed`]).
    speed_mark: i64,
    speed_at: i64,
    /// The host and the parse went to the session with the guaranteed pack in progress
    /// ([`BytesDst::suspend`]): the host is not this receiver's to drop.
    ///
    /// [`BytesDst::suspend`]: crate::lib::adhoc::BytesDst::suspend
    hollow: bool,
}

/// The host goes with the receiver, unless the session took it (`hollow`).
impl<H: RxHost> Drop for Receiver<H> {
    fn drop(&mut self) {
        if !self.hollow {
            // SAFETY: the host is there - nothing took it
            unsafe { std::mem::ManuallyDrop::drop(&mut self.host) }
        }
    }
}

impl<H: RxHost + Send> Receiver<H> {
    /// `id_bytes` — the bytes of a pack id with the address of its actor (`Receiver::ID_BYTES` of
    /// the generated code). A session whose socket was lost in the middle of a guaranteed pack goes
    /// on with the parse it kept: the host and the state of the receiver of that socket, the packs
    /// of the agreement first.
    pub fn new(host: H, id_bytes: i32) -> Self {
        let mut r = Self { host: std::mem::ManuallyDrop::new(host), rx: RxState::new(id_bytes), stop: None, greeting: false, greet_rx: None, speed_mark: 0, speed_at: crate::lib::network::engine::now_nanos(), hollow: false };
        let kept = positioned(&*r.host, |p| p.take_parked::<(H, RxState)>()).flatten();
        if let Some((host, rx)) = kept {
            // SAFETY: the fresh host is replaced right away, never used again
            unsafe { std::mem::ManuallyDrop::drop(&mut r.host) };
            r.host = std::mem::ManuallyDrop::new(host);
            r.rx = rx;
            r.speed_mark = r.rx.consumed();
            r.greeting = true;
            r.greet_rx = Some(RxState::new(id_bytes));
        }
        r
    }
}

impl<H: RxHost> Receiver<H> {

    /// The keys of the session: a ChaCha20 stage of a pack chain finds the key a pack names there.
    pub fn keys(mut self, keys: std::sync::Arc<crate::lib::adhoc::connection::keyring::Keyring>) -> Self {
        self.rx.keys = keys;
        self
    }

    /// The keys of the Envelope stages of the connection ( [`crate::lib::chain::Envelope`] ).
    pub fn envelope(mut self, envelope: std::sync::Arc<crate::lib::chain::Envelope>) -> Self {
        self.rx.envelope = Some(envelope);
        self
    }

    /// The flag a session that moves onto the transport sets: the parse stops behind the pack it
    /// came in (`Reseat::stop`).
    pub fn stop(mut self, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.stop = Some(stop);
        self
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    /// Between packs: no pack, nor a pack id, is received in part.
    pub fn idle(&self) -> bool {
        self.rx.idle()
    }

    /// The speed the bytes are taken at, into the field the host names ([`RxHost::speed`]): the
    /// bytes taken since the last measure, over the time since then - measured once 10 ms at least
    /// went by, a pause longer than a second is not counted. Not averaged: the speed now. The clock
    /// of the loop pass.
    #[inline]
    fn measure(&mut self) {
        let Some(speed) = self.host.speed() else { return };
        let now = crate::lib::network::engine::now_nanos(); // the clock of the loop pass: no reading a receive call
        let t = now - self.speed_at;
        if t < 10_000_000 {
            return;
        }
        let consumed = self.rx.consumed();
        if t < 1_000_000_000 {
            // what came since the last measure, over the time it took - not a pause
            speed.store(((consumed - self.speed_mark) as i128 * 1_000_000_000 / t as i128) as i64, std::sync::atomic::Ordering::Relaxed);
        }
        self.speed_at = now;
        self.speed_mark = consumed;
    }

    /// The pack in flight is dropped: the next bytes start a pack, and the host ends what it fed
    /// with this one.
    pub fn abandon(&mut self) {
        dropped(&mut self.rx, &mut *self.host);
    }

    /// The packs of the agreement on a new socket, ahead of the guaranteed pack in progress: how
    /// many bytes of `src` they took. They are parsed by the generated code with a parse of their
    /// own ([`Greeter`]): the pack in progress waits in `rx`, its position stays. The answer to the
    /// position named ends them. `None` — not a pack of the agreement: the stream is not the one
    /// agreed on, the pack is dropped and the link with it.
    fn greet(&mut self, src: &[u8]) -> Option<usize> {
        let mut rx = self.greet_rx.take().unwrap_or_else(|| RxState::new(self.rx.id_bytes));
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut g = Greeter { host: &mut *self.host, heard: crate::lib::adhoc::connection::control::Heard::Nothing, stop: &stop };
        let n = rx.write_all_until(src, &mut g, Some(&stop));
        let heard = g.heard;
        self.greet_rx = Some(rx);
        if n < 0 {
            let at = positioned(&*self.host, |p| p.named()).unwrap_or(0);
            self.lose(at);
            return None;
        }
        match heard {
            crate::lib::adhoc::connection::control::Heard::Lost(at) => self.lose(at),
            crate::lib::adhoc::connection::control::Heard::Ok(at) => {
                self.greeting = false;
                if positioned(&*self.host, |p| p.named() != at).unwrap_or(false) {
                    self.lose(at);
                } else {
                    positioned(&*self.host, |p| p.counted_from(at)); // the steps are counted from the position agreed on
                }
            }
            crate::lib::adhoc::connection::control::Heard::Nothing => {}
        }
        Some(n as usize)
    }

    /// The journal of the peer does not hold the bytes of the guaranteed pack in progress: it is
    /// dropped - told to the handler of the transport -, the peer's stream goes on from `at`, the
    /// session hears it.
    fn lose(&mut self, at: i64) {
        self.greeting = false;
        if let Some(id) = positioned(&*self.host, |p| p.id()) {
            let e = std::io::Error::other(format!("The guaranteed pack {id} was cut by the loss of a connection and is dropped: the journal of the peer does not hold its bytes, its stream goes on from {at}"));
            self.host.failure(&e);
        }
        dropped(&mut self.rx, &mut *self.host);
        positioned(&*self.host, |p| p.position(at));
        self.host.on_lost();
    }
}

/// The position of the receiver `host` serves ( [`RxHost::position`] ), lent to `f`: `None` - the connection counts none.
#[inline(always)]
pub(crate) fn positioned<H: RxHost + ?Sized, R>(host: &H, f: impl FnOnce(&mut Position) -> R) -> Option<R> {
    // SAFETY: the receiver that serves `host` runs on the thread of its connection, or before the session carries anything -
    // the one place its position is touched; nothing of the receive lends it again inside `f`
    host.position().map(|p| unsafe { p.lend(f) })
}

/// The host of the parse of the packs of the agreement on a new socket ([`Receiver::greet`]): the
/// packs of the agreement go to the host of the connection, any other is refused - the stream is
/// not the one agreed on. The answer to the position named stops the parse right behind it: what
/// follows is the pack in progress.
struct Greeter<'a, H: RxHost> {
    host: &'a mut H,
    heard: crate::lib::adhoc::connection::control::Heard,
    stop: &'a std::sync::atomic::AtomicBool,
}

impl<H: RxHost> RxHost for Greeter<'_, H> {
    fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool {
        self.host.service(id) && self.host.on_receiving(src, id)
    }

    fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
        self.host.put_bytes(src)
    }

    fn on_received(&mut self, _st: &mut RxState) {
        let h = self.host.on_service();
        if h != crate::lib::adhoc::connection::control::Heard::Nothing {
            self.heard = h;
            self.stop.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

/// The pack in flight is dropped (a framing error, an error of the receive, the stream jumped):
/// what the host fed with it ends too, and a guaranteed one is not received - the position stays at
/// its start, the peer sends it again from there.
pub(crate) fn dropped<H: RxHost>(rx: &mut RxState, host: &mut H) {
    rx.abandon();
    host.on_abandon();
    positioned(&*host, |p| p.drop_pack());
}

impl<H: RxHost + Send> crate::lib::adhoc::BytesDst for Receiver<H> {
    /// The socket of a stream under the session is gone, and the session parks: the guaranteed pack
    /// in progress goes to the session with the host and the state of this receiver - the receiver
    /// of the next socket goes on with it from the byte it was cut at, or drops it when the peer's
    /// journal does not hold it any more. Anything else in progress will never be completed:
    /// dropped now.
    fn suspend(&mut self) {
        if self.hollow {
            return;
        }
        let Some(pm) = self.host.position().map(|p| p as *const crate::lib::adhoc::utils::OwnCell<Position>) else {
            Receiver::abandon(self);
            return;
        };
        // SAFETY: the position lives in the connection the host holds: the host moves, the
        // connection stays where it is
        let pm = unsafe { &*pm };
        // SAFETY: the receiver suspends on the thread of its connection
        if !unsafe { pm.lend(|p| p.in_pack()) } {
            Receiver::abandon(self);
            return;
        }
        self.greeting = false;
        self.greet_rx = None;
        let consumed = self.rx.consumed();
        let id_bytes = self.rx.id_bytes;
        let rx = std::mem::replace(&mut self.rx, RxState::new(id_bytes));
        self.hollow = true;
        // SAFETY: `hollow` - the host is not dropped here, nor used again
        let host = unsafe { std::mem::ManuallyDrop::take(&mut self.host) };
        unsafe { pm.lend(|p| p.park(consumed, (host, rx))) }; // SAFETY: as above
    }

    /// The stream goes on from `position` of the peer's journal, past bytes never received: the
    /// pack in flight is dropped, a pack starts at `position`, and the session hears it.
    fn lost(&mut self, position: i64) {
        Receiver::abandon(self);
        positioned(&*self.host, |p| p.position(position));
        self.host.on_lost();
    }

    fn abandon(&mut self) {
        Receiver::abandon(self);
    }

    /// The bytes that follow come from the peer's journal, from the position this receiver named: a
    /// guaranteed pack in progress goes on; one of no guarantee, or a pack id read in part, is
    /// dropped.
    fn continue_(&mut self) {
        if !positioned(&*self.host, |p| p.in_pack()).unwrap_or(false) {
            Receiver::abandon(self);
        }
    }

    /// The position in the peer's journal, the guaranteed pack in progress counted.
    #[inline]
    fn received(&self) -> i64 {
        let consumed = self.rx.consumed();
        positioned(&*self.host, |p| p.received_at(consumed)).unwrap_or(-1)
    }

    /// Between packs, and no guaranteed pack in progress the session keeps to go on with.
    #[inline]
    fn idle(&self) -> bool {
        self.rx.idle() && !positioned(&*self.host, |p| p.in_pack()).unwrap_or(false)
    }

    fn broken(&self) -> bool {
        framing::broken(&self.rx)
    }

    /// The bytes are parsed where the transport read them: what a parked relay does not take stays
    /// there, and a relay may take the storage over.
    fn on_block(&mut self, block: &crate::lib::network::buf::RxBuf, at: usize, end: usize) -> Option<usize> {
        let mut from = at;
        if self.greeting {
            // SAFETY: `[at, end)` are the connection's own bytes
            from += self.greet(unsafe { block.slice(at, end) })?;
            if self.greeting || from == end {
                return Some(from - at);
            }
        }
        let n = self.rx.write_all_block(block, from, end, &mut *self.host, self.stop.as_deref());
        self.measure();
        if n < 0 { None } else { Some(from - at + n as usize) }
    }

    /// The bytes are parsed where they lie: what a parked relay does not take stays with whoever
    /// offered them.
    fn on_part(&mut self, bytes: &[u8]) -> Option<usize> {
        let mut from = 0;
        if self.greeting {
            from = self.greet(bytes)?;
            if self.greeting || from == bytes.len() {
                return Some(from);
            }
        }
        let n = self.rx.write_all_until(&bytes[from..], &mut *self.host, self.stop.as_deref());
        self.measure();
        if n < 0 { None } else { Some(from + n as usize) }
    }

    /// All of them, or false: a caller that cannot keep what is refused.
    fn on_bytes(&mut self, bytes: &[u8]) -> bool {
        self.on_part(bytes) == Some(bytes.len())
    }

    /// The bytes of a record in the clear: frames of tunnels with a Noise of their own only
    /// ([`RxHost::clear`]), whole - anything else breaks the stream. The packs of the agreement are
    /// sealed.
    fn on_clear(&mut self, bytes: &[u8]) -> Option<usize> {
        if self.greeting {
            return None;
        }
        self.rx.clear = true;
        let n = self.rx.write_all_until(bytes, &mut *self.host, self.stop.as_deref());
        self.rx.clear = false;
        self.measure();
        if n < 0 { None } else { Some(n as usize) }
    }
}

pub use framing::Framing;

#[cfg(test)]
mod tests {
    /// A pack body that panics breaks the stream as a parse error does: the pack in flight is
    /// dropped, the parser waits for a pack id, and nothing more is taken until the stream is set
    /// up again.
    #[test]
    fn a_panicking_pack_body_breaks_the_stream() {
        struct Panics;
        impl super::BytesDst for Panics {
            fn put_bytes(&mut self, _src: &mut super::Rx<'_>) -> bool {
                panic!("a pack body with a bug");
            }
        }
        let mut v = super::Virtual::new(1);
        v.set_bytes_dst(Panics, 5);
        assert_eq!(v.write(&[5, 1, 2, 3]), -1);
        assert!(matches!(v.error(), Some(super::RxError::Broken(_))));
        assert!(v.st.idle(), "the pack in flight is dropped");
        assert_eq!(v.write(&[5, 1]), -1, "broken: nothing more is taken");
    }

    use super::*;

    /// A signed byte into `u8` is the same whole or cut by the end of a window: sign-extended both
    /// ways - the generated code compares it as `i64` with the null value of its field.
    #[test]
    fn a_signed_byte_is_the_same_whole_or_cut() {
        let mut st = RxState::new(1);
        let mut rx = Rx::new(&[0xFF], &mut st);
        assert!(rx.get_sbyte_u8(5));
        let whole = rx.u8;
        let mut st = RxState::new(1);
        let mut rx = Rx::new(&[], &mut st);
        assert!(!rx.get_sbyte_u8(5), "the window has no byte: the read waits");
        let mut rx = Rx::new(&[0xFF], &mut st);
        assert_eq!(rx.resume(), 1);
        assert_eq!(rx.u8, whole, "cut and whole the same");
        assert_eq!(whole as i64, -1);
    }

    /// A window past 256 MB holds more bits than an i32 counts, one past 2 GB more bytes: the bits and the values are
    /// there all the same - no count cut to a negative one leaves the receive waiting for ever. The zeroed buffer is lazy:
    /// its pages are not touched.
    #[test]
    fn a_window_past_what_an_i32_counts_holds_its_bits_and_values() {
        for len in [(1usize << 28) + 16, (1usize << 31) + 16] {
            let buf = vec![0u8; len];
            let mut st = RxState::new(1);
            let mut rx = Rx::new(&buf, &mut st);
            assert!(rx.has_bits(8), "{len}: the bits are there");
            assert!(rx.try_get4(4, 0), "{len}: the value is there");
            assert!(rx.try_get8(8, 0), "{len}: the value is there");
            let mut st = RxState::new(1);
            st.bit = 8; // on a byte boundary, as the bits of a list begin
            let mut rx = Rx::new(&buf, &mut st);
            assert_eq!(rx.bits8(8, 1), 8, "{len}: eight one-bit items");
        }
    }

    /// A pack of one u32 and one string: what the generated code does, by hand.
    #[derive(Default, Debug, PartialEq)]
    struct P {
        a: u32,
        s: Option<String>,
        /// `a` stands for the headers of a connection: the body - the copy of the pack - begins
        /// behind it.
        headers: bool,
    }

    impl BytesDst for P {
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            loop {
                match src.state() {
                    0 => {
                        if !src.has_4bytes(1) {
                            return false;
                        }
                        self.a = src.get_uint();
                        if self.headers {
                            src.capture_body();
                        }
                        src.set_state(2);
                    }
                    1 => {
                        self.a = src.get_uint_();
                        if self.headers {
                            src.capture_body();
                        }
                        src.set_state(2);
                    }
                    2 => {
                        if !src.try_get_string(100, 3) {
                            return false;
                        }
                        src.set_state(3);
                    }
                    3 => {
                        self.s = src.get_string();
                        return true;
                    }
                    _ => return true,
                }
            }
        }
    }

    struct Host {
        p: P,
        got: Vec<(u32, Option<String>)>,
    }

    impl RxHost for Host {
        fn on_receiving(&mut self, _src: &mut Rx<'_>, id: u32) -> bool {
            id == 7
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            self.p.put_bytes(src)
        }
        fn on_received(&mut self, _st: &mut RxState) {
            self.got.push((self.p.a, self.p.s.take()));
        }
    }

    fn wire(a: u32, s: &str) -> Vec<u8> {
        let mut w = vec![7u8];
        w.extend_from_slice(&a.to_le_bytes());
        let units: Vec<u16> = s.encode_utf16().collect();
        let mut v = units.len() as u64;
        loop {
            if v < 0x80 {
                w.push(v as u8);
                break;
            }
            w.push(v as u8 | 0x80);
            v >>= 7;
        }
        for u in units {
            let mut v = u as u64;
            loop {
                if v < 0x80 {
                    w.push(v as u8);
                    break;
                }
                w.push(v as u8 | 0x80);
                v >>= 7;
            }
        }
        w
    }

    #[test]
    fn any_window_size() {
        let mut stream = wire(0xDEADBEEF, "hello, мир");
        stream.extend(wire(3, "x"));
        for window in 1..=stream.len() {
            let mut st = RxState::new(1);
            let mut host = Host { p: P::default(), got: vec![] };
            for chunk in stream.chunks(window) {
                assert!(st.write_all(chunk, &mut host) >= 0);
            }
            assert_eq!(host.got, vec![(0xDEADBEEF, Some("hello, мир".to_string())), (3, Some("x".to_string()))], "window {window}");
        }
    }

    /// What a copy got: the bytes of the pack in progress, the packs whole, the packs dropped.
    #[derive(Default)]
    struct Rec {
        cur: std::sync::Mutex<Vec<u8>>,
        whole: std::sync::Mutex<Vec<Vec<u8>>>,
        failed: std::sync::atomic::AtomicUsize,
    }

    impl BytesCopy for Rec {
        fn pass(&self, bytes: &[u8]) {
            self.cur.lock().unwrap().extend_from_slice(bytes);
        }
        fn last(&self, bytes: &[u8]) {
            let mut c = std::mem::take(&mut *self.cur.lock().unwrap());
            c.extend_from_slice(bytes);
            self.whole.lock().unwrap().push(c);
        }
        fn fail(&self) {
            self.cur.lock().unwrap().clear();
            self.failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The application's handler of the receiver, by hand as the generated host calls it: it sets
    /// the copy on every pack in `on_receiving`.
    struct Copying {
        p: P,
        rec: std::sync::Arc<Rec>,
        received: usize,
    }

    impl RxHost for Copying {
        fn on_receiving(&mut self, src: &mut Rx<'_>, id: u32) -> bool {
            if self.p.headers {
                src.defer_copy(); // as the generated code of a connection with headers
            }
            src.bytes_copy = Some(self.rec.clone());
            id == 7
        }
        fn put_bytes(&mut self, src: &mut Rx<'_>) -> bool {
            self.p.put_bytes(src)
        }
        fn on_received(&mut self, st: &mut RxState) {
            assert!(st.bytes_copy.is_none(), "taken away when the pack is whole");
            self.received += 1;
        }
    }

    /// The copy gets the bytes of each pack behind its id - behind the headers when the host defers
    /// it - however the windows cut them; a pack dropped half way is `fail`ed.
    #[test]
    fn a_copy_gets_the_pack_as_it_came() {
        let packs = [wire(0xDEADBEEF, "hello, мир"), wire(3, "x")];
        let stream = packs.concat();
        for headers in [false, true] {
            for window in 1..=stream.len() {
                let rec: std::sync::Arc<Rec> = Default::default();
                let mut st = RxState::new(1);
                let mut host = Copying { p: P { headers, ..P::default() }, rec: rec.clone(), received: 0 };
                for chunk in stream.chunks(window) {
                    assert!(st.write_all(chunk, &mut host) >= 0);
                }
                let skip = if headers { 5 } else { 1 };
                let want: Vec<Vec<u8>> = packs.iter().map(|p| p[skip..].to_vec()).collect();
                assert_eq!(host.received, 2);
                assert_eq!(*rec.whole.lock().unwrap(), want, "headers {headers}, window {window}");
            }
        }
        let rec: std::sync::Arc<Rec> = Default::default();
        let mut st = RxState::new(1);
        let mut host = Copying { p: P::default(), rec: rec.clone(), received: 0 };
        assert!(st.write_all(&stream[..8], &mut host) >= 0);
        assert_eq!(rec.cur.lock().unwrap().len(), 7, "passed");
        st.abandon();
        assert_eq!(rec.failed.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Packs of a 1-byte id and a 3-byte body; the pack 7 is a frame of a tunnel with a Noise of its
    /// own - only it may come in a record in the clear.
    #[derive(Default)]
    struct Frames {
        left: usize,
        id: u32,
        got: Vec<u32>,
    }

    impl super::RxHost for Frames {
        fn on_receiving(&mut self, _src: &mut super::Rx<'_>, id: u32) -> bool {
            self.id = id;
            self.left = 3;
            true
        }
        fn put_bytes(&mut self, src: &mut super::Rx<'_>) -> bool {
            let k = self.left.min(src.byte_max - src.byte_);
            src.byte_ += k;
            self.left -= k;
            self.left == 0
        }
        fn on_received(&mut self, _st: &mut super::RxState) {
            self.got.push(self.id);
        }
        fn clear(&self, id: u32) -> bool {
            id == 7
        }
    }

    /// Frames of a tunnel with a Noise of its own come in the clear between sealed packs; a pack
    /// of the connection there - a `login` put in the clear on the line - breaks the stream, as does
    /// a pack begun sealed and gone on in the clear. A frame begun in the clear goes on sealed - a
    /// replay of the journal - and the pack after it is the connection's own again.
    #[test]
    fn only_frames_of_noise_tunnels_come_in_the_clear() {
        use crate::lib::adhoc::BytesDst as _;
        let mut r = super::Receiver::new(Frames::default(), 1);
        assert_eq!(r.on_part(&[1, 1, 2, 3, 7]), Some(5), "sealed: anything");
        assert_eq!(r.on_part(&[1, 2, 3]), Some(3));
        assert_eq!(r.on_clear(&[7, 1, 2, 3, 7, 1]), Some(6), "in the clear: the frames");
        assert_eq!(r.on_clear(&[2, 3]), Some(2));
        assert_eq!(r.on_part(&[1, 0, 0, 0]), Some(4));
        assert_eq!(r.host().got, vec![1, 7, 7, 7, 1]);
        assert!(!crate::lib::adhoc::BytesDst::broken(&r));

        let mut r = super::Receiver::new(Frames::default(), 1);
        assert_eq!(r.on_clear(&[1, 1, 2, 3]), None, "a pack of the connection in the clear");
        assert!(crate::lib::adhoc::BytesDst::broken(&r), "forged: the connection ends, its session does not park");
        assert!(r.host().got.is_empty());

        let mut r = super::Receiver::new(Frames::default(), 1);
        assert_eq!(r.on_part(&[1, 1]), Some(2));
        assert_eq!(r.on_clear(&[2, 3]), None, "a sealed pack goes on in the clear");
        assert!(crate::lib::adhoc::BytesDst::broken(&r));

        let mut r = super::Receiver::new(Frames::default(), 1);
        assert_eq!(r.on_clear(&[7, 1]), Some(2));
        assert_eq!(r.on_part(&[2, 3, 1, 4, 5, 6]), Some(6), "a frame in the clear goes on sealed, then a pack of the connection");
        assert_eq!(r.host().got, vec![7, 1]);
        assert!(!crate::lib::adhoc::BytesDst::broken(&r));

        let mut r = super::Receiver::new(Frames::default(), 1);
        assert_eq!(r.on_clear(&[7, 1]), Some(2));
        assert_eq!(r.on_part(&[2]), Some(1));
        assert_eq!(r.on_clear(&[3, 1, 0, 0, 0]), None, "the frame goes on in the clear again, a pack of the connection behind it there");
        assert!(crate::lib::adhoc::BytesDst::broken(&r));
    }
}

pub mod bytes_copy;
pub mod chunked;
pub mod framing;
pub mod on_error;
pub mod tunnel;
mod position;
mod stream;
mod string;
pub use position::Position;
