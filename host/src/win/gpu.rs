// H.264 on the GPU: the frame never leaves it. The video processor of D3D11 turns the copy of the desktop back as the user sees it,
// scales it to what the encoder takes and makes NV12 of it ( BT.709, limited range ) in one pass; the hardware encoder of Media
// Foundation on the same adapter reads that texture.
//
// The hardware encoder works asynchronously: it asks for input and tells of output by events. Its callback counts them on a thread
// of Media Foundation and wakes the thread of the video; a frame goes in when it asks, and what it gives is taken as it comes -
// with low latency and no B-frames, an access unit for every frame, while the next frame is already captured.

use std::ffi::c_void;
use std::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use windows::core::{IUnknown, IUnknown_Vtbl, Interface, Result, GUID, HRESULT};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_NOINTERFACE, E_NOTIMPL, HANDLE, RECT, S_OK};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

use super::capture::Duplicator;
use super::encode;
use crate::video::Unit;

/// The largest side the hardware encoders of H.264 take.
pub const MAX_SIDE: u32 = 4096;
/// NV12 textures: the frames the encoder may hold at once, and the one being made.
const POOL: usize = 4;

/// An NV12 texture of the pool: the processor writes it, the encoder reads it by its sample - made once, only its time changes.
struct Slot {
    texture: ID3D11Texture2D,
    view:    ID3D11VideoProcessorOutputView,
    sample:  IMFSample,
}

pub struct GpuEncoder {
    pub width:  u32,
    pub height: u32,
    /// The name of the encoder, as its maker gives it.
    pub name:   String,
    context:    ID3D11DeviceContext,
    video:      ID3D11VideoDevice,
    vctx:       ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor:  ID3D11VideoProcessor,
    /// The input views of the textures read last, by the textures: DXGI may give the desktop in more than one.
    input:      [(*mut c_void, Option<ID3D11VideoProcessorInputView>); 3],
    input_next: usize,
    /// The processor reads the texture of the desktop in place; when it cannot, a copy of it in `copy`.
    direct:     bool,
    copy:       Option<ID3D11Texture2D>,
    pool:       Vec<Slot>,
    next:       usize,
    /// `pool[next]` holds a frame converted and not given yet.
    pending:    bool,
    activate:   IMFActivate,
    transform:  IMFTransform,
    codec:      Option<ICodecAPI>,
    _manager:   IMFDXGIDeviceManager,
    wake:       IMFAsyncCallback,
    asked:      u32, // inputs the encoder asked for and did not get yet
    /// Frames given whose access units have not come yet.
    inside:     u32,
    /// The last frame offered was refused: the encoder had asked for none.
    refused:    bool,
    clock:      encode::Clock,
    header:     Vec<u8>,
}

/// The size of the video for a desktop of `width` × `height`: as it is when it fits `MAX_SIDE`, else scaled down to fit; even.
pub fn fitted(width: u32, height: u32) -> (u32, u32) {
    let scale = (MAX_SIDE as f64 / width as f64).min(MAX_SIDE as f64 / height as f64).min(1.0);
    (((width as f64 * scale) as u32) & !1, ((height as f64 * scale) as u32) & !1)
}

impl GpuEncoder {
    /// For the frames of `dup`: `source` is the size of its texture, as the panel shows it.
    pub fn new(dup: &Duplicator, source: (u32, u32), fps: u32, bitrate: u32) -> Result<Self> {
        let (width, height) = fitted(dup.width, dup.height);
        unsafe {
            let video: ID3D11VideoDevice = dup.device.cast()?;
            let vctx: ID3D11VideoContext = dup.context.cast()?;

            // the video processor: the panel's texture in, the desktop as the user sees it out, scaled, NV12
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate:   DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                InputWidth:       source.0,
                InputHeight:      source.1,
                OutputFrameRate:  DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                OutputWidth:      width,
                OutputHeight:     height,
                Usage:            D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video.CreateVideoProcessorEnumerator(&content)?;
            let processor = video.CreateVideoProcessor(&enumerator, 0)?;
            vctx.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            // DXGI turns the desktop clockwise onto the panel: it is turned on by the rest of the circle
            let turn = match dup.rotation {
                DXGI_MODE_ROTATION_ROTATE90 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_270),
                DXGI_MODE_ROTATION_ROTATE180 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_180),
                DXGI_MODE_ROTATION_ROTATE270 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_90),
                _ => None,
            };
            if let Some(turn) = turn {
                vctx.VideoProcessorSetStreamRotation(&processor, 0, true, turn);
            }
            let whole = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
            vctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&whole));
            vctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&whole));
            // RGB of the desktop: full range; the video: BT.709, limited range
            vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            vctx.VideoProcessorSetOutputColorSpace(&processor, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 2 | 1 << 4 });

            let mut pool = Vec::with_capacity(POOL);
            for _ in 0..POOL {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width:          width,
                    Height:         height,
                    MipLevels:      1,
                    ArraySize:      1,
                    Format:         DXGI_FORMAT_NV12,
                    SampleDesc:     DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage:          D3D11_USAGE_DEFAULT,
                    BindFlags:      D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags:      0,
                };
                let mut texture = None;
                dup.device.CreateTexture2D(&desc, None, Some(&mut texture))?;
                let texture = texture.unwrap();
                let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous:     D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut view = None;
                video.CreateVideoProcessorOutputView(&texture, &enumerator, &view_desc, Some(&mut view))?;
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false)?;
                if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                    if let Ok(length) = two_d.GetContiguousLength() {
                        buffer.SetCurrentLength(length)?;
                    }
                }
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleDuration(10_000_000 / fps.max(1) as i64)?;
                pool.push(Slot { texture, view: view.unwrap(), sample });
            }

            // the hardware encoder of the adapter that captures
            let (activate, name) = hardware_encoder(dup)?;
            let transform: IMFTransform = activate.ActivateObject()?;
            let attributes = transform.GetAttributes()?;
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(&dup.device, token)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            // the low latency is read when the types are set: it is told first
            let codec = transform.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                encode::tune(codec, fps, bitrate);
                // speed before quality: the frame is on the screen of the viewer sooner
                let _ = codec.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &encode::variant_u32(0));
            }
            encode::set_types(&transform, width, height, fps, bitrate)?;
            if let Some(codec) = &codec {
                encode::tune(codec, fps, bitrate);
            }
            let header = encode::sequence_header(&transform);
            let events: IMFMediaEventGenerator = transform.cast()?;
            let wake = Wake::new(events.clone())?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            events.BeginGetEvent(&wake, None)?;

            Ok(Self {
                width,
                height,
                name,
                context: dup.context.clone(),
                video,
                vctx,
                enumerator,
                processor,
                input: [(std::ptr::null_mut(), None), (std::ptr::null_mut(), None), (std::ptr::null_mut(), None)],
                input_next: 0,
                direct: true,
                copy: None,
                pool,
                next: 0,
                pending: false,
                activate,
                transform,
                codec,
                _manager: manager,
                wake,
                asked: 0,
                inside: 0,
                refused: false,
                clock: encode::Clock::new(),
                header,
            })
        }
    }

    pub fn key_frame(&self) {
        if let Some(codec) = &self.codec {
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &encode::variant_u32(1));
            }
        }
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        if let Some(codec) = &self.codec {
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &encode::variant_u32(bitrate));
            }
        }
    }

    /// Gives a frame to the encoder, presented at `present` ( QPC ): `texture` - the desktop as DXGI holds it; none - the frame
    /// converted before it was let go, or else the frame given last, again. `false` - the encoder asks for none now, the frame waits.
    pub fn submit(&mut self, texture: Option<&ID3D11Texture2D>, present: i64) -> Result<bool> {
        unsafe {
            self.asked += self.wake().need.swap(0, Ordering::Acquire);
            if self.asked == 0 {
                self.refused = true;
                return Ok(false);
            }
            self.refused = false;
            let slot = self.next;
            match texture {
                Some(texture) => self.convert(texture)?,
                None if self.pending => {}
                // a key frame asked of a still desktop
                None => self.context.CopyResource(&self.pool[slot].texture, &self.pool[(slot + POOL - 1) % POOL].texture),
            }
            self.pending = false;
            self.next = (slot + 1) % POOL;
            let sample = &self.pool[slot].sample;
            sample.SetSampleTime(self.clock.time(present))?;
            self.transform.ProcessInput(0, sample, 0)?;
            self.asked -= 1;
            self.inside += 1;
            Ok(true)
        }
    }

    /// The desktop in `texture` turned, scaled and made NV12 into `pool[next]`, read in place: the next frame to give.
    pub fn convert(&mut self, texture: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let view = match self.direct.then(|| self.input_view(texture)) {
                Some(Ok(view)) => view,
                _ => {
                    // the processor does not read the texture of DXGI: it reads a copy
                    self.direct = false;
                    let copy = self.copy_of(texture)?;
                    self.context.CopyResource(&copy, texture);
                    self.input_view(&copy)?
                }
            };
            // the view is lent to the stream, not given: no reference is taken or let go for it
            let stream = D3D11_VIDEO_PROCESSOR_STREAM { Enable: true.into(), pInputSurface: std::mem::transmute_copy(&self.input[view].1), ..Default::default() };
            self.vctx.VideoProcessorBlt(&self.processor, &self.pool[self.next].view, 0, &[stream])?;
            self.pending = true;
            Ok(())
        }
    }

    /// Frames are in the encoder, or it refused one and has not asked since: what it tells next is waited for.
    pub fn waits(&self) -> bool { self.inside > 0 || self.refused && self.asked == 0 }

    /// Waits until the encoder tells something, `ms` at most.
    pub fn wait(&self, ms: u32) {
        unsafe {
            WaitForSingleObject(self.wake().event, ms);
        }
    }

    /// What the encoder has given since: the access units, in the order of the frames.
    pub fn poll(&mut self, out: &mut Vec<Unit>) -> Result<()> {
        unsafe {
            let wake = &*(self.wake.as_raw() as *const Wake);
            if wake.broken.load(Ordering::Acquire) {
                return Err(windows::core::Error::new(E_FAIL, "the encoder stopped"));
            }
            self.asked += wake.need.swap(0, Ordering::Acquire);
            for _ in 0..wake.have.swap(0, Ordering::Acquire) {
                self.output(out)?;
            }
        }
        Ok(())
    }

    fn wake(&self) -> &Wake { unsafe { &*(self.wake.as_raw() as *const Wake) } }

    /// The input view of `texture`, by its place in `input`: made when the texture is not one of those seen last.
    unsafe fn input_view(&mut self, texture: &ID3D11Texture2D) -> Result<usize> {
        let raw = texture.as_raw();
        if let Some(i) = self.input.iter().position(|(r, v)| *r == raw && v.is_some()) {
            return Ok(i);
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC:        0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous:     D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut view = None;
        self.video.CreateVideoProcessorInputView(texture, &self.enumerator, &desc, Some(&mut view))?;
        let i = self.input_next;
        self.input_next = (i + 1) % self.input.len();
        self.input[i] = (raw, view);
        Ok(i)
    }

    /// The texture the desktop is copied to when the processor does not read it in place: of the size and format of `texture`.
    unsafe fn copy_of(&mut self, texture: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut d);
        if let Some(copy) = &self.copy {
            let mut have = D3D11_TEXTURE2D_DESC::default();
            copy.GetDesc(&mut have);
            if have.Width == d.Width && have.Height == d.Height && have.Format == d.Format {
                return Ok(copy.clone());
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width:          d.Width,
            Height:         d.Height,
            MipLevels:      1,
            ArraySize:      1,
            Format:         d.Format,
            SampleDesc:     DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage:          D3D11_USAGE_DEFAULT,
            BindFlags:      (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags:      0,
        };
        let mut copy = None;
        texture.GetDevice()?.CreateTexture2D(&desc, None, Some(&mut copy))?;
        self.copy = copy.clone();
        copy.ok_or_else(|| E_FAIL.into())
    }

    unsafe fn output(&mut self, out: &mut Vec<Unit>) -> Result<()> {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: std::mem::ManuallyDrop::new(None), dwStatus: 0, pEvents: std::mem::ManuallyDrop::new(None) }];
        let mut status = 0u32;
        let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
        drop(std::mem::ManuallyDrop::take(&mut buffers[0].pEvents));
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let current = self.transform.GetOutputAvailableType(0, 0)?;
                self.transform.SetOutputType(0, &current, 0)?;
                self.header = encode::sequence_header(&self.transform);
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        if let Some(sample) = sample {
            self.inside = self.inside.saturating_sub(1);
            out.push(encode::unit_of(&sample, &self.header)?);
        }
        Ok(())
    }
}

impl Drop for GpuEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            // the callback lets the encoder go: the request it has pending holds no more than the callback
            drop(self.wake().events.lock().unwrap_or_else(|e| e.into_inner()).take());
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// The callback of the events of the encoder: counts what it asks and gives, and wakes the thread of the video. A COM object of
/// its own, reference counted - it lives on while the encoder holds a request of it.
#[repr(C)]
struct Wake {
    vtable: *const IMFAsyncCallback_Vtbl,
    refs:   AtomicU32,
    /// Set at every event: what the thread of the video waits on.
    event:  HANDLE,
    /// METransformNeedInput and METransformHaveOutput not taken yet.
    need:   AtomicU32,
    have:   AtomicU32,
    /// The events stopped: an error of the encoder, or it is shut down.
    broken: AtomicBool,
    /// None once the encoder is let go.
    events: Mutex<Option<IMFMediaEventGenerator>>,
}

static WAKE: IMFAsyncCallback_Vtbl = IMFAsyncCallback_Vtbl {
    base__:        IUnknown_Vtbl { QueryInterface: Wake::query, AddRef: Wake::add_ref, Release: Wake::release },
    GetParameters: Wake::parameters,
    Invoke:        Wake::invoke,
};

impl Wake {
    unsafe fn new(events: IMFMediaEventGenerator) -> Result<IMFAsyncCallback> {
        let event = CreateEventW(None, false, false, None)?;
        let wake = Box::new(Wake {
            vtable: &WAKE,
            refs: AtomicU32::new(1),
            event,
            need: AtomicU32::new(0),
            have: AtomicU32::new(0),
            broken: AtomicBool::new(false),
            events: Mutex::new(Some(events)),
        });
        Ok(IMFAsyncCallback::from_raw(Box::into_raw(wake) as *mut c_void))
    }

    unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        if *iid == IUnknown::IID || *iid == IMFAsyncCallback::IID {
            Self::add_ref(this);
            *out = this;
            S_OK
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 { (*(this as *const Wake)).refs.fetch_add(1, Ordering::Relaxed) + 1 }

    unsafe extern "system" fn release(this: *mut c_void) -> u32 {
        let left = (*(this as *const Wake)).refs.fetch_sub(1, Ordering::Release) - 1;
        if left == 0 {
            fence(Ordering::Acquire);
            let wake = Box::from_raw(this as *mut Wake);
            let _ = CloseHandle(wake.event);
        }
        left
    }

    unsafe extern "system" fn parameters(_: *mut c_void, _: *mut u32, _: *mut u32) -> HRESULT { E_NOTIMPL }

    /// An event of the encoder: counted, the thread of the video woken, the next asked for.
    unsafe extern "system" fn invoke(this: *mut c_void, result: *mut c_void) -> HRESULT {
        let wake = &*(this as *const Wake);
        let events = wake.events.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(events) = events {
            let told = (|| -> Result<()> {
                let result = IMFAsyncResult::from_raw_borrowed(&result).ok_or(windows::core::Error::from(E_FAIL))?;
                let event = events.EndGetEvent(result)?;
                let kind = event.GetType()?;
                if kind == METransformNeedInput.0 as u32 {
                    wake.need.fetch_add(1, Ordering::Release);
                } else if kind == METransformHaveOutput.0 as u32 {
                    wake.have.fetch_add(1, Ordering::Release);
                } else if kind == MEError.0 as u32 {
                    return Err(E_FAIL.into());
                }
                events.BeginGetEvent(IMFAsyncCallback::from_raw_borrowed(&this), None)
            })();
            if told.is_err() {
                wake.broken.store(true, Ordering::Release);
            }
        }
        let _ = SetEvent(wake.event);
        S_OK
    }
}

/// The hardware H.264 encoder of the adapter `dup` captures on, and its name.
unsafe fn hardware_encoder(dup: &Duplicator) -> Result<(IMFActivate, String)> {
    let luid = dup.display.adapter.GetDesc1()?.AdapterLuid;
    let luid = (luid.HighPart as u64) << 32 | luid.LowPart as u64;
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output), &mut list, &mut count)?;
    let found: Vec<IMFActivate> = (0..count as usize).filter_map(|i| (*list.add(i)).take()).collect();
    CoTaskMemFree(Some(list as _));

    let adapter_of = |a: &IMFActivate| -> Option<u64> {
        let mut bytes = [0u8; 8];
        let mut size = 0u32;
        a.GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut bytes, Some(&mut size)).ok()?;
        Some(u64::from_le_bytes(bytes))
    };
    // the one on the adapter that captures; one that does not tell its adapter, when there is no other
    let chosen = found.iter().find(|a| adapter_of(a) == Some(luid)).or_else(|| found.iter().find(|a| adapter_of(a).is_none()));
    let Some(activate) = chosen.cloned() else { return Err(windows::core::Error::new(E_FAIL, "no hardware H.264 encoder on the adapter")) };
    let mut text = windows::core::PWSTR::null();
    let mut length = 0u32;
    let name = if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut length).is_ok() {
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(text.0, length as usize));
        CoTaskMemFree(Some(text.0 as _));
        name
    } else {
        String::from("a hardware encoder")
    };
    Ok((activate, name))
}
