// The video on Linux. X11: a monitor read by MIT-SHM when DAMAGE tells the screen changed, the pointer by XFIXES. Wayland: the
// frames of a stream of the portal by PipeWire, the pointer beside them when the compositor sends it so, else drawn into them.
// H.264 by openh264.

// without the feature `native` the capture is of X11 only: a `let` of its kind cannot fail then
#![allow(irrefutable_let_patterns)]

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::{ConnectionExt as _, CursorNotifyMask};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use super::h264::Encoder;
use super::{portal, wayland, x11};
use crate::video::{Error, Next, Pointer, Unit};

/// A display of the machine: its place in the virtual screen.
#[derive(Clone, Debug)]
pub struct Display {
    pub screen:  u8,
    pub x:       i32,
    pub y:       i32,
    pub width:   u32,
    pub height:  u32,
    pub name:    String,
    pub primary: bool,
    /// Wayland: the node of PipeWire of its stream. X11: the outputs of RandR.
    pub(super) node:    u32,
    pub(super) outputs: Vec<u32>,
}

/// The displays: the monitors of the X server, or the streams the user shared through the portal; 16 at most.
pub fn displays() -> Vec<Display> {
    if wayland() {
        let portal = match portal::portal() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("portal: the screen is not shared: {e}");
                return Vec::new();
            }
        };
        return portal
            .streams
            .iter()
            .take(16)
            .enumerate()
            .map(|(i, s)| {
                let (x, y) = s.position().unwrap_or((0, 0));
                let (w, h) = s.size().unwrap_or((0, 0));
                Display {
                    screen: i as u8,
                    x,
                    y,
                    width: w.max(0) as u32,
                    height: h.max(0) as u32,
                    name: s.id().map(String::from).unwrap_or_else(|| format!("Monitor {}", i + 1)),
                    primary: i == 0,
                    node: s.pipe_wire_node_id(),
                    outputs: Vec::new(),
                }
            })
            .collect();
    }
    x11::monitors()
        .into_iter()
        .take(16)
        .enumerate()
        .map(|(i, m)| Display {
            screen: i as u8,
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
            name: m.name,
            primary: m.primary,
            node: 0,
            outputs: m.outputs,
        })
        .collect()
}

/// The display of the pointer: the user works there. Wayland does not tell.
pub fn active(displays: &[Display]) -> Option<u8> {
    let x = x11::x().filter(|_| !wayland())?;
    let p = x.conn.query_pointer(x.root).ok()?.reply().ok()?;
    let (px, py) = (p.root_x as i32, p.root_y as i32);
    displays.iter().find(|d| px >= d.x && py >= d.y && px < d.x + d.width as i32 && py < d.y + d.height as i32).map(|d| d.screen)
}

/// Only the software encoder is there.
pub fn software_only() {}

pub fn startup() -> Result<(), Error> { Ok(()) }

/// What fails is told as it is: the video starts again by `Restart` only.
pub fn lost(_e: &Error) -> bool { false }

enum Capture {
    X11(Shm),
    #[cfg(feature = "native")]
    Wayland { frames: super::pw::Frames, seen: u64, pointer_seen: u64, frame: super::pw::Frame },
}

/// A segment of shared memory the X server writes the frames to, on a connection of its own: no other thread reads its events,
/// so the thread of the video sleeps in poll(2) until one comes.
struct Shm {
    conn:   RustConnection,
    root:   Window,
    xfixes: bool,
    seg:    u32,
    addr:   *mut u8,
    size:   usize,
    damage: u32,
    /// The screen changed since the last frame.
    dirty:  bool,
    /// The shape of the pointer changed since it was read.
    cursor: bool,
    serial: u32,
}

// SAFETY: the segment is used by the thread of the video only
unsafe impl Send for Shm {}

impl Drop for Shm {
    // the segment, the damage and the selection of the server go with the connection
    fn drop(&mut self) { unsafe { libc::shmdt(self.addr as *const libc::c_void) }; }
}

impl Shm {
    fn new(size: usize) -> Result<Self, Error> {
        let x = x11::x().ok_or("no X server")?;
        if !x.shm || !x.damage {
            return Err("the X server has no MIT-SHM or DAMAGE".into());
        }
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        // DAMAGE and XFIXES take no request before their version is told
        conn.damage_query_version(1, 1)?.reply()?;
        if x.xfixes {
            conn.xfixes_query_version(5, 0)?.reply()?;
            conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)?;
        }
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if id < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let addr = libc::shmat(id, std::ptr::null(), 0);
            // the segment goes when both sides let it go
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            if addr as isize == -1 {
                return Err(std::io::Error::last_os_error().into());
            }
            // made first: a failure below detaches the segment
            let mut shm = Self { conn, root, xfixes: x.xfixes, seg: 0, addr: addr as *mut u8, size, damage: 0, dirty: true, cursor: x.xfixes, serial: 0 };
            shm.seg = shm.conn.generate_id()?;
            shm.conn.shm_attach(shm.seg, id as u32, false)?.check()?;
            shm.damage = shm.conn.generate_id()?;
            shm.conn.damage_create(shm.damage, root, ReportLevel::NON_EMPTY)?.check()?;
            Ok(shm)
        }
    }

    /// Sleeps until the X server sends something, `left` at most.
    fn wait(&self, left: Duration) -> Result<(), Error> {
        self.conn.flush()?;
        let mut fd = libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // whole milliseconds, rounded up: a wait of less would wake at once and spin
        let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        // an interruption is no error: the caller looks again
        unsafe { libc::poll(&mut fd, 1, ms) };
        Ok(())
    }
}

/// The frames of one display, encoded.
pub struct Source {
    display:  Display,
    capture:  Capture,
    encoder:  Option<Encoder>,
    ready:    Vec<Unit>,
    fps:      u32,
    bitrate:  u32,
    pointer:  Pointer,
    announce: Option<String>,
}

impl Source {
    pub fn new(display: &Display, fps: u32, bitrate: u32) -> Result<Self, Error> {
        let capture = if wayland() {
            #[cfg(feature = "native")]
            {
                let portal = portal::portal()?;
                let fd = portal::pipewire(&portal)?;
                Capture::Wayland { frames: super::pw::Frames::start(fd, display.node)?, seen: 0, pointer_seen: 0, frame: super::pw::Frame::default() }
            }
            #[cfg(not(feature = "native"))]
            return Err("the host is built without PipeWire: the feature `native`".into());
        } else {
            Capture::X11(Shm::new(display.width as usize * display.height as usize * 4)?)
        };
        Ok(Self { display: display.clone(), capture, encoder: None, ready: Vec::new(), fps, bitrate, pointer: Pointer::default(), announce: None })
    }

    /// The size of the display in pixels: where the pointer is told in.
    pub fn width(&self) -> u32 { self.display.width }
    pub fn height(&self) -> u32 { self.display.height }

    pub fn pointer(&mut self) -> &mut Pointer { &mut self.pointer }

    /// Waits for the next change of the display, `wait` at most.
    pub fn next(&mut self, wait: Duration) -> Result<Next, Error> {
        match &mut self.capture {
            Capture::X11(shm) => {
                let until = Instant::now() + wait;
                loop {
                    while let Some(event) = shm.conn.poll_for_event()? {
                        match event {
                            Event::DamageNotify(_) => shm.dirty = true,
                            Event::XfixesCursorNotify(_) => shm.cursor = true,
                            _ => {}
                        }
                    }
                    let now = Instant::now();
                    if shm.dirty || shm.cursor || now >= until {
                        break;
                    }
                    shm.wait(until - now)?;
                }
                let moved = Self::pointer_of_x11(shm, &self.display, &mut self.pointer)?;
                if !shm.dirty {
                    return Ok(if moved { Next::Pointer } else { Next::Nothing });
                }
                shm.dirty = false;
                shm.conn.damage_subtract(shm.damage, x11rb::NONE, x11rb::NONE)?;
                let d = &self.display;
                shm.conn
                    .shm_get_image(shm.root, d.x as i16, d.y as i16, d.width as u16, d.height as u16, !0, ImageFormat::Z_PIXMAP.into(), shm.seg, 0)?
                    .reply()?;
                Ok(Next::Frame)
            }
            #[cfg(feature = "native")]
            Capture::Wayland { frames, seen, pointer_seen, .. } => {
                let (lock, wake) = &*frames.latest;
                let frame = lock.lock().unwrap();
                let (mut frame, _) = wake.wait_timeout_while(frame, wait, |f| f.seq == *seen && f.pointer_seq == *pointer_seen).unwrap();
                let moved = frame.pointer_seq != *pointer_seen;
                if moved {
                    *pointer_seen = frame.pointer_seq;
                    let p = &mut self.pointer;
                    (p.visible, p.x, p.y) = (frame.visible, frame.x, frame.y);
                    if let Some(shape) = frame.shape.take() {
                        p.shape = Some(shape);
                    }
                }
                if frame.seq == *seen {
                    return Ok(if moved { Next::Pointer } else { Next::Nothing });
                }
                *seen = frame.seq;
                Ok(Next::Frame)
            }
        }
    }

    /// The pointer of the X server: where it is, and its image once XFIXES told it changed. `true` - it moved or changed.
    fn pointer_of_x11(shm: &mut Shm, display: &Display, pointer: &mut Pointer) -> Result<bool, Error> {
        if !shm.xfixes {
            return Ok(false);
        }
        let p = shm.conn.query_pointer(shm.root)?.reply()?;
        let (px, py) = (p.root_x as i32 - display.x, p.root_y as i32 - display.y);
        let visible = px >= 0 && py >= 0 && px < display.width as i32 && py < display.height as i32;
        let moved = (visible, px, py) != (pointer.visible, pointer.x, pointer.y);
        pointer.visible = visible;
        pointer.x = px;
        pointer.y = py;
        if !std::mem::take(&mut shm.cursor) {
            return Ok(moved);
        }
        let c = shm.conn.xfixes_get_cursor_image()?.reply()?;
        if c.cursor_serial == shm.serial {
            return Ok(moved);
        }
        shm.serial = c.cursor_serial;
        // ARGB of 32 bits, alpha premultiplied: little endian, it is BGRA in memory
        let mut bgra = Vec::with_capacity(c.cursor_image.len() * 4);
        for p in &c.cursor_image {
            bgra.extend_from_slice(&p.to_le_bytes());
        }
        pointer.shape = Some((c.width as u32, c.height as u32, c.xhot as u32, c.yhot as u32, bgra));
        Ok(true)
    }

    /// The size of the video.
    pub fn size(&self) -> (u32, u32) {
        match &self.encoder {
            Some(e) => (e.width, e.height),
            None => (self.display.width & !1, self.display.height & !1),
        }
    }

    /// Frames the encoder may hold at once: it encodes as it is given one.
    pub fn ahead(&self) -> usize { 1 }

    /// What encodes, once, when it is made.
    pub fn announce(&mut self) -> Option<String> { self.announce.take() }

    pub fn key_frame(&mut self) {
        if let Some(e) = &mut self.encoder {
            e.key_frame();
        }
    }

    /// The running encoder takes the bitrate from its next frame; one that has not encoded yet is made again with it.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            if self.encoder.as_mut().is_some_and(|e| !e.set_bitrate(bitrate)) {
                self.encoder = None;
            }
        }
    }

    /// The frame captured last goes to the encoder.
    pub fn submit(&mut self) -> Result<bool, Error> {
        // the frame of Wayland is taken out of the lock first: PipeWire fills the next while this one is encoded
        let (bytes, width, height, rgb): (&[u8], u32, u32, bool) = match &mut self.capture {
            Capture::X11(shm) => (unsafe { std::slice::from_raw_parts(shm.addr, shm.size) }, self.display.width, self.display.height, false),
            #[cfg(feature = "native")]
            Capture::Wayland { frames, frame, .. } => {
                frames.take(frame);
                (frame.data.as_slice(), frame.width, frame.height, frame.rgb)
            }
        };
        if width == 0 || height == 0 {
            return Ok(false);
        }
        if self.encoder.as_ref().is_none_or(|e| e.input != (width, height)) {
            let e = Encoder::new(width, height, self.fps, self.bitrate)?;
            self.announce = Some(format!("{width}×{height} → {}×{}, openh264", e.width, e.height));
            self.encoder = Some(e);
        }
        self.encoder.as_mut().unwrap().encode(bytes, width as usize * 4, rgb, &mut self.ready)?;
        Ok(true)
    }

    /// What the encoder has given since: the access units, in the order of the frames.
    pub fn poll(&mut self, out: &mut Vec<Unit>) -> Result<(), Error> {
        out.append(&mut self.ready);
        Ok(())
    }
}
