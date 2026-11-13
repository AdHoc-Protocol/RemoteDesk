// The video on macOS: ScreenCaptureKit gives the frames of a display already scaled and in NV12, VideoToolbox encodes them in the
// hardware of the Mac - the frame is not copied on the way. The pointer is drawn into the frames.

use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::ptr::NonNull;
use std::sync::mpsc::{channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGRect};
use objc2_core_graphics::{CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGGetActiveDisplayList, CGMainDisplayID};
use objc2_core_media::{kCMTimeInvalid, CMBlockBuffer, CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex};
use objc2_core_video::CVImageBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Baseline_AutoLevel, kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
};

use crate::video::{Error, Next, Pointer, Unit};

/// The largest side the H.264 encoder of the Mac takes.
const MAX_SIDE: u32 = 4096;
/// NV12, video range: '420v'.
const NV12: u32 = 0x3432_3076;
/// H.264: 'avc1'.
const H264: u32 = 0x6176_6331;

/// A display of the machine: its place in the virtual screen.
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
    pub(super) id:     CGDirectDisplayID,
    /// Points, as the input is given.
    pub(super) bounds: CGRect,
}

// SAFETY: the rectangle is plain numbers
unsafe impl Send for Display {}

/// The displays macOS has active; 16 at most.
pub fn displays() -> Vec<Display> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) }.0 != 0 {
        return Vec::new();
    }
    let main = CGMainDisplayID();
    ids[..count as usize]
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            let bounds = CGDisplayBounds(id);
            let mode = CGDisplayCopyDisplayMode(id);
            let (pw, ph) = mode.as_deref().map_or((bounds.size.width as usize, bounds.size.height as usize), |m| (CGDisplayMode::pixel_width(Some(m)), CGDisplayMode::pixel_height(Some(m))));
            let scale = if bounds.size.width > 0.0 { pw as f64 / bounds.size.width } else { 1.0 };
            Display {
                screen: i as u8,
                x: (bounds.origin.x * scale) as i32,
                y: (bounds.origin.y * scale) as i32,
                width: pw as u32,
                height: ph as u32,
                name: format!("Display {id}"),
                primary: id == main,
                id,
                bounds,
            }
        })
        .collect()
}

/// The display of the pointer: the user works there.
pub fn active(displays: &[Display]) -> Option<u8> {
    let event = objc2_core_graphics::CGEvent::new(None)?;
    let p = objc2_core_graphics::CGEvent::location(Some(&event));
    displays
        .iter()
        .find(|d| p.x >= d.bounds.origin.x && p.y >= d.bounds.origin.y && p.x < d.bounds.origin.x + d.bounds.size.width && p.y < d.bounds.origin.y + d.bounds.size.height)
        .map(|d| d.screen)
}

/// The encoder of the Mac is the only one.
pub fn software_only() {}

pub fn startup() -> Result<(), Error> { Ok(()) }

/// What fails is told as it is: the video starts again by `Restart` only.
pub fn lost(_e: &Error) -> bool { false }

// ── ScreenCaptureKit ──

/// A frame of ScreenCaptureKit: the pixel buffer it gave, kept until the next.
struct Image(CFRetained<CVImageBuffer>);

// SAFETY: a pixel buffer is not changed once delivered, and goes to the encoder of one thread
unsafe impl Send for Image {}

/// What the capture and the encoder give, shared with the queue of ScreenCaptureKit and the threads of VideoToolbox.
#[derive(Default)]
pub(super) struct Shared {
    latest: Mutex<Latest>,
    /// Wakes `next`: a frame came, or the encoder gave an access unit.
    wake:   Condvar,
    sound:  Mutex<Option<Sound>>,
}

#[derive(Default)]
struct Latest {
    image: Option<Image>,
    /// Grows with every frame.
    seq:   u64,
    /// What the encoder gave since `poll`: the access units, in the order of the frames.
    units: Vec<Unit>,
}

/// The sound - 16-bit stereo, little endian - in pieces to `sound::capture`, which gives them back to be filled again.
pub(super) struct Sound {
    full:    SyncSender<Vec<u8>>,
    empty:   Receiver<Vec<u8>>,
    /// The bytes of a block buffer of more than one piece.
    scratch: Vec<u8>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RemoteDeskCaptureOutput"]
    #[ivars = Arc<Shared>]
    pub(super) struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            let shared = self.ivars();
            if kind == SCStreamOutputType::Screen {
                // a frame of no change carries no image
                if let Some(image) = unsafe { sample.image_buffer() } {
                    let mut latest = shared.latest.lock().unwrap();
                    latest.image = Some(Image(image));
                    latest.seq += 1;
                    drop(latest);
                    shared.wake.notify_all();
                }
            } else if kind == SCStreamOutputType::Audio {
                if let Some(sound) = &mut *shared.sound.lock().unwrap() {
                    let mut piece = sound.empty.try_recv().unwrap_or_default();
                    if unsafe { pcm16(sample, &mut sound.scratch, &mut piece) } {
                        let _ = sound.full.try_send(piece);
                    }
                }
            }
        }
    }
);

impl Output {
    pub(super) fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        unsafe { msg_send![super(this), init] }
    }
}

/// The bytes of `block`: where they are when they are of one piece, else copied into `scratch`.
unsafe fn bytes_of<'a>(block: &'a CMBlockBuffer, scratch: &'a mut Vec<u8>) -> Option<&'a [u8]> {
    let (mut here, mut total, mut data) = (0usize, 0usize, std::ptr::null_mut::<c_char>());
    if unsafe { block.data_pointer(0, &mut here, &mut total, &mut data) } == 0 && !data.is_null() && here == total {
        // valid while the block is referenced: the caller holds it
        return Some(unsafe { std::slice::from_raw_parts(data as *const u8, total) });
    }
    let length = unsafe { block.data_length() };
    scratch.clear();
    scratch.resize(length, 0);
    if length == 0 || unsafe { block.copy_data_bytes(0, length, NonNull::new(scratch.as_mut_ptr() as *mut c_void)?) } != 0 {
        return None;
    }
    Some(scratch)
}

/// The sound of ScreenCaptureKit - 32-bit float, the channels one after the other - into `out` as 16-bit stereo interleaved.
unsafe fn pcm16(sample: &CMSampleBuffer, scratch: &mut Vec<u8>, out: &mut Vec<u8>) -> bool {
    let Some(block) = (unsafe { sample.data_buffer() }) else { return false };
    let frames = unsafe { sample.num_samples() } as usize;
    let Some(raw) = (unsafe { bytes_of(&block, scratch) }) else { return false };
    if frames == 0 || raw.len() < frames * 4 {
        return false;
    }
    let left = &raw[..frames * 4];
    let right = if raw.len() >= frames * 8 { &raw[frames * 4..frames * 8] } else { left };
    out.clear();
    out.reserve(frames * 4);
    for (l, r) in left.chunks_exact(4).zip(right.chunks_exact(4)) {
        for v in [l, r] {
            let v = f32::from_le_bytes([v[0], v[1], v[2], v[3]]);
            out.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
    }
    true
}

struct Content(Retained<SCShareableContent>);

// SAFETY: the content is read only
unsafe impl Send for Content {}

/// What ScreenCaptureKit can capture: it asks for the permission Screen Recording the first time.
fn shareable() -> Result<Retained<SCShareableContent>, Error> {
    let (tx, rx) = channel::<Result<Content, String>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let got = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(Content(c)),
            None => Err(unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| String::from("no content"))),
        };
        let _ = tx.send(got);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => Err(format!("the screen cannot be captured: {e} - is Screen Recording allowed?").into()),
        Err(_) => Err("ScreenCaptureKit did not answer".into()),
    }
}

/// A stream of ScreenCaptureKit, stopped when dropped.
pub(super) struct Capture {
    stream:  Retained<SCStream>,
    _output: Retained<Output>,
    _queue:  DispatchRetained<DispatchQueue>,
}

impl Drop for Capture {
    fn drop(&mut self) { unsafe { self.stream.stopCaptureWithCompletionHandler(None) }; }
}

impl Capture {
    /// The display `id`: `width` × `height` pixels of NV12 at `fps` - or its sound only, when `sound`.
    pub(super) fn start(id: CGDirectDisplayID, width: u32, height: u32, fps: u32, shared: Arc<Shared>, sound: bool) -> Result<Self, Error> {
        let content = shareable()?;
        let displays = unsafe { content.displays() };
        let display: Retained<SCDisplay> = displays.iter().find(|d| unsafe { d.displayID() } == id).ok_or("the display is gone")?;
        unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setPixelFormat(NV12);
            config.setMinimumFrameInterval(CMTime::new(1, fps.max(1) as i32));
            config.setShowsCursor(true);
            config.setQueueDepth(4);
            if sound {
                config.setCapturesAudio(true);
                config.setSampleRate(crate::audio::RATE as isize);
                config.setChannelCount(2);
                config.setExcludesCurrentProcessAudio(true);
            }
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            let output = Output::new(shared);
            let queue = DispatchQueue::new("rdp.capture", None);
            let kind = if sound { SCStreamOutputType::Audio } else { SCStreamOutputType::Screen };
            stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), kind, Some(&queue)).map_err(|e| e.localizedDescription().to_string())?;
            let (tx, rx) = channel::<Option<String>>();
            let done = RcBlock::new(move |error: *mut NSError| {
                let _ = tx.send(error.as_ref().map(|e| e.localizedDescription().to_string()));
            });
            stream.startCaptureWithCompletionHandler(Some(&done));
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(None) => Ok(Self { stream, _output: output, _queue: queue }),
                Ok(Some(e)) => Err(format!("the capture does not start: {e}").into()),
                Err(_) => Err("the capture did not start".into()),
            }
        }
    }
}

/// The sound of the capture, for `sound::capture`: the pieces go to `full`, come back by `empty`.
pub(super) fn sound_capture(full: SyncSender<Vec<u8>>, empty: Receiver<Vec<u8>>) -> Result<Capture, Error> {
    let display = displays().into_iter().find(|d| d.primary).ok_or("no display")?;
    let shared = Arc::new(Shared::default());
    *shared.sound.lock().unwrap() = Some(Sound { full, empty, scratch: Vec::new() });
    Capture::start(display.id, 2, 2, 1, shared, true)
}


// ── VideoToolbox ──

struct Session(CFRetained<VTCompressionSession>);

/// The callback of the encoder, on a thread of VideoToolbox: `refcon` is the `Shared` of the source.
unsafe extern "C-unwind" fn encoded(refcon: *mut c_void, _frame: *mut c_void, status: i32, _flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer) {
    if status != 0 || sample.is_null() || refcon.is_null() {
        return;
    }
    let shared = unsafe { &*(refcon as *const Shared) };
    if let Some(unit) = unsafe { annex_b(&*sample) } {
        shared.latest.lock().unwrap().units.push(unit);
        shared.wake.notify_all();
    }
}

thread_local! {
    /// The bytes of an access unit of more than one piece, on a thread of VideoToolbox.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// An access unit of VideoToolbox - NAL units after their 4-byte lengths - in Annex B: start codes, the parameter sets in front
/// of a key frame. The units are copied once, into the buffer of the frame: a length and a start code are of the same size.
unsafe fn annex_b(sample: &CMSampleBuffer) -> Option<Unit> {
    let block = unsafe { sample.data_buffer() }?;
    SCRATCH.with_borrow_mut(|scratch| {
        let avcc = unsafe { bytes_of(&block, scratch) }?;
        // the units as far as they are whole; a key frame has a slice of an IDR picture
        let (mut end, mut keyframe) = (0, false);
        while end + 4 <= avcc.len() {
            let n = u32::from_be_bytes([avcc[end], avcc[end + 1], avcc[end + 2], avcc[end + 3]]) as usize;
            if end + 4 + n > avcc.len() {
                break;
            }
            keyframe |= n > 0 && avcc[end + 4] & 0x1f == 5;
            end += 4 + n;
        }
        let mut data = crate::video::buffer();
        data.reserve(end + 64);
        if keyframe {
            unsafe { parameter_sets(sample, &mut data) };
        }
        let mut at = data.len();
        data.extend_from_slice(&avcc[..end]);
        while at < data.len() {
            let n = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
            data[at..at + 4].copy_from_slice(&[0, 0, 0, 1]);
            at += 4 + n;
        }
        Some(Unit { data, keyframe })
    })
}

/// The parameter sets of the stream of `sample`, each after a start code.
unsafe fn parameter_sets(sample: &CMSampleBuffer, data: &mut Vec<u8>) {
    let Some(format) = (unsafe { sample.format_description() }) else { return };
    let mut count = 0usize;
    let _ = unsafe { CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&format, 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut count, std::ptr::null_mut()) };
    for i in 0..count {
        let (mut set, mut size) = (std::ptr::null::<u8>(), 0usize);
        if unsafe { CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&format, i, &mut set, &mut size, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 && !set.is_null() {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(set, size) });
        }
    }
}

fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) { unsafe { VTSessionSetProperty(session, key, Some(value)) }; }

/// The size of the video for a display of `width` × `height`: as it is when it fits `MAX_SIDE`, else scaled down; even.
fn fitted(width: u32, height: u32) -> (u32, u32) {
    let scale = (MAX_SIDE as f64 / width as f64).min(MAX_SIDE as f64 / height as f64).min(1.0);
    (((width as f64 * scale) as u32) & !1, ((height as f64 * scale) as u32) & !1)
}

/// The frames of one display, encoded.
pub struct Source {
    display:  Display,
    shared:   Arc<Shared>,
    _capture: Capture,
    session:  Session,
    /// The `Shared` the callback of the encoder is given: a reference of its own, let go after the session.
    refcon:   *const Shared,
    seen:     u64,
    frame:    i64,
    fps:      u32,
    width:    u32,
    height:   u32,
    key:      bool,
    pointer:  Pointer,
    announce: Option<String>,
}

// SAFETY: the source is used by the thread of the video only; the encoder is safe to call from any thread
unsafe impl Send for Source {}

impl Drop for Source {
    fn drop(&mut self) {
        unsafe {
            let _ = self.session.0.complete_frames(kCMTimeInvalid);
            self.session.0.invalidate();
            drop(Arc::from_raw(self.refcon));
        }
    }
}

impl Source {
    pub fn new(display: &Display, fps: u32, bitrate: u32) -> Result<Self, Error> {
        let (width, height) = fitted(display.width, display.height);
        let shared = Arc::new(Shared::default());
        let capture = Capture::start(display.id, width, height, fps, shared.clone(), false)?;
        let refcon = Arc::into_raw(shared.clone());
        unsafe {
            let hardware: &CFString = kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder;
            let spec = CFDictionary::<CFString, CFType>::from_slices(&[hardware], &[CFBoolean::new(true).as_ref()]);
            let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
            let status = VTCompressionSession::create(None, width as i32, height as i32, H264, Some(spec.as_opaque()), None, None, Some(encoded), refcon as *mut c_void, NonNull::from(&mut raw));
            let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
                drop(Arc::from_raw(refcon));
                return Err(format!("VideoToolbox makes no H.264 encoder ( {status} )").into());
            };
            let session = Session(CFRetained::from_raw(raw));
            let s = &*session.0;
            set(s, kVTCompressionPropertyKey_RealTime, CFBoolean::new(true));
            set(s, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Baseline_AutoLevel);
            set(s, kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false));
            set(s, kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i32(bitrate as i32));
            set(s, kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::new_i32((fps * 10) as i32));
            set(s, kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_i32(fps as i32));
            Ok(Self {
                display: display.clone(),
                shared,
                _capture: capture,
                session,
                refcon,
                seen: 0,
                frame: 0,
                fps,
                width,
                height,
                key: true,
                pointer: Pointer::default(),
                announce: Some(format!("{}×{} → {width}×{height}, VideoToolbox", display.width, display.height)),
            })
        }
    }

    /// The size of the display in pixels.
    pub fn width(&self) -> u32 { self.display.width }
    pub fn height(&self) -> u32 { self.display.height }

    /// The pointer is in the frames: none beside them.
    pub fn pointer(&mut self) -> &mut Pointer { &mut self.pointer }

    /// Waits for the next frame, `wait` at most; an access unit encoded ends the wait too, for `poll` to take it.
    pub fn next(&mut self, wait: Duration) -> Result<Next, Error> {
        let latest = self.shared.latest.lock().unwrap();
        let seen = self.seen;
        let (latest, _) = self.shared.wake.wait_timeout_while(latest, wait, |l| l.seq == seen && l.units.is_empty()).unwrap();
        if latest.seq == self.seen {
            return Ok(Next::Nothing);
        }
        self.seen = latest.seq;
        Ok(Next::Frame)
    }

    pub fn size(&self) -> (u32, u32) { (self.width, self.height) }

    /// Frames the encoder may hold at once.
    pub fn ahead(&self) -> usize { 2 }

    pub fn announce(&mut self) -> Option<String> { self.announce.take() }

    pub fn key_frame(&mut self) { self.key = true; }

    pub fn set_bitrate(&mut self, bitrate: u32) { set(&self.session.0, unsafe { kVTCompressionPropertyKey_AverageBitRate }, &CFNumber::new_i32(bitrate as i32)); }

    /// The frame captured last goes to the encoder.
    pub fn submit(&mut self) -> Result<bool, Error> {
        let image = {
            let latest = self.shared.latest.lock().unwrap();
            let Some(image) = &latest.image else { return Ok(false) };
            image.0.clone()
        };
        let time = unsafe { CMTime::new(self.frame, self.fps as i32) };
        self.frame += 1;
        let force = std::mem::take(&mut self.key).then(|| {
            let key: &CFString = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            CFDictionary::<CFString, CFType>::from_slices(&[key], &[CFBoolean::new(true).as_ref()])
        });
        let status = unsafe { self.session.0.encode_frame(&image, time, kCMTimeInvalid, force.as_deref().map(|d| d.as_opaque()), std::ptr::null_mut(), std::ptr::null_mut()) };
        if status != 0 {
            return Err(format!("VideoToolbox did not take the frame ( {status} )").into());
        }
        Ok(true)
    }

    /// What the encoder has given since: the access units, in the order of the frames.
    pub fn poll(&mut self, out: &mut Vec<Unit>) -> Result<(), Error> {
        out.append(&mut self.shared.latest.lock().unwrap().units);
        Ok(())
    }
}
