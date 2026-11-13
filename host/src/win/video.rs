// The video on Windows: DXGI Desktop Duplication, H.264 by the encoder of the graphics card ( the frame stays on it ) or, when
// there is none, by the software encoder of Media Foundation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub use super::capture::{displays, Display};
use super::capture::Duplicator;
use super::encode::{self, Encoder};
use super::gpu::GpuEncoder;
use crate::video::{Error, Next, Pointer, Restart, Unit};

/// The hardware encoder failed once, or is not wanted: the software one from then on.
static NO_GPU: AtomicBool = AtomicBool::new(false);

/// The software encoder only.
pub fn software_only() { NO_GPU.store(true, Ordering::Relaxed); }

/// COM and Media Foundation for the thread of the video.
pub fn startup() -> Result<(), Error> { Ok(encode::startup()?) }

/// The desktop switched or the mode changed: the duplication is to be made again.
pub fn lost(e: &Error) -> bool { e.downcast_ref::<windows::core::Error>().is_some_and(super::capture::lost) }

/// The display of the window in front.
pub fn active(displays: &[Display]) -> Option<u8> {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe {
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        displays.iter().find(|d| d.x == info.rcMonitor.left && d.y == info.rcMonitor.top).map(|d| d.screen)
    }
}

enum Coder {
    Gpu(GpuEncoder),
    Cpu { encoder: Encoder, ready: Vec<Unit> },
}

/// The frames of one display, encoded.
pub struct Source {
    dup:      Duplicator,
    /// The hardware encoder is made at the first frame: it needs the texture of the frame.
    coder:    Option<Coder>,
    fps:      u32,
    bitrate:  u32,
    announce: Option<String>,
    /// A frame captured and not given to the encoder yet.
    unsent:   bool,
    _timer:   Timer,
}

/// While a display is captured, waits of a millisecond last a millisecond: the timeout of `AcquireNextFrame` - DXGI gives no handle
/// to wait on - sets when a frame held back for its interval goes, and how soon the encoder is looked at again.
struct Timer;

impl Timer {
    fn new() -> Self {
        unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        Timer
    }
}

impl Drop for Timer {
    fn drop(&mut self) { unsafe { windows::Win32::Media::timeEndPeriod(1) }; }
}

impl Source {
    pub fn new(display: &Display, fps: u32, bitrate: u32) -> Result<Self, Error> {
        let gpu = !NO_GPU.load(Ordering::Relaxed);
        let dup = Duplicator::new(display, gpu)?;
        let mut source = Self { dup, coder: None, fps, bitrate, announce: None, unsent: false, _timer: Timer::new() };
        if !gpu {
            // larger than H.264 takes - a page decodes none of it: halved as it is converted, as the GPU scales it
            let (dw, dh) = (source.dup.width, source.dup.height);
            let half = dw > super::gpu::MAX_SIDE || dh > super::gpu::MAX_SIDE;
            let (width, height) = if half { ((dw / 2) & !1, (dh / 2) & !1) } else { (dw & !1, dh & !1) };
            source.coder = Some(Coder::Cpu { encoder: Encoder::new(width, height, half, fps, bitrate)?, ready: Vec::new() });
            source.announce = Some(if half { format!("{dw}×{dh} → {width}×{height}, the software encoder") } else { format!("{width}×{height}, the software encoder") });
        }
        Ok(source)
    }

    /// The size of the desktop of the display, in pixels: where the pointer is told in.
    pub fn width(&self) -> u32 { self.dup.width }
    pub fn height(&self) -> u32 { self.dup.height }

    pub fn pointer(&mut self) -> &mut Pointer { &mut self.dup.pointer }

    /// Waits for the next change of the display, `wait` at most. While the hardware encoder works, what it tells wakes the thread
    /// instead: a change of the desktop is looked for without waiting, and a frame not given yet waits for it where DXGI holds it.
    pub fn next(&mut self, wait: Duration) -> Result<Next, Error> {
        let ms = wait.as_millis().max(1) as u32;
        let busy = match &self.coder {
            Some(Coder::Gpu(g)) if g.waits() => Some(g),
            _ => None,
        };
        if let Some(g) = busy {
            if self.unsent && self.dup.frame().is_some() {
                g.wait(ms);
                return Ok(Next::Nothing);
            }
        }
        let busy = busy.is_some();
        // the frame held is let go: one not given yet is converted for the encoder first
        if self.unsent {
            if let (Some(Coder::Gpu(g)), Some(texture)) = (&mut self.coder, self.dup.frame()) {
                g.convert(texture)?;
            }
        }
        let next = self.dup.next(if busy { 0 } else { ms })?;
        match next {
            Next::Frame => {
                self.unsent = true;
                if self.coder.is_none() {
                    self.gpu_encoder()?;
                }
            }
            Next::Nothing if busy => {
                if let Some(Coder::Gpu(g)) = &self.coder {
                    g.wait(ms);
                }
            }
            _ => {}
        }
        Ok(next)
    }

    /// The hardware encoder, for the texture of the first frame.
    fn gpu_encoder(&mut self) -> Result<(), Error> {
        let Some(texture) = self.dup.frame() else { return Ok(()) };
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        match GpuEncoder::new(&self.dup, (desc.Width, desc.Height), self.fps, self.bitrate) {
            Ok(g) => {
                self.announce = Some(format!("{}×{} → {}×{}, {}", self.dup.width, self.dup.height, g.width, g.height, g.name));
                self.coder = Some(Coder::Gpu(g));
                Ok(())
            }
            Err(e) => {
                NO_GPU.store(true, Ordering::Relaxed);
                Err(Box::new(Restart(format!("no hardware encoder ( {e} ): the software one"))))
            }
        }
    }

    /// The size of the video.
    pub fn size(&self) -> (u32, u32) {
        match &self.coder {
            Some(Coder::Gpu(g)) => (g.width, g.height),
            Some(Coder::Cpu { encoder, .. }) => (encoder.width, encoder.height),
            None => (self.dup.width & !1, self.dup.height & !1),
        }
    }

    /// Frames the encoder may hold at once.
    pub fn ahead(&self) -> usize { 1 }

    /// What encodes, once, when it is made.
    pub fn announce(&mut self) -> Option<String> { self.announce.take() }

    pub fn key_frame(&mut self) {
        match &self.coder {
            Some(Coder::Gpu(g)) => g.key_frame(),
            Some(Coder::Cpu { encoder, .. }) => encoder.key_frame(),
            None => {} // the first frame of an encoder decodes alone
        }
    }

    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.bitrate = bitrate;
        match &self.coder {
            Some(Coder::Gpu(g)) => g.set_bitrate(bitrate),
            Some(Coder::Cpu { encoder, .. }) => encoder.set_bitrate(bitrate),
            None => {}
        }
    }

    /// The frame captured last goes to the encoder: `false` - it is busy, the frame waits.
    pub fn submit(&mut self) -> Result<bool, Error> {
        let time = self.dup.time;
        let sent = match &mut self.coder {
            // the frame held by DXGI is read in place; one let go was converted before
            Some(Coder::Gpu(g)) => g.submit(if self.unsent { self.dup.frame() } else { None }, time)?,
            // the encoder may be narrower than the capture by an odd pixel: it reads `width` of every row
            Some(Coder::Cpu { encoder, ready }) => {
                self.dup.pixels(|bgra, pitch| encoder.encode(bgra, pitch, time, ready))??;
                true
            }
            None => false,
        };
        if sent {
            self.unsent = false;
        }
        Ok(sent)
    }

    /// What the encoder has given since: the access units, in the order of the frames.
    pub fn poll(&mut self, out: &mut Vec<Unit>) -> Result<(), Error> {
        match &mut self.coder {
            Some(Coder::Gpu(g)) => g.poll(out)?,
            Some(Coder::Cpu { ready, .. }) => out.append(ready),
            None => {}
        }
        Ok(())
    }
}
