// H.264 by openh264, in software: BGRx → I420 ( BT.709, limited range ) by the host, then the encoder - screen content, real time,
// no B-frames. A display larger than 3840 × 2160 is encoded at half its size: openh264 takes no larger frame. Built with the
// feature `native` only: openh264 is a library of C.

use crate::video::{Error, Unit};

/// Macroblocks of the largest frame: 3840 × 2160.
#[cfg(feature = "native")]
const MAX_MACROBLOCKS: u32 = 3840 * 2160 / 256;

#[cfg(feature = "native")]
pub struct Encoder {
    inner:      openh264::encoder::Encoder,
    pub width:  u32,
    pub height: u32,
    /// The size of the frames it takes.
    pub input:  (u32, u32),
    /// 1 or 2: the frame is halved.
    scale:      u32,
    yuv:        Vec<u8>,
    key:        bool,
    /// The target of the rate control, bits a second: the maximum of the layer is the same, as openh264-rs sets it.
    bitrate:    u32,
}

/// The options of openh264 for the bitrate, with their SBitrateInfo: openh264-sys2 is no dependency of the host.
#[cfg(feature = "native")]
const ENCODER_OPTION_BITRATE: std::os::raw::c_int = 5;
#[cfg(feature = "native")]
const ENCODER_OPTION_MAX_BITRATE: std::os::raw::c_int = 6;
#[cfg(feature = "native")]
const SPATIAL_LAYER_0: std::os::raw::c_int = 0;
#[cfg(feature = "native")]
const SPATIAL_LAYER_ALL: std::os::raw::c_int = 4;
#[cfg(feature = "native")]
#[repr(C)]
struct BitrateInfo {
    layer:   std::os::raw::c_int,
    bitrate: std::os::raw::c_int,
}

#[cfg(feature = "native")]
impl Encoder {
    /// For frames of `width` × `height`.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self, Error> {
        use openh264::encoder::{BitRate, EncoderConfig, FrameRate, IntraFramePeriod, UsageType, VuiConfig};
        let scale = if width * height / 256 > MAX_MACROBLOCKS { 2 } else { 1 };
        let (w, h) = ((width / scale) & !1, (height / scale) & !1);
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 10))
            .vui(VuiConfig::bt709());
        let inner = openh264::encoder::Encoder::with_api_config(openh264::OpenH264API::from_source(), config)?;
        Ok(Self { inner, width: w, height: h, input: (width, height), scale, yuv: vec![0; (w * h * 3 / 2) as usize], key: true, bitrate })
    }

    pub fn key_frame(&mut self) { self.key = true; }

    /// The target of the rate control from the next frame on: no new encoder, no key frame. `false` - not taken: the encoder has
    /// not encoded yet.
    pub fn set_bitrate(&mut self, bitrate: u32) -> bool {
        let bitrate = bitrate.clamp(1, i32::MAX as u32);
        // openh264 refuses a target above the maximum of the layer: a higher one raises the maximum first, a lower one lowers it after
        let order = if bitrate > self.bitrate { [(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0), (ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL)] } else { [(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL), (ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0)] };
        let api = unsafe { self.inner.raw_api() };
        for (option, layer) in order {
            let mut info = BitrateInfo { layer, bitrate: bitrate as std::os::raw::c_int };
            // SAFETY: the option reads the structure during the call only; the bitrate is a parameter the rate control follows
            if unsafe { api.set_option(option, &mut info as *mut BitrateInfo as *mut std::os::raw::c_void) } != 0 {
                return false;
            }
        }
        self.bitrate = bitrate;
        true
    }

    /// Encodes a frame of BGRx ( RGBx when `rgb` ), `stride` bytes a row, of the size the encoder was made for: the access unit
    /// goes to `out`.
    pub fn encode(&mut self, bgrx: &[u8], stride: usize, rgb: bool, out: &mut Vec<Unit>) -> Result<(), Error> {
        let (w, h) = (self.width as usize, self.height as usize);
        bgrx_to_i420(bgrx, stride, w, h, self.scale as usize, rgb, &mut self.yuv);
        let (luma, chroma) = self.yuv.split_at(w * h);
        let (u, v) = chroma.split_at(w * h / 4);
        let frame = openh264::formats::YUVSlices::new((luma, u, v), (w, h), (w, w / 2, w / 2));
        if std::mem::take(&mut self.key) {
            self.inner.force_intra_frame();
        }
        let stream = self.inner.encode(&frame)?;
        use openh264::encoder::FrameType;
        let keyframe = match stream.frame_type() {
            FrameType::IDR | FrameType::I => true,
            FrameType::P | FrameType::IPMixed => false,
            _ => return Ok(()), // skipped: nothing changed enough
        };
        let mut data = crate::video::buffer();
        stream.write_vec(&mut data);
        out.push(Unit { data, keyframe });
        Ok(())
    }
}

#[cfg(not(feature = "native"))]
pub struct Encoder {
    pub width:  u32,
    pub height: u32,
    pub input:  (u32, u32),
}

#[cfg(not(feature = "native"))]
impl Encoder {
    pub fn new(_width: u32, _height: u32, _fps: u32, _bitrate: u32) -> Result<Self, Error> { Err("the host is built without H.264: the feature `native`".into()) }
    pub fn key_frame(&mut self) {}
    pub fn set_bitrate(&mut self, _bitrate: u32) -> bool { true }
    pub fn encode(&mut self, _bgrx: &[u8], _stride: usize, _rgb: bool, _out: &mut Vec<Unit>) -> Result<(), Error> { Ok(()) }
}

/// BGRx → I420, BT.709 limited range: the luma of every pixel, the chroma of each 2 × 2 block averaged; `scale` 2 - the frame is
/// halved, the four pixels of a block averaged first. `rgb` - the source is RGBx. Each kind has a loop of its own over slices of
/// rows: no branch and no bounds check per pixel.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
fn bgrx_to_i420(bgrx: &[u8], stride: usize, width: usize, height: usize, scale: usize, rgb: bool, yuv: &mut [u8]) {
    if width < 2 || height < 2 {
        return;
    }
    match (scale == 2, rgb) {
        (false, false) => full::<false>(bgrx, stride, width, height, yuv),
        (false, true) => full::<true>(bgrx, stride, width, height, yuv),
        (true, false) => half::<false>(bgrx, stride, width, height, yuv),
        (true, true) => half::<true>(bgrx, stride, width, height, yuv),
    }
}

/// The red, green and blue of the pixel at `p[i..i + 4]`.
#[inline(always)]
fn px<const RGB: bool>(p: &[u8], i: usize) -> (i32, i32, i32) {
    let (r, b) = if RGB { (p[i], p[i + 2]) } else { (p[i + 2], p[i]) };
    (r as i32, p[i + 1] as i32, b as i32)
}

/// The average of the 2 × 2 pixels at `i` of the rows `p` and `q`.
#[inline(always)]
fn avg<const RGB: bool>(p: &[u8], q: &[u8], i: usize) -> (i32, i32, i32) {
    let (a, b, c, d) = (px::<RGB>(p, i), px::<RGB>(p, i + 4), px::<RGB>(q, i), px::<RGB>(q, i + 4));
    ((a.0 + b.0 + c.0 + d.0) >> 2, (a.1 + b.1 + c.1 + d.1) >> 2, (a.2 + b.2 + c.2 + d.2) >> 2)
}

#[inline(always)]
fn luma_of((r, g, b): (i32, i32, i32)) -> u8 { (16 + ((47 * r + 157 * g + 16 * b + 128) >> 8)) as u8 }

/// The chroma of the average of four pixels.
#[inline(always)]
fn chroma_of(a: (i32, i32, i32), b: (i32, i32, i32), c: (i32, i32, i32), d: (i32, i32, i32)) -> (u8, u8) {
    let (r, g, b) = ((a.0 + b.0 + c.0 + d.0) >> 2, (a.1 + b.1 + c.1 + d.1) >> 2, (a.2 + b.2 + c.2 + d.2) >> 2);
    ((128 + ((-26 * r - 87 * g + 112 * b + 128) >> 8)).clamp(0, 255) as u8, (128 + ((112 * r - 102 * g - 10 * b + 128) >> 8)).clamp(0, 255) as u8)
}

/// A block of 2 × 2 pixels encoded: `a`, `b` above, `c`, `d` below - four luma, one chroma of their average.
#[inline(always)]
fn block(a: (i32, i32, i32), b: (i32, i32, i32), c: (i32, i32, i32), d: (i32, i32, i32), y0: &mut [u8], y1: &mut [u8], u: &mut u8, v: &mut u8) {
    y0[0] = luma_of(a);
    y0[1] = luma_of(b);
    y1[0] = luma_of(c);
    y1[1] = luma_of(d);
    (*u, *v) = chroma_of(a, b, c, d);
}

/// Every pixel of the source: two of its rows give two rows of luma, each a loop of its own that vectorizes, then one of chroma
/// while the rows are in the cache.
fn full<const RGB: bool>(bgrx: &[u8], stride: usize, width: usize, height: usize, yuv: &mut [u8]) {
    let (luma, chroma) = yuv.split_at_mut(width * height);
    let (u_plane, v_plane) = chroma.split_at_mut(width * height / 4);
    let row = width * 4;
    let rows = luma.chunks_exact_mut(2 * width).zip(u_plane.chunks_exact_mut(width / 2)).zip(v_plane.chunks_exact_mut(width / 2));
    for (y, ((l, u), v)) in rows.enumerate() {
        let (l0, l1) = l.split_at_mut(width);
        let s0 = &bgrx[2 * y * stride..][..row];
        let s1 = &bgrx[(2 * y + 1) * stride..][..row];
        for (d, p) in l0.iter_mut().zip(s0.chunks_exact(4)) {
            *d = luma_of(px::<RGB>(p, 0));
        }
        for (d, p) in l1.iter_mut().zip(s1.chunks_exact(4)) {
            *d = luma_of(px::<RGB>(p, 0));
        }
        for (((p0, p1), u), v) in s0.chunks_exact(8).zip(s1.chunks_exact(8)).zip(u.iter_mut()).zip(v.iter_mut()) {
            (*u, *v) = chroma_of(px::<RGB>(p0, 0), px::<RGB>(p0, 4), px::<RGB>(p1, 0), px::<RGB>(p1, 4));
        }
    }
}

/// The source halved: four of its rows give two rows of luma and one of chroma.
fn half<const RGB: bool>(bgrx: &[u8], stride: usize, width: usize, height: usize, yuv: &mut [u8]) {
    let (luma, chroma) = yuv.split_at_mut(width * height);
    let (u_plane, v_plane) = chroma.split_at_mut(width * height / 4);
    let row = width * 8;
    let rows = luma.chunks_exact_mut(2 * width).zip(u_plane.chunks_exact_mut(width / 2)).zip(v_plane.chunks_exact_mut(width / 2));
    for (y, ((l, u), v)) in rows.enumerate() {
        let (l0, l1) = l.split_at_mut(width);
        let s0 = &bgrx[4 * y * stride..][..row];
        let s1 = &bgrx[(4 * y + 1) * stride..][..row];
        let s2 = &bgrx[(4 * y + 2) * stride..][..row];
        let s3 = &bgrx[(4 * y + 3) * stride..][..row];
        let sources = s0.chunks_exact(16).zip(s1.chunks_exact(16)).zip(s2.chunks_exact(16).zip(s3.chunks_exact(16)));
        let blocks = sources.zip(l0.chunks_exact_mut(2).zip(l1.chunks_exact_mut(2))).zip(u.iter_mut().zip(v.iter_mut()));
        for ((((p0, p1), (p2, p3)), (y0, y1)), (u, v)) in blocks {
            block(avg::<RGB>(p0, p1, 0), avg::<RGB>(p0, p1, 8), avg::<RGB>(p2, p3, 0), avg::<RGB>(p2, p3, 8), y0, y1, u, v);
        }
    }
}
