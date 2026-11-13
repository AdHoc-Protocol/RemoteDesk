// The video of the sessions: the layout of the displays, then the frames of the display each viewer selected as H.264, the pointer
// beside them.
//
// One capture and one encoder a display, however many viewers show it: a display has a thread of its own while one does, and every
// frame it encodes goes to all of them - the same `Frame`, serialized by each connection. A page acknowledges every frame it showed;
// a frame is encoded only when every viewer of the display has fewer than `IN_FLIGHT` unacknowledged, so the slowest lowers the frame
// rate instead of a queue filling. One that holds the others back longer than `STALLED` is passed over - it takes no frames - until
// it has acknowledged all it had, then a key frame lets it go on. How long the acknowledgements of the slowest take sets the bitrate:
// it goes down when they come late - the link is full - and back up while they come quickly. A page turns its video off while it is
// not seen - it takes no frames then, and holds nobody back - and sets the most frames a second: a display goes at the most any of its
// viewers asks. How a display is captured and encoded is the system's: `os::video::Source`.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwapOption;
use std::time::{Duration, Instant};

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};
use crate::os::video::{self as osv, Display, Source};

const IN_FLIGHT: u32 = 2;
/// A viewer that holds the others back this long is passed over until it catches up.
const STALLED: Duration = Duration::from_secs(3);
pub const DEFAULT_FPS: u32 = 30;
/// No display: of a viewer not given one yet; the primary display, selected.
const NONE: u8 = 255;
/// The bitrate of a session's start, and the most it goes up to; the least it goes down to.
const MAX_BITRATE: u32 = 8_000_000;
const MIN_BITRATE: u32 = 1_000_000;

/// What the systems fail with.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// The capture of the display is to be made again: the desktop switched, the mode changed, the encoder fell back to another.
#[derive(Debug)]
pub struct Restart(pub String);

impl std::fmt::Display for Restart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}

impl std::error::Error for Restart {}

/// One encoded access unit: Annex B, the parameter sets in front of a key frame.
pub struct Unit {
    pub data:     Vec<u8>,
    pub keyframe: bool,
}

/// What one capture gave.
pub enum Next {
    /// The desktop changed: the source has a new frame.
    Frame,
    /// Only the pointer moved or changed: of the systems that tell the pointer beside the frames.
    #[allow(dead_code)]
    Pointer,
    /// Nothing within the time.
    Nothing,
}

/// The pointer of the machine as the capture told it last.
#[derive(Default)]
pub struct Pointer {
    pub visible: bool,
    /// Pixels of the display.
    pub x:       i32,
    pub y:       i32,
    /// A new shape: width, height, hotspot, BGRA. None from a system that draws the pointer into the frames.
    pub shape:   Option<(u32, u32, u32, u32, Vec<u8>)>,
}

/// What a viewer asks of the video: set by its packs, read by the thread of the display it shows.
pub struct Watch {
    conn:         Arc<Connection>,
    name:         String,
    acked:        AtomicU32, // the last frame the page showed
    keyframe:     AtomicBool,
    enabled:      AtomicBool,
    fps:          AtomicU32,
    select:       AtomicU8,  // NONE - the primary display
    showing:      AtomicU8,  // the display it is given, NONE - not yet: set by the thread of the layout
    told_layout:  AtomicBool,
    active:       AtomicU8,  // the display the user of the machine works on, as the page was told; NONE - not yet
    latest_event: AtomicU32,
    event_seen:   AtomicBool,
}

/// The viewers of the video: read by every acknowledgement and by the threads of the displays, without a lock.
static WATCHES: ArcSwapOption<Vec<Arc<Watch>>> = ArcSwapOption::const_empty();
/// Changes when a viewer comes or leaves, is given another display, turns its video on or off or asks another frame rate: the threads
/// of the displays look at their viewers again.
static CHANGED: AtomicU32 = AtomicU32::new(0);
/// The number of the last frame, of all displays: a viewer that goes to another display tells the frames of the one before apart.
static FRAME_ID: AtomicU32 = AtomicU32::new(0);
/// The displays with a thread now.
static STREAMS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// The thread of the layout, and whether it has something to do.
static LAYOUT: OnceLock<std::thread::Thread> = OnceLock::new();
static WAKE: AtomicBool = AtomicBool::new(false);

pub fn register() {
    actor0::working::on_receive_d::Viewer_VideoAck.add(on_ack);
    actor0::working::on_receive_d::Viewer_KeyframeRequest.add(on_keyframe);
    actor0::working::on_receive_d::Viewer_SelectDisplay.add(on_select);
    actor0::working::on_receive_d::Viewer_VideoControl.add(on_control);
    actor0::working::on_receive_d::Viewer_EventTime.add(on_event_time);
}

/// The viewer on `conn`, named `name`, takes the video: the layout, then the frames of the primary display.
pub fn start(conn: Arc<Connection>, name: String) {
    let watch = Arc::new(Watch {
        conn,
        name,
        acked: AtomicU32::new(0),
        keyframe: AtomicBool::new(false),
        enabled: AtomicBool::new(true),
        fps: AtomicU32::new(DEFAULT_FPS),
        select: AtomicU8::new(NONE),
        showing: AtomicU8::new(NONE),
        told_layout: AtomicBool::new(false),
        active: AtomicU8::new(NONE),
        latest_event: AtomicU32::new(0),
        event_seen: AtomicBool::new(false),
    });
    WATCHES.rcu(|all| {
        let mut all = all.as_deref().cloned().unwrap_or_default();
        all.push(watch.clone());
        Some(Arc::new(all))
    });
    changed();
    wake();
}

/// The viewer on `conn` left: its display stops with the last viewer of it.
pub fn stop(conn: &Connection) {
    WATCHES.rcu(|all| {
        let mut all = all.as_deref().cloned().unwrap_or_default();
        all.retain(|w| !std::ptr::eq(Arc::as_ptr(&w.conn), conn));
        (!all.is_empty()).then(|| Arc::new(all))
    });
    changed();
}

fn changed() { CHANGED.fetch_add(1, Ordering::Release); }

/// The thread of the layout looks at the displays and the viewers again: started by the first viewer.
fn wake() {
    WAKE.store(true, Ordering::Release);
    LAYOUT.get_or_init(|| std::thread::Builder::new().name("video".into()).spawn(layout).expect("the thread of the video").thread().clone()).unpark();
}

/// `f` with the video of the viewer on `conn`.
fn with_watch(conn: &Connection, f: impl FnOnce(&Watch)) {
    if let Some(w) = WATCHES.load().as_deref().and_then(|all| all.iter().find(|w| std::ptr::eq(Arc::as_ptr(&w.conn), conn))) {
        f(w);
    }
}

fn on_ack(ack: Viewer_VideoAck, conn: &Connection, _: &Actor0) {
    with_watch(conn, |w| {
        w.acked.fetch_max(ack.frame_id, Ordering::Relaxed);
    });
}

fn on_keyframe(_: &mut Viewer_KeyframeRequest, conn: &Connection, _: &Actor0) {
    with_watch(conn, |w| w.keyframe.store(true, Ordering::Relaxed));
}

fn on_select(select: Viewer_SelectDisplay, conn: &Connection, _: &Actor0) {
    with_watch(conn, |w| w.select.store(select.screen, Ordering::Relaxed));
    wake();
}

/// Off: the viewer takes no frames until on again - its page is not seen; a display no viewer takes is captured, not encoded. The frame
/// rate: the capture of the display starts again with the most its viewers ask.
fn on_control(control: Viewer_VideoControl, conn: &Connection, _: &Actor0) {
    with_watch(conn, |w| {
        if control.enable_hasValue() {
            w.enabled.store(control.enable(), Ordering::Relaxed);
        }
        if control.framerate_hasValue() {
            w.fps.store(control.framerate() as u32, Ordering::Relaxed);
        }
    });
    changed();
}

fn on_event_time(time: Viewer_EventTime, conn: &Connection, _: &Actor0) {
    with_watch(conn, |w| {
        w.latest_event.store(time.ms, Ordering::Relaxed);
        w.event_seen.store(true, Ordering::Relaxed);
    });
}

// ── the threads ──

/// The thread of the layout, woken when there is something to do: tells the pages the displays - one that came, or all when they
/// changed -, gives each viewer the display it selected, the primary one when that is gone, and starts the thread of a display a
/// viewer shows.
fn layout() {
    if let Err(e) = osv::startup() {
        eprintln!("video: the system does not start the video: {e}");
        return;
    }
    let mut told: Vec<(u8, u32, u32, String)> = Vec::new();
    loop {
        if !WAKE.swap(false, Ordering::AcqRel) {
            std::thread::park();
            continue;
        }
        let Some(watches) = WATCHES.load_full() else { continue };
        let displays = osv::displays();
        if displays.is_empty() {
            eprintln!("video: no display to capture");
            std::thread::sleep(Duration::from_secs(1));
            WAKE.store(true, Ordering::Release);
            continue;
        }
        crate::os::input::set_displays(&displays);
        let now: Vec<_> = displays.iter().map(|d| (d.screen, d.width, d.height, d.name.clone())).collect();
        let changed_layout = now != told;
        told = now;
        let layout = layout_of(&displays);
        let primary = displays.iter().find(|d| d.primary).unwrap_or(&displays[0]).screen;
        let mut moved = false;
        for w in watches.iter() {
            if !w.told_layout.swap(true, Ordering::Relaxed) || changed_layout {
                actor0::working::transmitter::send_machine_video_layout(layout.clone(), &w.conn);
            }
            let wanted = w.select.load(Ordering::Relaxed);
            let screen = if displays.iter().any(|d| d.screen == wanted) { wanted } else { primary };
            moved |= w.showing.swap(screen, Ordering::Relaxed) != screen;
        }
        if moved {
            changed();
        }
        let mut streams = STREAMS.lock().unwrap();
        for w in watches.iter() {
            let screen = w.showing.load(Ordering::Relaxed);
            if !streams.contains(&screen) {
                streams.push(screen);
                std::thread::Builder::new().name(format!("video {screen}")).spawn(move || run(screen)).expect("the thread of a display");
            }
        }
    }
}

/// A viewer shows `screen` now.
fn shown(screen: u8) -> bool { WATCHES.load().as_deref().is_some_and(|all| all.iter().any(|w| w.showing.load(Ordering::Relaxed) == screen)) }

/// The thread of a display: its frames, to the viewers that show it, until none does.
fn run(screen: u8) {
    let mut watchers = Watchers { list: Vec::with_capacity(8), shape: None, position: None };
    if let Err(e) = osv::startup() {
        eprintln!("video: the system does not start the video: {e}");
        STREAMS.lock().unwrap().retain(|&s| s != screen);
        return;
    }
    println!("video: display {screen} started");
    loop {
        // none shows it: the thread ends - with the display let go, so a thread started for it again may capture it
        if !shown(screen) {
            let mut streams = STREAMS.lock().unwrap();
            if !shown(screen) {
                streams.retain(|&s| s != screen);
                return println!("video: display {screen} stopped");
            }
        }
        let displays = osv::displays();
        let Some(display) = displays.iter().find(|d| d.screen == screen) else {
            wake(); // the display is gone: its viewers are given another one
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        if let Err(e) = stream(screen, &displays, display, &mut watchers) {
            if let Some(why) = e.downcast_ref::<Restart>() {
                println!("video: {why}");
            } else if !osv::lost(&e) {
                eprintln!("video: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
            wake(); // the mode may have changed: the layout is told again
        }
    }
}

/// A viewer of a display, as the thread of the display keeps it.
struct Subscriber {
    watch:      Arc<Watch>,
    /// Frames given to its transport and not acknowledged yet, the oldest first.
    sent:       VecDeque<u32>,
    /// Frames its transport had no room for: they go first, in order, once it has - a frame lost would break the ones after it.
    waiting:    VecDeque<Arc<Frame>>,
    /// Its video is on, as the thread looked last.
    enabled:    bool,
    /// It takes no frame before a key frame: it came, was passed over, turned its video on again, or the encoder is new.
    need_key:   bool,
    /// Passed over: it held the others back too long, and takes no frames until it has acknowledged all it had.
    skipped:    bool,
    /// Since when it has no room for another frame.
    full_since: Option<Instant>,
    /// The frame before its first key frame: its acknowledgements count for the bitrate from there.
    floor:      u32,
    /// Not told the pointer yet.
    fresh:      bool,
}

impl Subscriber {
    fn new(watch: Arc<Watch>) -> Self {
        let sent = VecDeque::with_capacity(8);
        Self { watch, sent, waiting: VecDeque::with_capacity(4), enabled: false, need_key: true, skipped: false, full_since: None, floor: 0, fresh: true }
    }

    /// Takes the frames, and holds the others back while it has no room.
    fn taking(&self) -> bool { self.enabled && !self.skipped }

    fn in_flight(&self) -> u32 { (self.sent.len() + self.waiting.len()) as u32 }

    /// The frames the page showed are let go.
    fn acknowledged(&mut self) {
        let acked = self.watch.acked.load(Ordering::Relaxed);
        while self.sent.front().is_some_and(|&id| !newer(id, acked)) {
            self.sent.pop_front();
        }
    }

    /// The frames waiting for room in its transport, while it has room.
    fn flush(&mut self) {
        while let Some(frame) = self.waiting.front() {
            if !actor0::working::transmitter::send_machine_video_packet(frame.clone(), &self.watch.conn) {
                return;
            }
            self.sent.push_back(frame.frame_id);
            self.waiting.pop_front();
        }
    }
}

/// `a` is after `b`, the numbers going round.
fn newer(a: u32, b: u32) -> bool { (a.wrapping_sub(b) as i32) > 0 }

/// What the thread of a display keeps over the captures it makes again.
struct Watchers {
    list:     Vec<Subscriber>,
    /// The shape of the pointer and where it is, as the viewers were told last: a viewer that comes is told them too.
    shape:    Option<Machine_CursorShape>,
    position: Option<Machine_HostCursorPosition>,
}

/// The viewers of `screen` as they are now: those gone are let go; one that came, or turned its video on again, waits for a key frame -
/// true then. The frame rate: the most a viewer with the video on asks; of all of them while none has it on.
fn reconcile(screen: u8, list: &mut Vec<Subscriber>) -> (u32, bool) {
    let all = WATCHES.load();
    let all = all.as_deref().map(Vec::as_slice).unwrap_or_default();
    list.retain(|v| v.watch.showing.load(Ordering::Relaxed) == screen && all.iter().any(|w| Arc::ptr_eq(w, &v.watch)));
    for w in all.iter().filter(|w| w.showing.load(Ordering::Relaxed) == screen) {
        if !list.iter().any(|v| Arc::ptr_eq(&v.watch, w)) {
            list.push(Subscriber::new(w.clone()));
        }
    }
    let (mut key, mut on, mut any) = (false, 0, 0);
    for v in list.iter_mut() {
        let enabled = v.watch.enabled.load(Ordering::Relaxed);
        if enabled && !v.enabled {
            v.need_key = true;
            key = true;
        }
        if !enabled {
            v.waiting.clear();
            v.full_since = None;
        }
        v.enabled = enabled;
        let fps = v.watch.fps.load(Ordering::Relaxed);
        any = any.max(fps);
        if enabled {
            on = on.max(fps);
        }
    }
    (if on > 0 { on } else { any.max(1) }, key)
}

/// The frames of one display, until no viewer shows it, the frame rate asked changes or the display is lost.
fn stream(screen: u8, displays: &[Display], display: &Display, w: &mut Watchers) -> Result<(), Error> {
    let mut seen = CHANGED.load(Ordering::Acquire);
    let (fps, _) = reconcile(screen, &mut w.list);
    if w.list.is_empty() {
        return Ok(());
    }
    // a new encoder: every viewer starts from its first key frame
    for v in w.list.iter_mut() {
        v.need_key = true;
        v.waiting.clear();
    }
    let interval = Duration::from_millis(1000 / fps.max(1) as u64);
    let mut rate = Rate::new();
    let mut source = Source::new(display, fps, rate.bitrate)?;
    let mut format_sent = false;
    let mut made = 0u32; // frames since the last report
    let mut report = Instant::now();
    let mut active_checked = Instant::now() - Duration::from_secs(1);
    let mut units: Vec<Unit> = Vec::new();
    // the frames in the encoder: when each was captured and given to it
    let mut encoding: VecDeque<(Instant, Instant)> = VecDeque::with_capacity(4);
    let mut have_frame = false;
    let mut dirty = false;
    let mut want_key = true;
    let mut last_frame = Instant::now() - Duration::from_secs(1);
    let mut last_pointer = (false, i32::MIN, i32::MIN);

    loop {
        let now_changed = CHANGED.load(Ordering::Acquire);
        if now_changed != seen {
            seen = now_changed;
            let (wanted, key) = reconcile(screen, &mut w.list);
            want_key |= key;
            if w.list.is_empty() || wanted != fps {
                return Ok(());
            }
        }
        if report.elapsed() >= Duration::from_secs(5) {
            if made > 0 {
                let mut line = format!("video: display {screen}: {made} frame(s) in 5 s, unacknowledged");
                for v in &w.list {
                    let state = if !v.enabled { " ( off )" } else if v.skipped { " ( passed over )" } else { "" };
                    let _ = write!(line, " · {} {}{state}", v.watch.name, v.in_flight());
                }
                println!("{line}");
            }
            report = Instant::now();
            made = 0;
        }
        // while the encoder works, what it gives is taken soon; a changed frame that came too early goes when its interval is
        // over, not a whole interval later; one held back by the page's acknowledgements looks again in a moment
        let wait = if !encoding.is_empty() {
            Duration::from_millis(2)
        } else if dirty {
            interval.saturating_sub(last_frame.elapsed()).max(Duration::from_millis(4))
        } else {
            interval
        };
        let next = source.next(wait)?;
        let captured_at = Instant::now();
        if let Next::Frame = next {
            have_frame = true;
            dirty = true;
        }
        send_pointer(&mut source, display, &mut last_pointer, w);
        if active_checked.elapsed() >= Duration::from_millis(500) {
            active_checked = Instant::now();
            if let Some(screen) = osv::active(displays) {
                for v in &w.list {
                    if v.watch.active.swap(screen, Ordering::Relaxed) != screen {
                        actor0::working::transmitter::send_machine_active_display(Machine_ActiveDisplay { screen }, &v.watch.conn);
                    }
                }
            }
        }

        // the bitrate by the slowest of the viewers that take every frame
        let mut slowest: Option<u32> = None;
        for v in w.list.iter_mut() {
            v.acknowledged();
            if v.taking() && !v.need_key {
                let acked = v.watch.acked.load(Ordering::Relaxed);
                let acked = if newer(acked, v.floor) { acked } else { v.floor };
                slowest = Some(slowest.filter(|&s| !newer(s, acked)).unwrap_or(acked));
            }
            v.flush();
        }
        if slowest.is_some_and(|acked| rate.acked(acked)) {
            source.set_bitrate(rate.bitrate);
        }
        units.clear();
        source.poll(&mut units)?;
        if !units.is_empty() {
            let (width, height) = source.size();
            for unit in units.drain(..) {
                let (captured, given) = encoding.pop_front().unwrap_or((captured_at, captured_at));
                let id = send(screen, w, &mut format_sent, unit, width, height, (given - captured).as_millis() as u16, given.elapsed().as_millis() as u16);
                rate.sent(id);
                made += 1;
            }
        }

        // a frame is encoded when every viewer that takes the frames has room for it; one that has had none for `STALLED` holds the
        // others back no longer, while another goes on - and one passed over that acknowledged all it had goes on from a key frame
        let ahead = source.ahead();
        let now = Instant::now();
        let (mut room, mut taking) = (true, false);
        for v in w.list.iter_mut() {
            if v.watch.keyframe.load(Ordering::Relaxed) && v.watch.keyframe.swap(false, Ordering::Relaxed) {
                want_key = true; // its decoder lost its state
            }
            if v.skipped && v.sent.is_empty() {
                v.skipped = false;
                v.need_key = true;
                want_key |= v.enabled; // one turned off gets its key frame when it is on again
            }
            if !v.taking() {
                continue;
            }
            taking = true;
            if v.in_flight() + (encoding.len() as u32) < IN_FLIGHT + ahead as u32 {
                v.full_since = None;
            } else {
                v.full_since.get_or_insert(now);
                room = false;
            }
        }
        let stalled = |v: &Subscriber| v.taking() && v.full_since.is_some_and(|since| now - since >= STALLED);
        if !room && w.list.iter().any(|v| v.taking() && !stalled(v)) {
            for v in w.list.iter_mut().filter(|v| stalled(v)) {
                println!("video: display {screen}: {} is passed over until it catches up", v.watch.name);
                v.skipped = true;
                v.waiting.clear();
                v.full_since = None;
            }
            room = w.list.iter().all(|v| !v.taking() || v.full_since.is_none());
        }

        if !have_frame || !taking || !(dirty || want_key) {
            continue;
        }
        if last_frame.elapsed() < interval || !room || encoding.len() >= ahead {
            continue; // the frame waits: the next change, or the next turn, takes it
        }

        if want_key {
            want_key = false;
            source.key_frame();
        }
        let encode_start = Instant::now();
        if !source.submit()? {
            continue; // the encoder is busy: the frame waits
        }
        if let Some(what) = source.announce() {
            println!("video: display {} ( {} ) {what}", display.screen, display.name);
        }
        encoding.push_back((captured_at, encode_start));
        dirty = false;
        last_frame = captured_at; // the cadence is of the captures: the time the encoder takes is not added to it
    }
}

/// Buffers of frames: an encoder fills one, it goes out in a `Frame`, and comes back here once the transport has sent it - a frame
/// costs no allocation, however many viewers it goes to. No more than the frames that can be on their way, of two displays, are kept.
static BUFFERS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
const BUFFERS_KEPT: usize = (IN_FLIGHT as usize + 2) * 2;

/// An empty buffer for the bytes of a frame: one that came back, with its capacity, or a new one.
pub fn buffer() -> Vec<u8> {
    BUFFERS.lock().unwrap().pop().unwrap_or_default()
}

/// `Machine.VideoPacket` as the machine sends it: the bytes in a buffer of `BUFFERS`.
#[derive(Default)]
pub struct Frame {
    screen:     u8,
    frame_id:   u32,
    keyframe:   bool,
    format:     Option<Machine_VideoFormat>,
    data:       Vec<u8>,
    capture_ms: u16,
    encode_ms:  u16,
    event_ms:   Option<u32>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        let mut data = std::mem::take(&mut self.data);
        if data.capacity() == 0 {
            return;
        }
        data.clear();
        let mut kept = BUFFERS.lock().unwrap();
        if kept.len() < BUFFERS_KEPT {
            kept.push(data);
        }
    }
}

impl Machine_VideoPacket for Frame {
    fn _screen(&self) -> u8 { self.screen }
    fn set__screen(&mut self, value: u8) { self.screen = value; }
    fn _frame_id(&self) -> u32 { self.frame_id }
    fn set__frame_id(&mut self, value: u32) { self.frame_id = value; }
    fn _keyframe(&self) -> bool { self.keyframe }
    fn set__keyframe(&mut self, value: bool) { self.keyframe = value; }
    fn _format(&self) -> Option<Machine_VideoFormat> { self.format }
    fn set__format(&mut self, value: Option<Machine_VideoFormat>) { self.format = value; }
    fn _data(&self) -> Option<&[u8]> { Some(&self.data) }
    fn _data_mut(&mut self) -> &mut [u8] { &mut self.data }
    fn _data_new(&mut self, len: usize) -> &mut [u8] {
        self.data.resize(len, 0);
        &mut self.data
    }
    fn _data_to_null(&mut self) { self.data.clear(); }
    fn _capture_ms(&self) -> u16 { self.capture_ms }
    fn set__capture_ms(&mut self, value: u16) { self.capture_ms = value; }
    fn _encode_ms(&self) -> u16 { self.encode_ms }
    fn set__encode_ms(&mut self, value: u16) { self.encode_ms = value; }
    fn _latest_event_ms(&self) -> Option<u32> { self.event_ms }
    fn set__latest_event_ms(&mut self, value: Option<u32>) { self.event_ms = value; }
}

/// A frame the encoder gave, to every viewer of the display that takes it: one `Frame`, each connection serializes it from there. Its
/// number is returned.
#[allow(clippy::too_many_arguments)]
fn send(screen: u8, w: &mut Watchers, format_sent: &mut bool, unit: Unit, width: u32, height: u32, capture_ms: u16, encode_ms: u16) -> u32 {
    let frame_id = FRAME_ID.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let mut packet = Frame { screen, frame_id, keyframe: unit.keyframe, format: None, data: unit.data, capture_ms, encode_ms, event_ms: None };
    if !*format_sent || unit.keyframe {
        let mut format = Machine_VideoFormat::new(0);
        format.set_width(width as u16);
        format.set_height(height as u16);
        packet.format = Some(format);
        *format_sent = true;
    }
    // the time of the input is of the viewer that controls: the machine takes no other input
    if let Some(v) = w.list.iter().find(|v| crate::session::controls(&v.watch.conn)) {
        if v.watch.event_seen.swap(false, Ordering::Relaxed) {
            packet.event_ms = Some(v.watch.latest_event.load(Ordering::Relaxed));
        }
    }
    let packet = Arc::new(packet);
    for v in w.list.iter_mut() {
        if !v.taking() || (v.need_key && !packet.keyframe) {
            continue;
        }
        if v.need_key {
            v.need_key = false;
            v.floor = frame_id.wrapping_sub(1);
        }
        v.waiting.push_back(packet.clone());
        v.flush();
    }
    frame_id
}

/// The bitrate fitted to the link by the time the page takes to acknowledge a frame. The least time seen is the link empty: much
/// more than that - frames queue up on their way - takes the bitrate down by a quarter; close to it for a while, up by an eighth.
struct Rate {
    bitrate:     u32,
    /// When each of the last frames went to the transport, by `frame_id % 16`.
    sent:        [Instant; 16],
    newest:      u32,
    acked:       u32,
    least:       Duration,
    least_since: Instant,
    smooth:      Duration,
    changed:     Instant,
}

impl Rate {
    fn new() -> Self {
        let now = Instant::now();
        Self { bitrate: MAX_BITRATE, sent: [now; 16], newest: 0, acked: 0, least: Duration::MAX, least_since: now, smooth: Duration::ZERO, changed: now }
    }

    fn sent(&mut self, frame_id: u32) {
        self.sent[(frame_id % 16) as usize] = Instant::now();
        self.newest = frame_id;
    }

    /// The page acknowledged up to `acked`: true when the bitrate changed.
    fn acked(&mut self, acked: u32) -> bool {
        if acked == self.acked || self.newest.wrapping_sub(acked) >= 16 {
            return false;
        }
        self.acked = acked;
        let now = Instant::now();
        let took = now - self.sent[(acked % 16) as usize];
        // the least time is looked for anew every 10 s: the route may change
        if took < self.least || self.least_since.elapsed() > Duration::from_secs(10) {
            self.least = took;
            self.least_since = now;
        }
        self.smooth = if self.smooth.is_zero() { took } else { (self.smooth * 7 + took) / 8 };
        let since = self.changed.elapsed();
        let before = self.bitrate;
        if self.smooth > self.least * 2 + Duration::from_millis(40) && since > Duration::from_millis(500) {
            self.bitrate = (self.bitrate / 4 * 3).max(MIN_BITRATE);
        } else if self.smooth < self.least + self.least / 4 + Duration::from_millis(10) && since > Duration::from_secs(2) {
            self.bitrate = (self.bitrate / 8 * 9).min(MAX_BITRATE);
        }
        if self.bitrate == before {
            return false;
        }
        self.changed = now;
        true
    }
}

fn layout_of(displays: &[Display]) -> Machine_VideoLayout {
    let mut layout = Machine_VideoLayout::default();
    layout.displays = Some(
        displays
            .iter()
            .map(|d| {
                let mut m = Machine_Display::default();
                m.set_screen(d.screen);
                m.width = d.width as u16;
                m.height = d.height as u16;
                m.name = Some(d.name.clone());
                m
            })
            .collect(),
    );
    layout
}

/// A new shape of the pointer, and where it is now when it moved, to every viewer of the display; both to a viewer that came.
fn send_pointer(source: &mut Source, display: &Display, last: &mut (bool, i32, i32), viewers: &mut Watchers) {
    let (width, height) = (source.width(), source.height());
    let pointer = source.pointer();
    if let Some((w, h, hx, hy, bgra)) = pointer.shape.take() {
        if w >= 1 && h >= 1 && w <= 256 && h <= 256 {
            let mut shape = Machine_CursorShape::default();
            shape.set_width(w as u16);
            shape.set_height(h as u16);
            shape.hotspot_x = hx.min(255) as u8;
            shape.hotspot_y = hy.min(255) as u8;
            shape.bgra = Some(bgra);
            for v in &viewers.list {
                actor0::working::transmitter::send_machine_cursor_shape(shape.clone(), &v.watch.conn);
            }
            viewers.shape = Some(shape);
        }
    }
    let now = (pointer.visible, pointer.x, pointer.y);
    if now != *last {
        *last = now;
        let position = position_of(now, width, height, display.screen);
        for v in &viewers.list {
            actor0::working::transmitter::send_machine_host_cursor_position(position, &v.watch.conn);
        }
        viewers.position = Some(position);
    }
    for v in viewers.list.iter_mut().filter(|v| v.fresh) {
        v.fresh = false;
        if let Some(shape) = &viewers.shape {
            actor0::working::transmitter::send_machine_cursor_shape(shape.clone(), &v.watch.conn);
        }
        if let Some(position) = viewers.position {
            actor0::working::transmitter::send_machine_host_cursor_position(position, &v.watch.conn);
        }
    }
}

/// The pointer at `now` - visible, x, y in pixels of the display - as the page is told it.
fn position_of(now: (bool, i32, i32), width: u32, height: u32, screen: u8) -> Machine_HostCursorPosition {
    let mut position = Machine_HostCursorPosition::new(0);
    if now.0 {
        let mut at = FractionalCoordinate::new(0);
        at.set_x(fraction(now.1, width));
        at.set_y(fraction(now.2, height));
        at.set_screen(screen);
        position.set_at(at);
    } else {
        position.to_null();
    }
    position
}

fn fraction(v: i32, size: u32) -> u16 { ((v.clamp(0, size as i32 - 1) as u64 * 65535) / (size.max(2) as u64 - 1)) as u16 }
