// H.264 of the frames: BGRA → NV12 ( BT.709, limited range ), then the H.264 encoder of Media Foundation - Baseline, low
// latency, a key frame when asked. The output is Annex B: start codes, the parameter sets in front of a key frame.

use windows::core::{Interface, Result};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use crate::video::Unit;

/// Starts COM and Media Foundation for the thread that encodes.
pub fn startup() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)
    }
}

pub struct Encoder {
    transform:  IMFTransform,
    codec:      Option<ICodecAPI>,
    pub width:  u32,
    pub height: u32,
    /// The sample of NV12 the frames are converted into: made once, the encoder takes it again for every frame.
    input:      IMFSample,
    pixels:     IMFMediaBuffer,
    /// The sample the access units come in, when the encoder does not give its own.
    output:     Option<IMFSample>,
    clock:      Clock,
    /// SPS and PPS of the output type: put in front of a key frame that comes without them.
    header:     Vec<u8>,
}

impl Encoder {
    /// `width` and `height` even; `bitrate` bits a second.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            let transform: IMFTransform = CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)?;

            set_types(&transform, width, height, fps, bitrate)?;
            let codec = transform.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                tune(codec, fps, bitrate);
            }
            let header = sequence_header(&transform);

            let info = transform.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;

            let size = width * height * 3 / 2;
            let pixels = MFCreateMemoryBuffer(size)?;
            pixels.SetCurrentLength(size)?;
            let input = MFCreateSample()?;
            input.AddBuffer(&pixels)?;
            input.SetSampleDuration(10_000_000 / fps.max(1) as i64)?;
            let output = if provides {
                None
            } else {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(width * height))?)?;
                Some(sample)
            };

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self { transform, codec, width, height, input, pixels, output, clock: Clock::new(), header })
        }
    }

    /// The next frame decodes alone.
    pub fn key_frame(&self) {
        if let Some(codec) = &self.codec {
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
        }
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        if let Some(codec) = &self.codec {
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate));
            }
        }
    }

    /// Encodes a frame of BGRA, rows `pitch` bytes apart, presented at `present` ( QPC ): the access units it gives. The frame is
    /// converted straight into the sample of the encoder; with low latency the encoder is done with it once it asks for more.
    pub fn encode(&mut self, bgra: &[u8], pitch: usize, present: i64, out: &mut Vec<Unit>) -> Result<()> {
        unsafe {
            let (width, height) = (self.width as usize, self.height as usize);
            let mut data = std::ptr::null_mut();
            self.pixels.Lock(&mut data, None, None)?;
            bgra_to_nv12(bgra, pitch, width, height, std::slice::from_raw_parts_mut(data, width * height * 3 / 2));
            self.pixels.Unlock()?;
            self.input.SetSampleTime(self.clock.time(present))?;
            self.transform.ProcessInput(0, &self.input, 0)?;
            self.drain(out)
        }
    }

    unsafe fn drain(&mut self, out: &mut Vec<Unit>) -> Result<()> {
        loop {
            // the sample of `output` is lent, not given: no reference is taken or let go for it
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample:    std::mem::transmute_copy(&self.output),
                dwStatus:   0,
                pEvents:    std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            drop(std::mem::ManuallyDrop::take(&mut buffers[0].pEvents));
            let given = if self.output.is_none() { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) } else { None };
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // the encoder settled its output type: take it as it is
                    let current = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &current, 0)?;
                    continue;
                }
                Err(e) => return Err(e),
            }
            let Some(sample) = given.as_ref().or(self.output.as_ref()) else { continue };
            out.push(unit_of(sample, &self.header)?);
        }
    }
}

/// The times of the frames for an encoder, in 100 ns from the first: when each was presented, so the rate control sees the real
/// time between frames at any frame rate.
pub(crate) struct Clock {
    frequency: i64,
    origin:    i64,
    last:      i64,
}

#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
    fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
}

impl Clock {
    pub fn new() -> Self {
        let mut frequency = 0;
        unsafe { QueryPerformanceFrequency(&mut frequency) };
        Self { frequency: frequency.max(1), origin: 0, last: -1 }
    }

    /// The time of the frame presented at `present` ( QPC ). A frame given again - a key frame asked of a still desktop - is of
    /// the moment it is given: the times only go forward.
    pub fn time(&mut self, present: i64) -> i64 {
        if self.last < 0 {
            self.origin = present;
        }
        let mut time = self.since(present);
        if time <= self.last {
            let mut now = 0;
            unsafe { QueryPerformanceCounter(&mut now) };
            time = self.since(now).max(self.last + 1);
        }
        self.last = time;
        time
    }

    fn since(&self, qpc: i64) -> i64 { ((qpc - self.origin) as i128 * 10_000_000 / self.frequency as i128) as i64 }
}

/// The access unit in an output sample of an encoder: the parameter sets put in front of a key frame that came without them.
pub(crate) unsafe fn unit_of(sample: &IMFSample, header: &[u8]) -> Result<Unit> {
    let buffer = sample.ConvertToContiguousBuffer()?;
    let mut data = std::ptr::null_mut();
    let mut length = 0u32;
    buffer.Lock(&mut data, None, Some(&mut length))?;
    let bytes = std::slice::from_raw_parts(data, length as usize);
    let (keyframe, sps) = leading(bytes);
    let mut unit = crate::video::buffer();
    if keyframe && !sps {
        unit.reserve(length as usize + header.len());
        unit.extend_from_slice(header);
    }
    unit.extend_from_slice(bytes);
    buffer.Unlock()?;
    Ok(Unit { data: unit, keyframe })
}

/// The SPS and PPS of the output type of `transform`, when it tells them.
pub(crate) unsafe fn sequence_header(transform: &IMFTransform) -> Vec<u8> {
    let mut header = Vec::new();
    if let Ok(current) = transform.GetOutputCurrentType(0) {
        let mut size = current.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).unwrap_or(0);
        if size > 0 {
            header.resize(size as usize, 0);
            let _ = current.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, Some(&mut size));
            header.truncate(size as usize);
        }
    }
    header
}

/// The media types of H.264 out and NV12 in, `width` x `height` at `fps`: Baseline, BT.709 limited range, progressive.
pub(crate) unsafe fn set_types(transform: &IMFTransform, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<()> {
    let output = MFCreateMediaType()?;
    output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
    output.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
    output.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1)?;
    output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1)?;
    output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
    output.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
    output.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
    output.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
    output.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    transform.SetOutputType(0, &output, 0)?;

    let input = MFCreateMediaType()?;
    input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    input.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
    input.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1)?;
    input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1)?;
    input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    transform.SetInputType(0, &input, 0)
}

/// Low latency, no B-frames, a key frame every 10 s and when asked.
pub(crate) unsafe fn tune(codec: &ICodecAPI, fps: u32, bitrate: u32) {
    let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
    let _ = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &variant_u32(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32));
    let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate));
    let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(fps * 10));
    let _ = codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0));
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// The NAL units of the Annex B bytes up to the first slice: ( the slice is of an IDR picture, an SPS came before it ). The scan
/// ends at the slice - the parameter sets are in front of it.
fn leading(bytes: &[u8]) -> (bool, bool) {
    let mut sps = false;
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i + 2] > 1 {
            i += 3; // no start code begins at i, i + 1 or i + 2
        } else if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            match bytes[i + 3] & 0x1F {
                1 => return (false, sps),
                5 => return (true, sps),
                7 => sps = true,
                _ => {}
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    (false, sps)
}

/// BGRA, rows `pitch` bytes apart → NV12 of `width` × `height` ( even ), BT.709 limited range: the luma of every pixel, the chroma
/// of each 2×2 block averaged.
pub fn bgra_to_nv12(bgra: &[u8], pitch: usize, width: usize, height: usize, nv12: &mut [u8]) {
    let (luma, chroma) = nv12.split_at_mut(width * height);
    // Y = 16 + ( 47 R + 157 G + 16 B ) / 256, U = 128 + ( -26 R - 87 G + 112 B ) / 256, V = 128 + ( 112 R - 102 G - 10 B ) / 256
    let y_of = |p: &[u8]| (16 + ((47 * p[2] as i32 + 157 * p[1] as i32 + 16 * p[0] as i32 + 128) >> 8)) as u8;
    for ((pair, lumas), uv) in luma.chunks_exact_mut(width * 2).enumerate().zip(chroma.chunks_exact_mut(width)) {
        let row0 = &bgra[pair * 2 * pitch..][..width * 4];
        let row1 = &bgra[(pair * 2 + 1) * pitch..][..width * 4];
        let (y0, y1) = lumas.split_at_mut(width);
        for ((((p0, p1), l0), l1), c) in
            row0.chunks_exact(8).zip(row1.chunks_exact(8)).zip(y0.chunks_exact_mut(2)).zip(y1.chunks_exact_mut(2)).zip(uv.chunks_exact_mut(2))
        {
            l0[0] = y_of(&p0[..4]);
            l0[1] = y_of(&p0[4..]);
            l1[0] = y_of(&p1[..4]);
            l1[1] = y_of(&p1[4..]);
            let b = (p0[0] as i32 + p0[4] as i32 + p1[0] as i32 + p1[4] as i32) >> 2;
            let g = (p0[1] as i32 + p0[5] as i32 + p1[1] as i32 + p1[5] as i32) >> 2;
            let r = (p0[2] as i32 + p0[6] as i32 + p1[2] as i32 + p1[6] as i32) >> 2;
            // 15 … 240 for any colour: no clamping
            c[0] = (128 + ((-26 * r - 87 * g + 112 * b + 128) >> 8)) as u8;
            c[1] = (128 + ((112 * r - 102 * g - 10 * b + 128) >> 8)) as u8;
        }
    }
}

pub(crate) fn variant_u32(value: u32) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_UI4;
        (*v.Anonymous.Anonymous).Anonymous.ulVal = value;
    }
    v
}

fn variant_bool(value: bool) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_BOOL;
        (*v.Anonymous.Anonymous).Anonymous.boolVal = if value { windows::Win32::Foundation::VARIANT_TRUE } else { windows::Win32::Foundation::VARIANT_FALSE };
    }
    v
}
