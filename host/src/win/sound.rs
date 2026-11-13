// The sound on Windows: what the default output device plays ( WASAPI loopback ), converted by Windows to 48 kHz stereo 16-bit;
// the microphone of the page on the same device.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::RATE;
use crate::video::Error;

/// How long a wait for the device lasts at most: `stop` is looked at that often. Loopback tells nothing while nothing plays.
const WAIT_MS: u32 = 100;

fn format(rate: u32) -> WAVEFORMATEX {
    WAVEFORMATEX { wFormatTag: WAVE_FORMAT_PCM as u16, nChannels: 2, nSamplesPerSec: rate, nAvgBytesPerSec: rate * 4, nBlockAlign: 4, wBitsPerSample: 16, cbSize: 0 }
}

/// The event the device sets when it has sound to take or room to fill.
struct Event(HANDLE);

impl Event {
    fn of(client: &IAudioClient) -> windows::core::Result<Self> {
        unsafe {
            let event = Self(CreateEventW(None, false, false, None)?);
            client.SetEventHandle(event.0)?;
            Ok(event)
        }
    }

    fn wait(&self) { unsafe { WaitForSingleObject(self.0, WAIT_MS) }; }
}

impl Drop for Event {
    fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } }
}

fn output_client() -> windows::core::Result<IAudioClient> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        devices.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate::<IAudioClient>(CLSCTX_ALL, None)
    }
}

/// What the machine plays, to `deliver` - 48 kHz stereo, 16-bit little endian - until `stop`.
pub fn capture(stop: &AtomicBool, deliver: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
    unsafe {
        let client = output_client()?;
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, &format(RATE), None)?; // 100 ms
        let event = Event::of(&client)?;
        let source: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        while !stop.load(Ordering::Acquire) {
            event.wait();
            while source.GetNextPacketSize()? > 0 {
                let (mut at, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                source.GetBuffer(&mut at, &mut frames, &mut flags, None, None)?;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 {
                    deliver(std::slice::from_raw_parts(at, frames as usize * 4)); // silence is not sent
                }
                source.ReleaseBuffer(frames)?;
            }
        }
        let _ = client.Stop();
        Ok(())
    }
}

/// Plays what `fill` gives - stereo 16-bit at `rate`, the samples it wrote, 0 for none now - on the output of the machine, until
/// `stop`.
pub fn play(rate: u32, stop: &AtomicBool, fill: &mut dyn FnMut(&mut [i16]) -> usize) -> Result<(), Error> {
    unsafe {
        let client = output_client()?;
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format(rate), None)?; // 200 ms
        let event = Event::of(&client)?;
        let sink: IAudioRenderClient = client.GetService()?;
        let size = client.GetBufferSize()? as usize;
        client.Start()?;
        while !stop.load(Ordering::Acquire) {
            event.wait();
            let room = size - client.GetCurrentPadding()? as usize;
            if room == 0 {
                continue;
            }
            // filled in place: the frames written go, the rest of the room stays free
            let at = sink.GetBuffer(room as u32)? as *mut i16;
            let frames = fill(std::slice::from_raw_parts_mut(at, room * 2)) / 2;
            sink.ReleaseBuffer(frames as u32, 0)?;
        }
        let _ = client.Stop();
        Ok(())
    }
}
