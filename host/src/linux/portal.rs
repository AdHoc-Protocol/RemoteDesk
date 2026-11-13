// Wayland: one session of the portal RemoteDesktop with ScreenCast, for the life of the program - the monitors as streams of
// PipeWire, the keyboard, the pointer and touch. The first start asks the user of the machine; the answer is kept: the token the
// portal gives is saved in `portal.token` beside the program, and the next start shows no dialog.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions, StartOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream};
use ashpd::desktop::{PersistMode, Session};

use crate::video::Error;

pub struct Portal {
    pub proxy:   RemoteDesktop,
    pub session: Session<RemoteDesktop>,
    /// The monitors shared, in the order the user chose them.
    pub streams: Vec<Stream>,
}

static PORTAL: Mutex<Option<Arc<Portal>>> = Mutex::new(None);

fn token_file() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("portal.token"))).unwrap_or_else(|| PathBuf::from("portal.token"))
}

/// The session; made at the first call - the dialog of the portal waits for the user of the machine then.
pub fn portal() -> Result<Arc<Portal>, Error> {
    let mut slot = PORTAL.lock().unwrap();
    if let Some(p) = &*slot {
        return Ok(p.clone());
    }
    let portal = Arc::new(pollster::block_on(open())?);
    *slot = Some(portal.clone());
    let input = portal.clone();
    std::thread::Builder::new().name("portal input".into()).spawn(move || serve(&input)).expect("the thread of the input of the portal");
    Ok(portal)
}

/// The session, when it is open: the input does not open one.
pub fn opened() -> Option<Arc<Portal>> { PORTAL.lock().unwrap().clone() }

async fn open() -> Result<Portal, Error> {
    let token = std::fs::read_to_string(token_file()).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let proxy = RemoteDesktop::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::KEYBOARD | DeviceType::POINTER | DeviceType::TOUCHSCREEN)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await?;
    let screencast = Screencast::new().await?;
    // the pointer beside the frames, where the compositor can: its moves then encode no frame
    let cursor = if screencast.available_cursor_modes().await.is_ok_and(|m| m.contains(CursorMode::METADATA)) { CursorMode::METADATA } else { CursorMode::EMBEDDED };
    screencast
        .select_sources(&session, SelectSourcesOptions::default().set_sources(SourceType::MONITOR).set_multiple(true).set_cursor_mode(cursor))
        .await?;
    let selected = proxy.start(&session, None, StartOptions::default()).await?.response()?;
    if let Some(token) = selected.restore_token() {
        if let Err(e) = std::fs::write(token_file(), token) {
            eprintln!("portal: the allowance is not kept: {e}");
        }
    }
    let streams = selected.streams().to_vec();
    println!("portal: {} monitor(s) shared", streams.len());
    Ok(Portal { proxy, session, streams })
}

/// A connection to PipeWire that sees the streams of the session.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub fn pipewire(portal: &Portal) -> Result<OwnedFd, Error> {
    pollster::block_on(async {
        let screencast = Screencast::new().await?;
        Ok::<_, Error>(screencast.open_pipe_wire_remote(&portal.session, Default::default()).await?)
    })
}

// ── the input: the calls go to a thread of their own in order, the receiving thread does not wait for D-Bus; a failure is told
// once a call, the session goes on ──

#[derive(Clone, Copy)]
enum Input {
    Key(i32, KeyState),
    Keysym(i32, KeyState),
    PointerTo(u32, f64, f64),
    PointerBy(f64, f64),
    Button(i32, KeyState),
    Wheel(Axis, i32),
    TouchDown(u32, u32, f64, f64),
    TouchMotion(u32, u32, f64, f64),
    TouchUp(u32),
}

/// The calls not made yet. More are dropped: the portal does not answer.
static QUEUE: (Mutex<VecDeque<Input>>, Condvar) = (Mutex::new(VecDeque::new()), Condvar::new());
const QUEUED_MAX: usize = 1024;

/// A call queued: a motion right after another of the same pointer or touch takes its place - the portal is given the latest.
fn queue(input: Input) {
    let (lock, wake) = &QUEUE;
    let mut queue = lock.lock().unwrap();
    let merged = match (queue.back_mut(), input) {
        (Some(Input::PointerTo(s, x, y)), Input::PointerTo(stream, nx, ny)) if *s == stream => {
            (*x, *y) = (nx, ny);
            true
        }
        (Some(Input::PointerBy(dx, dy)), Input::PointerBy(ndx, ndy)) => {
            (*dx, *dy) = (*dx + ndx, *dy + ndy);
            true
        }
        (Some(Input::TouchMotion(s, t, x, y)), Input::TouchMotion(stream, slot, nx, ny)) if *s == stream && *t == slot => {
            (*x, *y) = (nx, ny);
            true
        }
        _ => false,
    };
    if merged || queue.len() >= QUEUED_MAX {
        return;
    }
    queue.push_back(input);
    drop(queue);
    wake.notify_one();
}

/// Makes the calls queued, one after another, for the life of the program.
fn serve(p: &Portal) {
    let (lock, wake) = &QUEUE;
    loop {
        let input = {
            let mut queue = wake.wait_while(lock.lock().unwrap(), |q| q.is_empty()).unwrap();
            queue.pop_front().unwrap()
        };
        let (what, result) = match input {
            Input::Key(evdev, state) => ("key", pollster::block_on(p.proxy.notify_keyboard_keycode(&p.session, evdev, state, Default::default()))),
            Input::Keysym(keysym, state) => ("text", pollster::block_on(p.proxy.notify_keyboard_keysym(&p.session, keysym, state, Default::default()))),
            Input::PointerTo(stream, x, y) => ("pointer", pollster::block_on(p.proxy.notify_pointer_motion_absolute(&p.session, stream, x, y, Default::default()))),
            Input::PointerBy(dx, dy) => ("pointer", pollster::block_on(p.proxy.notify_pointer_motion(&p.session, dx, dy, Default::default()))),
            Input::Button(button, state) => ("button", pollster::block_on(p.proxy.notify_pointer_button(&p.session, button, state, Default::default()))),
            Input::Wheel(axis, steps) => ("wheel", pollster::block_on(p.proxy.notify_pointer_axis_discrete(&p.session, axis, steps, Default::default()))),
            Input::TouchDown(stream, slot, x, y) => ("touch", pollster::block_on(p.proxy.notify_touch_down(&p.session, stream, slot, x, y, Default::default()))),
            Input::TouchMotion(stream, slot, x, y) => ("touch", pollster::block_on(p.proxy.notify_touch_motion(&p.session, stream, slot, x, y, Default::default()))),
            Input::TouchUp(slot) => ("touch", pollster::block_on(p.proxy.notify_touch_up(&p.session, slot, Default::default()))),
        };
        if let Err(e) = result {
            eprintln!("portal: {what}: {e}");
        }
    }
}

fn state(down: bool) -> KeyState { if down { KeyState::Pressed } else { KeyState::Released } }

// `_p`: the session is open - its thread makes the calls

pub fn key(_p: &Portal, evdev: i32, down: bool) { queue(Input::Key(evdev, state(down))); }

pub fn keysym(_p: &Portal, keysym: i32, down: bool) { queue(Input::Keysym(keysym, state(down))); }

pub fn pointer_to(_p: &Portal, stream: u32, x: f64, y: f64) { queue(Input::PointerTo(stream, x, y)); }

pub fn pointer_by(_p: &Portal, dx: f64, dy: f64) { queue(Input::PointerBy(dx, dy)); }

/// `button` - the code of the button of evdev: BTN_LEFT 0x110…
pub fn button(_p: &Portal, button: i32, down: bool) { queue(Input::Button(button, state(down))); }

/// Steps of the wheel: positive - down or right, as libinput counts.
pub fn wheel(_p: &Portal, vertical: bool, steps: i32) { queue(Input::Wheel(if vertical { Axis::Vertical } else { Axis::Horizontal }, steps)); }

pub fn touch_down(_p: &Portal, stream: u32, slot: u32, x: f64, y: f64) { queue(Input::TouchDown(stream, slot, x, y)); }

pub fn touch_motion(_p: &Portal, stream: u32, slot: u32, x: f64, y: f64) { queue(Input::TouchMotion(stream, slot, x, y)); }

pub fn touch_up(_p: &Portal, slot: u32) { queue(Input::TouchUp(slot)); }
