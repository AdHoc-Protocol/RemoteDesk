// The sound on Android: what the phone plays, by AudioPlaybackCapture ( Android 10 ) over the MediaProjection of the screen - the app
// records it ( Host.soundStart ) and gives it here ( `deliver` ). Android gives the sound of the apps that let it be captured: media,
// games; never a call. The microphone of the page plays on the speaker of the phone ( AudioTrack of the app, Host.mic* ); the app
// does not let its own sound be captured, so it does not come back to the page.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::video::Error;

/// The pieces the app recorded, and the pieces that came back to be filled again: no allocation once enough go round.
struct Pieces {
    full:  SyncSender<Vec<u8>>,
    taken: Mutex<Receiver<Vec<u8>>>,
    empty: Mutex<Vec<Vec<u8>>>,
}

fn pieces() -> &'static Pieces {
    static PIECES: OnceLock<Pieces> = OnceLock::new();
    PIECES.get_or_init(|| {
        let (full, taken) = sync_channel(64);
        Pieces { full, taken: Mutex::new(taken), empty: Mutex::new(Vec::new()) }
    })
}

/// A capture runs: the app's pieces are taken.
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// A piece the app recorded: 48 kHz stereo, 16-bit little endian. Dropped when nothing captures, or the capture is behind.
pub fn deliver(data: &[u8]) {
    if !CAPTURING.load(Ordering::Acquire) {
        return;
    }
    let p = pieces();
    let mut piece = p.empty.lock().unwrap().pop().unwrap_or_default();
    piece.clear();
    piece.extend_from_slice(data);
    let _ = p.full.try_send(piece);
}

/// What the phone plays, to `deliver` - 48 kHz stereo, 16-bit little endian - until `stop`.
pub fn capture(stop: &AtomicBool, deliver: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
    let p = pieces();
    let taken = p.taken.lock().unwrap(); // one capture at a time
    while taken.try_recv().is_ok() {} // what an earlier one left
    CAPTURING.store(true, Ordering::Release);
    if !super::sound_start() {
        CAPTURING.store(false, Ordering::Release);
        return Err("the phone does not let its sound be captured".into());
    }
    while !stop.load(Ordering::Relaxed) {
        match taken.recv_timeout(Duration::from_millis(100)) {
            Ok(piece) => {
                deliver(&piece);
                p.empty.lock().unwrap().push(piece);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    CAPTURING.store(false, Ordering::Release);
    super::sound_stop();
    Ok(())
}

/// The microphone of the page on the speaker of the phone - `rate` Hz stereo, 16-bit - until `stop`. The writes of the app wait while
/// its buffer is full: they set the pace.
pub fn play(rate: u32, stop: &AtomicBool, fill: &mut dyn FnMut(&mut [i16]) -> usize) -> Result<(), Error> {
    if !super::mic_start(rate) {
        return Err("the phone made no player of the sound".into());
    }
    let mut piece = vec![0i16; rate as usize / 100 * 2]; // 10 ms
    while !stop.load(Ordering::Relaxed) {
        let n = fill(&mut piece);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        super::mic_write(&piece[..n]);
    }
    super::mic_stop();
    Ok(())
}
