// The sound on Linux, by PulseAudio ( PipeWire serves its protocol too ): what the default output plays - its monitor - and the
// microphone of the page on the default output. Built with the feature `native` only: libpulse is a library of C.

use std::sync::atomic::AtomicBool;

use crate::video::Error;

#[cfg(feature = "native")]
fn stream(record: bool, rate: u32) -> Result<libpulse_simple_binding::Simple, Error> {
    use libpulse_binding::def::BufferAttr;
    use libpulse_binding::sample::{Format, Spec};
    use libpulse_binding::stream::Direction;
    let spec = Spec { format: Format::S16le, channels: 2, rate };
    // 20 ms pieces: the sound goes on soon
    let piece = rate / 50 * 4;
    let attr = BufferAttr { maxlength: u32::MAX, tlength: piece * 3, prebuf: u32::MAX, minreq: u32::MAX, fragsize: piece };
    let (direction, device, name) = if record { (Direction::Record, Some("@DEFAULT_MONITOR@"), "the sound of the machine") } else { (Direction::Playback, None, "the microphone of the viewer") };
    Ok(libpulse_simple_binding::Simple::new(None, "rdp-host", direction, device, name, &spec, None, Some(&attr))?)
}

/// What the machine plays, to `deliver` - 48 kHz stereo, 16-bit little endian - until `stop`.
#[cfg(feature = "native")]
pub fn capture(stop: &AtomicBool, deliver: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
    use std::sync::atomic::Ordering;
    let source = stream(true, crate::audio::RATE)?;
    let mut piece = vec![0u8; crate::audio::RATE as usize / 50 * 4];
    while !stop.load(Ordering::Acquire) {
        source.read(&mut piece)?;
        deliver(&piece);
    }
    Ok(())
}

/// Plays what `fill` gives - stereo 16-bit at `rate`, the samples it wrote, 0 for none now - on the output of the machine, until
/// `stop`.
#[cfg(feature = "native")]
pub fn play(rate: u32, stop: &AtomicBool, fill: &mut dyn FnMut(&mut [i16]) -> usize) -> Result<(), Error> {
    use std::sync::atomic::Ordering;
    let sink = stream(false, rate)?;
    let mut buffer = vec![0i16; rate as usize / 50 * 2];
    while !stop.load(Ordering::Acquire) {
        let n = fill(&mut buffer);
        if n == 0 {
            crate::audio::wait_for_microphone(std::time::Duration::from_millis(20)); // the next packet of the page wakes it
            continue;
        }
        // SAFETY: the bytes of `n` samples, read only; i16 has no padding, and the targets of the host are little endian, as S16le
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, n.min(buffer.len()) * 2) };
        sink.write(bytes)?;
    }
    Ok(())
}

#[cfg(not(feature = "native"))]
pub fn capture(_stop: &AtomicBool, _deliver: &mut dyn FnMut(&[u8])) -> Result<(), Error> { Err("the host is built without sound: the feature `native`".into()) }

#[cfg(not(feature = "native"))]
pub fn play(_rate: u32, _stop: &AtomicBool, _fill: &mut dyn FnMut(&mut [i16]) -> usize) -> Result<(), Error> { Err("the host is built without sound: the feature `native`".into()) }
