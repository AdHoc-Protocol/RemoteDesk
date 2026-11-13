// The sound of the session, RAW: 16-bit samples, little endian, the channels interleaved.
//
// The machine to the pages: what the machine plays, 48 kHz stereo, in packets of 20 ms, about 1.5 Mbit/s - one capture, each packet to
// every viewer with the sound on. Silence is not sent: a system that plays nothing gives nothing. The page that controls the machine to
// it: its microphone, played on the output of the machine. A small queue smooths the arrival; what is late by more than `MIC_MAX_MS`
// is dropped. How the sound is taken and played is the system's: `os::sound`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};

pub const RATE: u32 = 48_000;
/// Bytes of a packet to the page: 20 ms of 16-bit stereo.
const PACKET_BYTES: usize = RATE as usize / 50 * 4;
/// The microphone starts to play once this much is queued.
const MIC_START_MS: usize = 60;
const MIC_MAX_MS: usize = 300;

/// The sound of the machine being captured now: set to stop it. Its lock also orders the changes of `LISTENERS`.
static CAPTURE: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);
/// The microphone of the page playing on the machine now.
static MIC: Mutex<Option<Arc<Mic>>> = Mutex::new(None);

/// A viewer with the sound on.
struct Listener {
    conn:   Arc<Connection>,
    /// Packets given to its transport and not taken by it yet. A slow link holds them up: past `AHEAD` the sound is dropped for this
    /// viewer - it would come late, and hold back the video behind it.
    queued: AtomicU32,
}

/// The viewers with the sound on: read by every packet, without a lock.
static LISTENERS: ArcSwapOption<Vec<Arc<Listener>>> = ArcSwapOption::const_empty();

/// Buffers of packets the transport has sent: the next packets go in them - a packet costs no allocation.
static SPARE: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
const SPARE_KEPT: usize = (AHEAD as usize + 1) * 8; // as many as can wait for the most viewers

struct Mic {
    stop:  AtomicBool,
    /// Samples, stereo, as they came.
    queue:   Mutex<VecDeque<i16>>,
    /// Notified by each packet that comes: a player with nothing to play waits for it.
    arrived: Condvar,
    rate:    u32,
}

const AHEAD: u32 = 3;

pub fn register() {
    actor0::working::on_receive_d::Viewer_AudioControl.add(on_audio_control);
    actor0::working::on_receive_d::Viewer_MicrophoneControl.add(on_microphone_control);
    actor0::working::on_receive_d::AudioPacket.add(on_audio_packet);
    actor0::working::on_serialize_d::AudioPacket.add(on_sent);
}

/// A packet is on the wire: one less waits for the transport of its viewer, and its buffer takes the next packet.
fn on_sent(packet: &mut AudioPacket, conn: &Connection, _: &Actor0) {
    if let Some(listener) = LISTENERS.load().as_deref().and_then(|all| all.iter().find(|l| std::ptr::eq(Arc::as_ptr(&l.conn), conn))) {
        let _ = listener.queued.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)); // one of a listener before: none
    }
    if let Some(mut data) = packet.data.take() {
        data.clear();
        let mut spare = SPARE.lock().unwrap();
        if spare.len() < SPARE_KEPT {
            spare.push(data);
        }
    }
}

/// All zeros: silence, not sent.
fn silent(bytes: &[u8]) -> bool {
    bytes.chunks(64).all(|c| c.iter().fold(0u8, |a, &b| a | b) == 0)
}

/// The viewer on `conn` hears the sound of the machine: the capture starts with the first one.
pub fn start(conn: Arc<Connection>) {
    let mut capture = CAPTURE.lock().unwrap();
    let mut all = LISTENERS.load_full().map(|a| (*a).clone()).unwrap_or_default();
    if all.iter().any(|l| Arc::ptr_eq(&l.conn, &conn)) {
        return;
    }
    all.push(Arc::new(Listener { conn, queued: AtomicU32::new(0) }));
    LISTENERS.store(Some(Arc::new(all)));
    if capture.is_none() {
        let flag = Arc::new(AtomicBool::new(false));
        *capture = Some(flag.clone());
        std::thread::Builder::new().name("sound".into()).spawn(move || capture_sound(&flag)).expect("the thread of the sound");
    }
}

/// The viewer on `conn` hears it no more: the capture stops with the last one.
pub fn stop(conn: &Connection) {
    let mut capture = CAPTURE.lock().unwrap();
    let Some(mut all) = LISTENERS.load_full().map(|a| (*a).clone()) else { return };
    let before = all.len();
    all.retain(|l| !std::ptr::eq(Arc::as_ptr(&l.conn), conn));
    if all.len() == before {
        return;
    }
    let nobody = all.is_empty();
    LISTENERS.store(if nobody { None } else { Some(Arc::new(all)) });
    if nobody {
        if let Some(flag) = capture.take() {
            flag.store(true, Ordering::Release);
            say!("🔇", Quiet, "Sound off");
        }
    }
}

/// The microphone stops: the control passed, or the viewer that controls left.
pub fn mic_off() {
    if let Some(mic) = MIC.lock().unwrap().take() {
        mic.stop.store(true, Ordering::Release);
        say!("🎙️", Quiet, "Microphone off");
    }
}

fn on_audio_control(control: Viewer_AudioControl, conn: &Connection, _actor: &Actor0) {
    if !control.enable {
        return stop(conn);
    }
    if let Some(conn) = crate::session::viewer(conn) {
        start(conn);
    }
}

/// Only the microphone of the viewer that controls the machine plays.
fn on_microphone_control(control: Viewer_MicrophoneControl, conn: &Connection, _actor: &Actor0) {
    if !control.enable && crate::session::controls(conn) {
        mic_off();
    }
    // on: it starts with the first packet - the packet tells the rate
}

fn on_audio_packet(packet: &mut AudioPacket, conn: &Connection, _actor: &Actor0) {
    if !crate::session::controls(conn) {
        return;
    }
    let rate = RATE; // the microphone of the page: 48 kHz mono
    let Some(data) = packet.data.as_deref() else { return };

    let mic = {
        let mut slot = MIC.lock().unwrap();
        match &*slot {
            Some(mic) if mic.rate == rate && !mic.stop.load(Ordering::Acquire) => mic.clone(),
            _ => {
                if let Some(old) = slot.take() {
                    old.stop.store(true, Ordering::Release);
                }
                let mic = Arc::new(Mic { stop: AtomicBool::new(false), queue: Mutex::new(VecDeque::with_capacity(rate as usize)), arrived: Condvar::new(), rate });
                *slot = Some(mic.clone());
                let (player, conn) = (mic.clone(), crate::session::viewer(conn));
                std::thread::Builder::new()
                    .name("microphone".into())
                    .spawn(move || {
                        if let Err(e) = play(&player) {
                            say!("🎙️", Bad, "The microphone cannot play: {e}");
                            if let Some(conn) = conn {
                                actor0::working::transmitter::send_machine_microphone_error(Machine_MicrophoneError, &conn);
                            }
                            player.stop.store(true, Ordering::Release);
                        }
                    })
                    .expect("the thread of the microphone");
                say!("🎙️", News, "Microphone on: the viewer speaks through this computer");
                mic
            }
        }
    };

    let mut queue = mic.queue.lock().unwrap();
    // mono to both channels of the output
    queue.extend(data.chunks_exact(2).flat_map(|s| {
        let v = i16::from_le_bytes([s[0], s[1]]);
        [v, v]
    }));
    let max = rate as usize * MIC_MAX_MS / 1000 * 2;
    if queue.len() > max {
        let late = queue.len() - max;
        queue.drain(..late);
    }
    drop(queue);
    mic.arrived.notify_one();
}

/// A player of the microphone that has nothing to play waits here for the next packet of the page, `max` at most - a system whose
/// output does not ask for samples itself ( PulseAudio's simple API ).
#[cfg_attr(not(all(target_os = "linux", feature = "native")), allow(dead_code))] // the outputs of the other systems ask for the samples
pub fn wait_for_microphone(max: Duration) {
    let Some(mic) = MIC.lock().unwrap().clone() else { return std::thread::sleep(max) };
    let queue = mic.queue.lock().unwrap();
    let _ = mic.arrived.wait_timeout(queue, max);
}

/// The sound of the machine to the pages, until `stop`: the system gives it in pieces of its own size, a page gets 20 ms each - a copy
/// of its own, the transport of each connection takes its pack.
fn capture_sound(stop: &AtomicBool) {
    let mut out: Vec<u8> = Vec::with_capacity(PACKET_BYTES);
    let mut deliver = |bytes: &[u8]| {
        if silent(bytes) {
            return;
        }
        let mut bytes = bytes;
        while !bytes.is_empty() {
            let take = (PACKET_BYTES - out.len()).min(bytes.len());
            out.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if out.len() < PACKET_BYTES {
                continue;
            }
            for listener in LISTENERS.load().as_deref().into_iter().flatten() {
                if listener.queued.load(Ordering::Acquire) >= AHEAD {
                    continue; // late for this one: dropped
                }
                let mut data = SPARE.lock().unwrap().pop().unwrap_or_else(|| Vec::with_capacity(PACKET_BYTES));
                data.extend_from_slice(&out);
                listener.queued.fetch_add(1, Ordering::AcqRel);
                if !actor0::working::transmitter::send_audio_packet(AudioPacket { data: Some(data) }, &listener.conn) {
                    listener.queued.fetch_sub(1, Ordering::AcqRel); // the queue is full: this sound is late anyway
                }
            }
            out.clear();
        }
    };
    say!("🔊", News, "Sound on: the viewer hears this computer");
    if let Err(e) = crate::os::sound::capture(stop, &mut deliver) {
        say!("🔇", Bad, "The sound cannot be captured: {e}");
    }
}

/// The microphone of the page on the output of the machine, until `mic.stop`: it starts once `MIC_START_MS` are queued, and
/// waits for as much again when the queue runs dry.
fn play(mic: &Mic) -> Result<(), crate::video::Error> {
    let start = mic.rate as usize * MIC_START_MS / 1000 * 2;
    let mut playing = false;
    let mut fill = |dst: &mut [i16]| -> usize {
        let mut queue = mic.queue.lock().unwrap();
        if !playing {
            if queue.len() < start {
                return 0;
            }
            playing = true;
        }
        let n = dst.len().min(queue.len()) & !1;
        if n == 0 {
            playing = false;
            return 0;
        }
        for (d, s) in dst[..n].iter_mut().zip(queue.drain(..n)) {
            *d = s;
        }
        n
    };
    crate::os::sound::play(mic.rate, &mic.stop, &mut fill)
}
