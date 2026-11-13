// The video on Android: the screen of the phone, shown by MediaProjection into the input surface of its H.264 encoder ( MediaCodec,
// the hardware of the phone ) - the app sets both up ( Host.videoStart ), the frame is not copied on the way. What the encoder gives
// comes here ( `unit` ), Annex B as MediaCodec gives it, and goes out when the loop of src/video.rs submits a frame: the encoder
// encodes each change of the screen by itself, the loop only says when its units may go. Units the viewers have no room for wait
// here; too many, and they are dropped up to the next key frame, which is asked for.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::video::{Error, Next, Pointer, Restart, Unit};

/// Units waiting at most: beyond, the ones waiting are dropped up to a key frame.
const HOLD: usize = 8;
/// MediaCodec.BUFFER_FLAG_KEY_FRAME, BUFFER_FLAG_CODEC_CONFIG.
const KEY: i32 = 1;
const CONFIG: i32 = 2;

/// The screen of the phone: one display.
#[derive(Clone, Debug)]
pub struct Display {
    pub screen:  u8,
    pub x:       i32,
    pub y:       i32,
    /// Pixels.
    pub width:   u32,
    pub height:  u32,
    pub name:    String,
    pub primary: bool,
}

/// The size of the screen as the app told it: width << 32 | height; 0 - not told yet.
static SIZE: AtomicU64 = AtomicU64::new(0);

/// What the encoder gave, and what happened to the screen.
struct Shared {
    /// The parameter sets: put in front of a key frame that comes without them.
    config:   Vec<u8>,
    waiting:  Vec<Unit>,
    /// Counts the units given.
    seq:      u64,
    /// Units are dropped until a key frame.
    dropping: bool,
    /// The screen turned: the encoder is made again for its new size.
    turned:   bool,
    /// The phone stopped showing the screen.
    ended:    bool,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { config: Vec::new(), waiting: Vec::new(), seq: 0, dropping: false, turned: false, ended: false });
static WAKE: Condvar = Condvar::new();

/// The size of the screen: a new one makes the video start again.
pub fn screen_is(width: u32, height: u32) {
    let size = (width as u64) << 32 | height as u64;
    let before = SIZE.swap(size, Ordering::AcqRel);
    if before != 0 && before != size {
        SHARED.lock().unwrap().turned = true;
        WAKE.notify_all();
    }
}

/// The size of the screen in pixels: 0 × 0 before the app told it.
pub fn size() -> (u32, u32) {
    let size = SIZE.load(Ordering::Acquire);
    ((size >> 32) as u32, size as u32)
}

/// The screen of the phone, once the app told its size.
pub fn displays() -> Vec<Display> {
    let size = SIZE.load(Ordering::Acquire);
    if size == 0 {
        return Vec::new();
    }
    vec![Display { screen: 0, x: 0, y: 0, width: (size >> 32) as u32, height: size as u32, name: String::from("the screen of the phone"), primary: true }]
}

/// The phone has one screen.
pub fn active(_displays: &[Display]) -> Option<u8> { Some(0) }

/// The encoder of the phone is the only one.
pub fn software_only() {}

pub fn startup() -> Result<(), Error> { Ok(()) }

/// What fails is told as it is.
pub fn lost(_e: &Error) -> bool { false }

/// A unit of the encoder, as MediaCodec flags it.
pub fn unit(data: &[u8], flags: i32) {
    let mut s = SHARED.lock().unwrap();
    if flags & CONFIG != 0 {
        s.config.clear();
        s.config.extend_from_slice(data);
        return;
    }
    let keyframe = flags & KEY != 0;
    if s.waiting.len() >= HOLD {
        // the viewers took none for long: the units after a dropped one would not decode - all wait for a key frame
        s.waiting.clear();
        s.dropping = true;
        drop(s);
        super::key_frame();
        s = SHARED.lock().unwrap();
    }
    if s.dropping && !keyframe {
        return;
    }
    s.dropping = false;
    let mut bytes = crate::video::buffer();
    bytes.clear();
    // a key frame begins with its parameter sets: a viewer that joins decodes from it
    if keyframe && nal_type(data) != Some(7) {
        bytes.extend_from_slice(&s.config);
    }
    bytes.extend_from_slice(data);
    s.waiting.push(Unit { data: bytes, keyframe });
    s.seq += 1;
    drop(s);
    WAKE.notify_all();
}

/// The type of the first NAL unit of Annex B.
fn nal_type(data: &[u8]) -> Option<u8> {
    let start = if data.starts_with(&[0, 0, 0, 1]) { 4 } else if data.starts_with(&[0, 0, 1]) { 3 } else { return None };
    data.get(start).map(|b| b & 0x1f)
}

/// The phone stopped showing the screen.
pub fn ended() {
    SHARED.lock().unwrap().ended = true;
    WAKE.notify_all();
}

/// The frames of the screen of the phone, encoded.
pub struct Source {
    display:  Display,
    width:    u32,
    height:   u32,
    seen:     u64,
    /// Units the loop let go: `poll` gives them.
    released: Vec<Unit>,
    pointer:  Pointer,
    announce: Option<String>,
}

impl Drop for Source {
    fn drop(&mut self) { super::video_stop(); }
}

impl Source {
    pub fn new(display: &Display, fps: u32, bitrate: u32) -> Result<Self, Error> {
        {
            let mut s = SHARED.lock().unwrap();
            if s.ended {
                return Err("the phone no longer shows its screen: start the app again".into());
            }
            s.waiting.clear();
            s.config.clear();
            s.dropping = false;
            s.turned = false;
        }
        let Some((width, height)) = super::video_start(fps, bitrate) else {
            return Err("the encoder of the phone did not start".into());
        };
        let seen = SHARED.lock().unwrap().seq;
        Ok(Self {
            display: display.clone(),
            width,
            height,
            seen,
            released: Vec::new(),
            pointer: Pointer::default(),
            announce: Some(format!("{}×{} → {width}×{height}, the encoder of the phone", display.width, display.height)),
        })
    }

    /// The size of the screen in pixels.
    pub fn width(&self) -> u32 { self.display.width }
    pub fn height(&self) -> u32 { self.display.height }

    /// A phone has no pointer to show.
    pub fn pointer(&mut self) -> &mut Pointer { &mut self.pointer }

    /// Waits for a unit of the encoder, `wait` at most.
    pub fn next(&mut self, wait: Duration) -> Result<Next, Error> {
        let s = SHARED.lock().unwrap();
        let seen = self.seen;
        let (s, _) = WAKE.wait_timeout_while(s, wait, |s| s.seq == seen && !s.turned && !s.ended).unwrap();
        if s.turned {
            return Err(Restart(String::from("The screen turned")).into());
        }
        if s.ended {
            return Err("the phone stopped showing its screen".into());
        }
        if s.seq == self.seen {
            return Ok(Next::Nothing);
        }
        self.seen = s.seq;
        Ok(Next::Frame)
    }

    pub fn size(&self) -> (u32, u32) { (self.width, self.height) }

    /// Frames the encoder may hold at once.
    pub fn ahead(&self) -> usize { 2 }

    pub fn announce(&mut self) -> Option<String> { self.announce.take() }

    pub fn key_frame(&mut self) { super::key_frame(); }

    pub fn set_bitrate(&mut self, bitrate: u32) { super::bitrate(bitrate); }

    /// The units encoded since go: false - none came yet.
    pub fn submit(&mut self) -> Result<bool, Error> {
        let mut s = SHARED.lock().unwrap();
        if s.waiting.is_empty() {
            return Ok(false);
        }
        self.released.append(&mut s.waiting);
        Ok(true)
    }

    /// The units let go, in the order of the frames.
    pub fn poll(&mut self, out: &mut Vec<Unit>) -> Result<(), Error> {
        out.append(&mut self.released);
        Ok(())
    }
}
