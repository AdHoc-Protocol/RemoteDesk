// The frames of a monitor of the portal by PipeWire: a thread of its own runs the loop of PipeWire and keeps the last frame, BGRx or
// RGBx, for the capture to take. The buffers are of memory the host maps ( MAP_BUFFERS, no DMA-BUF is offered ). The pointer comes
// beside the frames, as metadata of the buffers, when the compositor sends it so: a move of it encodes no frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use crate::video::Error;

/// The last frame: BGRx or RGBx, `width * 4` a row.
#[derive(Default)]
pub struct Frame {
    pub data:   Vec<u8>,
    pub width:  u32,
    pub height: u32,
    /// The red and the blue are in the order of RGBx.
    pub rgb:    bool,
    /// Grows with every frame.
    pub seq:    u64,
    /// The pointer as the metadata told it last; `pointer_seq` grows with every change of it.
    pub pointer_seq: u64,
    pub visible: bool,
    pub x:       i32,
    pub y:       i32,
    /// A new shape not taken yet: width, height, hotspot, BGRA.
    pub shape:   Option<(u32, u32, u32, u32, Vec<u8>)>,
}

pub struct Frames {
    pub latest: Arc<(Mutex<Frame>, Condvar)>,
    stop:       Arc<AtomicBool>,
    thread:     Option<std::thread::JoinHandle<()>>,
}

impl Drop for Frames {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Frames {
    /// The frames of the stream `node` of the portal's connection `fd`.
    pub fn start(fd: std::os::fd::OwnedFd, node: u32) -> Result<Self, Error> {
        let latest = Arc::new((Mutex::new(Frame::default()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread = {
            let (latest, stop) = (latest.clone(), stop.clone());
            std::thread::Builder::new()
                .name("pipewire".into())
                .spawn(move || {
                    if let Err(e) = run(fd, node, &latest, &stop, &ready_tx) {
                        let _ = ready_tx.send(Err(e.to_string()));
                    }
                })
                .expect("the thread of PipeWire")
        };
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self { latest, stop, thread: Some(thread) }),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err("PipeWire did not connect the stream".into()),
        }
    }

    /// The last frame into `into`, when it is newer: the buffers are swapped under the lock, the frame is not copied - the
    /// encoder reads it while PipeWire fills the next.
    pub fn take(&self, into: &mut Frame) {
        let mut latest = self.latest.0.lock().unwrap();
        if latest.seq != into.seq {
            std::mem::swap(&mut latest.data, &mut into.data);
            into.width = latest.width;
            into.height = latest.height;
            into.rgb = latest.rgb;
            into.seq = latest.seq;
        }
    }
}

/// What the format of the stream is, as PipeWire settled it.
#[derive(Default)]
struct Settled {
    info:  spa::param::video::VideoInfoRaw,
    /// The red and the blue are in the order of RGBx.
    rgb:   bool,
    /// The buffer the next frame is copied to, outside the lock: it is swapped with the one of the last frame then.
    spare: Vec<u8>,
}

fn run(fd: std::os::fd::OwnedFd, node: u32, latest: &Arc<(Mutex<Frame>, Condvar)>, stop: &AtomicBool, ready: &std::sync::mpsc::Sender<Result<(), String>>) -> Result<(), Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(fd, None)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "rdp-host",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let shared = latest.clone();
    let _listener = stream
        .add_local_listener_with_user_data(Settled::default())
        .param_changed(|stream, settled, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            if settled.info.parse(param).is_ok() {
                use pw::spa::param::video::VideoFormat;
                settled.rgb = matches!(settled.info.format(), VideoFormat::RGBx | VideoFormat::RGBA);
                // the format is settled: the buffers are asked to carry the pointer too
                if let Some(meta) = cursor_meta() {
                    if let Some(pod) = Pod::from_bytes(&meta) {
                        let _ = stream.update_params(&mut [pod]);
                    }
                }
            }
        })
        .process(move |stream, settled| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            // the pointer first: a buffer may carry it alone, when only the pointer moved
            if let Some(cursor) = buffer.find_meta::<spa::buffer::meta::MetaCursor>() {
                if pointer(cursor, &shared) {
                    shared.1.notify_all();
                }
            }
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (offset, size, stride) = (d.chunk().offset() as usize, d.chunk().size() as usize, d.chunk().stride().max(0) as usize);
            let (w, h) = (settled.info.size().width, settled.info.size().height);
            let Some(bytes) = d.data() else { return };
            let row = w as usize * 4;
            let stride = if stride == 0 { row } else { stride };
            if w == 0 || h == 0 || size < stride * (h as usize - 1) + row || bytes.len() < offset + size {
                return;
            }
            let spare = &mut settled.spare;
            spare.resize(row * h as usize, 0);
            if stride == row {
                spare.copy_from_slice(&bytes[offset..][..row * h as usize]);
            } else {
                for (y, dst) in spare.chunks_exact_mut(row).enumerate() {
                    dst.copy_from_slice(&bytes[offset + y * stride..][..row]);
                }
            }
            let (lock, wake) = &*shared;
            let mut frame = lock.lock().unwrap();
            std::mem::swap(&mut frame.data, spare);
            frame.width = w;
            frame.height = h;
            frame.rgb = settled.rgb;
            frame.seq += 1;
            drop(frame);
            wake.notify_all();
        })
        .register()?;

    let format = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(pw::spa::param::format::FormatProperties::MediaType, Id, pw::spa::param::format::MediaType::Video),
        pw::spa::pod::property!(pw::spa::param::format::FormatProperties::MediaSubtype, Id, pw::spa::param::format::MediaSubtype::Raw),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle { width: 1920, height: 1080 },
            pw::spa::utils::Rectangle { width: 1, height: 1 },
            pw::spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 30, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pw::spa::pod::Value::Object(format))
        .map_err(|e| format!("the format of the stream: {e:?}"))?
        .0
        .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("the format of the stream")?];
    stream.connect(spa::utils::Direction::Input, Some(node), pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS, &mut params)?;
    let _ = ready.send(Ok(()));

    while !stop.load(Ordering::Acquire) {
        mainloop.loop_().iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
    let _ = stream.disconnect();
    Ok(())
}

/// The largest pointer the metadata is asked to carry, in pixels: what the page draws.
const CURSOR_MAX: usize = 256;

/// The parameter that asks the buffers for the metadata of the pointer, with room for its bitmap.
fn cursor_meta() -> Option<Vec<u8>> {
    use pw::spa::pod::{ChoiceValue, Object, Property, Value};
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id, SpaTypes};
    let room = |side: usize| (std::mem::size_of::<spa::sys::spa_meta_cursor>() + std::mem::size_of::<spa::sys::spa_meta_bitmap>() + side * side * 4) as i32;
    let meta = Value::Object(Object {
        type_:      SpaTypes::ObjectParamMeta.as_raw(),
        id:         pw::spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_META_type, Value::Id(Id(spa::sys::SPA_META_Cursor))),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Choice(ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: room(64), min: room(1), max: room(CURSOR_MAX) }))),
            ),
        ],
    });
    pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &meta).ok().map(|(cursor, _)| cursor.into_inner())
}

/// The pointer of the metadata into `shared`: true when it changed. A cursor of id 0 is none - hidden, or off this monitor.
fn pointer(cursor: &spa::buffer::meta::MetaCursor, shared: &(Mutex<Frame>, Condvar)) -> bool {
    let visible = cursor.is_valid();
    let at = cursor.position();
    let shape = if visible { cursor.bitmap().and_then(|b| bgra_of(b, cursor.hotspot())) } else { None };
    let mut frame = shared.0.lock().unwrap();
    if (visible, at.x, at.y) == (frame.visible, frame.x, frame.y) && shape.is_none() {
        return false;
    }
    frame.visible = visible;
    frame.x = at.x;
    frame.y = at.y;
    if shape.is_some() {
        frame.shape = shape;
    }
    frame.pointer_seq += 1;
    true
}

/// The bitmap of a pointer as BGRA, rows packed: of the formats the compositors send, none for another.
fn bgra_of(bitmap: &spa::buffer::meta::MetaBitmap, hotspot: spa::utils::Point) -> Option<(u32, u32, u32, u32, Vec<u8>)> {
    use pw::spa::param::video::VideoFormat;
    let size = bitmap.size();
    let (w, h) = (size.width as usize, size.height as usize);
    if w == 0 || h == 0 || w > CURSOR_MAX || h > CURSOR_MAX {
        return None;
    }
    let swap = match bitmap.format() {
        VideoFormat::RGBA | VideoFormat::RGBx => true,
        VideoFormat::BGRA | VideoFormat::BGRx => false,
        _ => return None,
    };
    let opaque = matches!(bitmap.format(), VideoFormat::RGBx | VideoFormat::BGRx);
    let data = bitmap.bitmap_data()?;
    let stride = bitmap.stride().unsigned_abs() as usize;
    if stride < w * 4 || data.len() < stride * (h - 1) + w * 4 {
        return None;
    }
    let mut bgra = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for p in data[y * stride..][..w * 4].chunks_exact(4) {
            let (r, b) = if swap { (p[0], p[2]) } else { (p[2], p[0]) };
            bgra.extend_from_slice(&[b, p[1], r, if opaque { 255 } else { p[3] }]);
        }
    }
    Some((w as u32, h as u32, hotspot.x.max(0) as u32, hotspot.y.max(0) as u32, bgra))
}
