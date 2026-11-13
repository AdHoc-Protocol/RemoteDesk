// The sound on macOS: what the Mac plays, by ScreenCaptureKit ( macOS 13 and later ); the microphone of the page by AVAudioEngine
// on the output of the Mac.

use std::sync::atomic::{AtomicBool, AtomicI16, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2_avf_audio::{AVAudioEngine, AVAudioFormat, AVAudioSourceNode};
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
use objc2::runtime::Bool;

use crate::video::Error;

/// What the Mac plays, to `deliver` - 48 kHz stereo, 16-bit little endian - until `stop`.
pub fn capture(stop: &AtomicBool, deliver: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
    // the pieces go back to be filled again: no allocation a piece once enough go round
    let (full, pieces) = sync_channel::<Vec<u8>>(64);
    let (back, empty) = sync_channel::<Vec<u8>>(66);
    let _capture = super::video::sound_capture(full, empty)?;
    while !stop.load(Ordering::Acquire) {
        match pieces.recv_timeout(Duration::from_millis(100)) {
            Ok(piece) => {
                deliver(&piece);
                let _ = back.try_send(piece);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Samples from `play` to the render block of the engine, which runs in real time: one writer, one reader, no lock.
struct Ring {
    samples: Box<[AtomicI16]>,
    /// The samples written and read since the start: their difference is what waits.
    written: AtomicUsize,
    read:    AtomicUsize,
}

impl Ring {
    /// Room for `capacity` samples at least.
    fn new(capacity: usize) -> Self {
        let samples = (0..capacity.next_power_of_two()).map(|_| AtomicI16::new(0)).collect();
        Self { samples, written: AtomicUsize::new(0), read: AtomicUsize::new(0) }
    }

    fn len(&self) -> usize { self.written.load(Ordering::Acquire).wrapping_sub(self.read.load(Ordering::Acquire)) }

    /// The writer: as much of `from` as there is room for.
    fn push(&self, from: &[i16]) {
        let (written, mask) = (self.written.load(Ordering::Relaxed), self.samples.len() - 1);
        let room = self.samples.len() - written.wrapping_sub(self.read.load(Ordering::Acquire));
        // whole frames of two samples: the reader takes them so
        let n = from.len().min(room) & !1;
        for (i, &s) in from[..n].iter().enumerate() {
            self.samples[(written + i) & mask].store(s, Ordering::Relaxed);
        }
        self.written.store(written.wrapping_add(n), Ordering::Release);
    }
}

/// Plays what `fill` gives - stereo 16-bit at `rate`, the samples it wrote, 0 for none now - on the output of the Mac, until
/// `stop`. The engine pulls from a ring of its own: `fill` is called here, every 5 ms.
pub fn play(rate: u32, stop: &AtomicBool, fill: &mut dyn FnMut(&mut [i16]) -> usize) -> Result<(), Error> {
    let ring = Arc::new(Ring::new(rate as usize));
    let pulled = ring.clone();
    let render = RcBlock::new(move |silence: std::ptr::NonNull<Bool>, _time: std::ptr::NonNull<AudioTimeStamp>, frames: u32, list: std::ptr::NonNull<AudioBufferList>| -> i32 {
        let list = unsafe { &mut *list.as_ptr() };
        let buffers = unsafe { std::slice::from_raw_parts_mut(list.mBuffers.as_mut_ptr(), list.mNumberBuffers as usize) };
        let (read, mask) = (pulled.read.load(Ordering::Relaxed), pulled.samples.len() - 1);
        let frames = frames as usize;
        // the frames that wait, as many as asked for at most; silence after them
        let ready = (pulled.written.load(Ordering::Acquire).wrapping_sub(read) / 2).min(frames);
        // the left channel to the first buffer, the right to the second
        for (channel, b) in buffers.iter_mut().take(2).enumerate() {
            if b.mData.is_null() {
                continue;
            }
            let out = unsafe { std::slice::from_raw_parts_mut(b.mData as *mut f32, frames) };
            for (i, o) in out[..ready].iter_mut().enumerate() {
                *o = pulled.samples[(read + 2 * i + channel) & mask].load(Ordering::Relaxed) as f32 / 32768.0;
            }
            out[ready..].fill(0.0);
        }
        pulled.read.store(read.wrapping_add(ready * 2), Ordering::Release);
        unsafe { *silence.as_ptr() = Bool::new(ready == 0) };
        0
    });
    unsafe {
        let engine = AVAudioEngine::new();
        let format = AVAudioFormat::initStandardFormatWithSampleRate_channels(AVAudioFormat::alloc(), rate as f64, 2).ok_or("no format of the sound")?;
        let node = AVAudioSourceNode::initWithFormat_renderBlock(AVAudioSourceNode::alloc(), &format, &*render as *const _ as *mut _);
        engine.attachNode(&node);
        engine.connect_to_format(&node, &engine.mainMixerNode(), Some(&format));
        engine.startAndReturnError().map_err(|e| e.localizedDescription().to_string())?;
        let mut piece = vec![0i16; rate as usize / 100 * 2];
        while !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
            loop {
                if ring.len() > rate as usize / 5 * 2 {
                    break; // 200 ms ahead: enough
                }
                let n = fill(&mut piece);
                if n == 0 {
                    break;
                }
                ring.push(&piece[..n]);
            }
        }
        engine.stop();
    }
    Ok(())
}
