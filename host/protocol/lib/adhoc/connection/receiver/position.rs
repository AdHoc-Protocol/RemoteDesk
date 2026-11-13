// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Receiver positions within the peer's guaranteed-byte journal.
//!
//! Tracks packet boundaries and committed bytes so a transport can acknowledge progress and resume
//! a guaranteed packet after interruption.

/// The position of the receiving side in the peer's journal: the bytes of the guaranteed packs
/// taken whole, and of the one in progress. It lives with the session, and so does the parse of a
/// guaranteed pack a lost socket cut: the receiver of the next socket takes it over and goes on
/// from the byte it was cut at.
pub struct Position {
    guarded: i64,
    /// Where the guaranteed pack in progress started in the stream of the receiver; -1 — none.
    from: i64,
    positioned: bool,
    told: i64,
    /// The bytes of the stream the receiver had taken at the end of its last receive call, or when
    /// its socket was lost: the bytes of the pack in progress it has ([`Position::named`]).
    seen: i64,
    /// The id of the guaranteed pack in progress: told when it is dropped half-way.
    id: u32,
    /// The parse of the guaranteed pack in progress the receiver of a lost socket left (its host
    /// and its state): the receiver of the next socket of the session takes it over
    /// ([`super::Receiver::new`]).
    parked: Option<Parked>,
    /// The header of the files of the session ([`crate::lib::adhoc::connection::control::Control`]): the position is stored
    /// into it as it moves - the next process goes on from it. Null: the session lives in memory.
    head: std::sync::atomic::AtomicPtr<u8>,
    /// The control of the session, once it gave the files: the position goes to the disk as it says
    /// ([`Control::durable`](crate::lib::adhoc::connection::control::Control::durable)). It lives in the connection, as the
    /// position does.
    control: std::sync::atomic::AtomicPtr<crate::lib::adhoc::connection::control::Control>,
}

/// A value of the type the receiver of a session parks, kept with no lifetime of its own: its type
/// is told by its name - the receivers of one session are of one type -, it is dropped with the
/// session when no receiver takes it.
pub(crate) struct Parked {
    ptr: *mut (),
    kind: &'static str,
    drop: unsafe fn(*mut ()),
}

// SAFETY: made of a value that is `Send` only ([`Position::park`])
unsafe impl Send for Parked {}

impl Drop for Parked {
    fn drop(&mut self) {
        // SAFETY: the box made by `park`, of the type `drop` was made for, not taken
        unsafe { (self.drop)(self.ptr) }
    }
}

unsafe fn drop_box<T>(p: *mut ()) {
    drop(Box::from_raw(p as *mut T));
}

impl Default for Position {
    fn default() -> Self {
        Self {
            guarded: 0,
            from: -1,
            positioned: false,
            told: i64::MAX,
            seen: 0,
            id: 0,
            parked: None,
            head: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            control: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        }
    }
}

impl Position {
    /// The session got its files, the header `h` mapped ( null: they went ): a session of the process before goes on from the
    /// position the header has - the generated `save` wrote an older one -, after a reboot ( `rebooted` ) from the one the disk
    /// has, when it was written through; a fresh one stores its own.
    pub(crate) fn attach(&mut self, h: *mut u8, restoring: bool, rebooted: bool, control: &crate::lib::adhoc::connection::control::Control) {
        use crate::lib::adhoc::connection::control::{get_i32, get_i64, set_i32, set_i64, Control};
        self.control.store(control as *const Control as *mut Control, std::sync::atomic::Ordering::Relaxed);
        self.head.store(h, std::sync::atomic::Ordering::Relaxed);
        if h.is_null() {
            return;
        }
        // SAFETY: the header just mapped
        unsafe {
            if !restoring {
                set_i64(h, Control::H_GUARDED, self.guarded);
                set_i32(h, Control::H_POSITIONED, self.positioned as i32);
            } else if rebooted && get_i32(h, Control::H_D_GVALID) != 0 {
                self.position(get_i64(h, Control::H_D_GUARDED)); // a reboot, and the position was written through ( resumable_sync_ms ): as the disk has it
            } else if get_i32(h, Control::H_POSITIONED) != 0 {
                self.position(get_i64(h, Control::H_GUARDED));
            }
        }
    }

    /// Records the start of guaranteed packet `id` at stream offset `at`. Use
    /// [`super::Rx::pack_at`] to obtain its offset before the identifier.
    #[inline]
    pub fn start(&mut self, at: i64, id: u32) {
        self.from = at;
        self.id = id;
    }

    /// The id of the guaranteed pack in progress.
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The position this receiver names to the peer (`CONTINUE`): behind the bytes it has, those of
    /// the guaranteed pack in progress counted; -1 — none is known.
    #[inline]
    pub fn named(&self) -> i64 {
        if !self.positioned {
            -1
        } else if self.from < 0 {
            self.guarded
        } else {
            self.guarded + self.seen - self.from
        }
    }

    /// The socket of the receiver is gone with `consumed` bytes of the stream taken, and the parse
    /// of the guaranteed pack in progress is kept for the next one (`parse`).
    pub(crate) fn park<T: Send>(&mut self, consumed: i64, parse: T) {
        self.seen = consumed;
        self.parked = Some(Parked {
            ptr: Box::into_raw(Box::new(parse)) as *mut (),
            kind: std::any::type_name::<T>(),
            drop: drop_box::<T>,
        });
    }

    /// The parse a receiver of a lost socket kept, taken by the receiver of the next one: `None` -
    /// none, or of another type (dropped then: the pack starts over).
    #[inline]
    pub(crate) fn take_parked<T>(&mut self) -> Option<T> {
        let p = self.parked.take()?;
        if p.kind != std::any::type_name::<T>() {
            self.from = -1;
            return None;
        }
        let p = std::mem::ManuallyDrop::new(p);
        // SAFETY: the box `park` made of a `T`; the `Parked` is forgotten, the box is this one's
        // now
        Some(*unsafe { Box::from_raw(p.ptr as *mut T) })
    }

    /// The pack in progress is whole and ends at `end` of the stream of the socket
    /// ([`RxState::consumed`]): a guaranteed one moves the position behind its last byte.
    ///
    /// [`RxState::consumed`]: crate::lib::adhoc::connection::receiver::RxState::consumed
    #[inline]
    pub fn whole(&mut self, end: i64) {
        if 0 <= self.from {
            self.guarded += end - self.from;
            self.from = -1;
        }
    }

    /// The guaranteed pack in progress is dropped: it is not received, the position stays at its
    /// start.
    #[inline]
    pub fn drop_pack(&mut self) {
        self.from = -1;
    }

    /// The position in the peer's journal behind the last guaranteed pack received whole; -1 — none
    /// is known.
    #[inline]
    pub fn received(&self) -> i64 {
        if self.positioned {
            self.guarded
        } else {
            -1
        }
    }

    /// The position in the peer's journal, the bytes of the guaranteed pack in progress up to
    /// `consumed` of the stream counted: what a datagram transport reports - the peer sends on from
    /// the byte a loss cut a pack at. -1 — none is known.
    #[inline]
    pub fn received_at(&self, consumed: i64) -> i64 {
        if !self.positioned {
            -1
        } else if self.from < 0 {
            self.guarded
        } else {
            self.guarded + consumed - self.from
        }
    }

    /// A guaranteed pack is in progress.
    #[inline]
    pub fn in_pack(&self) -> bool {
        0 <= self.from
    }

    /// The peer's stream goes on from `position` of its journal, with a pack that starts there.
    pub fn position(&mut self, position: i64) {
        self.from = -1;
        self.guarded = position;
        self.told = position;
        self.positioned = true;
        let c = self.control.load(std::sync::atomic::Ordering::Relaxed);
        if !c.is_null() {
            // SAFETY: the control lives in the connection, as this position does
            unsafe { (*c).rx_from(position) };
        }
        let h = self.head.load(std::sync::atomic::Ordering::Relaxed);
        if !h.is_null() {
            use crate::lib::adhoc::connection::control::{set_i32, set_i64, Control};
            // SAFETY: the header is mapped while it is set here
            unsafe {
                set_i64(h, Control::H_GUARDED, position);
                set_i32(h, Control::H_POSITIONED, 1);
            }
        }
    }

    /// The session is over: the position is of no journal any more, a pack kept of it goes.
    pub fn unpositioned(&mut self) {
        let head = std::mem::take(&mut self.head);
        let control = std::mem::take(&mut self.control);
        *self = Self::default();
        self.head = head; // the files of the session go with it ( Control::ended )
        self.control = control;
    }

    /// Appends the position of this receiver for the process that takes the session up after this
    /// one: behind the last guaranteed pack received whole. A pack in progress is not taken along -
    /// its bytes went to an object of this process; the peer's journal holds it from its start, and
    /// sends it again.
    pub fn save(&self, out: &mut Vec<u8>) {
        out.push(self.positioned as u8);
        out.extend_from_slice(&self.guarded.to_le_bytes());
    }

    /// Takes up the position [`Position::save`] wrote in the process before: the stream of the peer
    /// goes on behind the last guaranteed pack received whole, the peer is asked for it on the next
    /// socket.
    pub fn load(&mut self, from: &mut &[u8]) -> std::io::Result<()> {
        use crate::lib::adhoc::connection::transmitter::resumable::{take, take_i64};
        let known = take(from, 1)?[0] != 0;
        let at = take_i64(from)?;
        if known {
            self.position(at);
        } else {
            self.unpositioned();
        }
        Ok(())
    }

    /// The end of a receive call that took the stream up to `consumed`: how many borders of a step
    /// (`1 << shift` bytes) the guaranteed packs taken crossed since the last call - a `Confirm_`
    /// for each. The bytes of a guaranteed pack in progress count: a pack larger than the journal
    /// of the peer goes on while it is received.
    #[inline]
    pub fn due(&mut self, consumed: i64, shift: u32) -> u32 {
        self.seen = consumed;
        if !self.positioned {
            return 0;
        }
        let h = self.head.load(std::sync::atomic::Ordering::Relaxed);
        let mut at = self.received_at(consumed);
        if !h.is_null() {
            // the position behind the last guaranteed pack received whole, its handler returned: the next process goes on from it
            // SAFETY: the header is mapped while it is set here; the control set it, and lives in the connection as this does
            unsafe {
                crate::lib::adhoc::connection::control::set_i64(h, crate::lib::adhoc::connection::control::Control::H_GUARDED, self.guarded);
                at = (*self.control.load(std::sync::atomic::Ordering::Relaxed)).durable(self.guarded, at);
                // no pack end past the disk ( resumable_sync_ms )
            }
        }
        let borders = (at >> shift) - (self.told >> shift);
        if borders <= 0 {
            return 0;
        }
        self.told = at;
        borders as u32
    }

    /// The two sides agreed on `at` (`Ok_`): the steps are counted from it.
    #[inline]
    pub fn counted_from(&mut self, at: i64) {
        self.told = at;
    }
}
