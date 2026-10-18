//! The Windows microphone: the default capture endpoint through WASAPI, in shared mode. WASAPI
//! converts the device's format to the 16 kHz mono f32 the recognizers consume
//! (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`), so the samples go to the sink as they arrive. A
//! thread of its own owns the COM objects for the whole capture and polls for packets.
//!
//! Desktop apps get no microphone prompt on Windows: the system's privacy switch ("Let desktop
//! apps access your microphone") either allows every desktop app or none, and a blocked capture
//! fails with `E_ACCESSDENIED` when it opens.

use super::{audio, Authorization, Microphone, Sink, Stream, VoiceError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};

/// `WAVE_FORMAT_IEEE_FLOAT` (mmreg.h).
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// The shared-mode buffer, in 100 ns units: 200 ms.
const BUFFER_DURATION: i64 = 2_000_000;
/// How often the capture thread looks for packets; well inside the buffer.
const POLL: Duration = Duration::from_millis(10);
/// How long `open` waits for the capture thread to start the device.
const OPEN_WAIT: Duration = Duration::from_secs(5);

// HRESULTs that tell the user something they can act on.
const E_ACCESSDENIED: u32 = 0x8007_0005;
/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`: no capture endpoint at all.
const E_NOTFOUND: u32 = 0x8007_0490;
const AUDCLNT_E_DEVICE_INVALIDATED: u32 = 0x8889_0004;

pub struct WindowsMicrophone;

/// The format asked of WASAPI: `audio::SAMPLE_RATE` mono 32-bit float.
pub(crate) fn capture_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: 1,
        nSamplesPerSec: audio::SAMPLE_RATE,
        nAvgBytesPerSec: audio::SAMPLE_RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// What a failure to open the capture means to the user.
pub(crate) fn open_error(code: u32, message: &str) -> VoiceError {
    match code {
        E_ACCESSDENIED => VoiceError::MicrophoneDenied,
        E_NOTFOUND | AUDCLNT_E_DEVICE_INVALIDATED => VoiceError::NoMicrophone,
        _ => VoiceError::Failed(format!("The microphone could not start: {message}")),
    }
}

fn failed_to_start(error: impl std::fmt::Display) -> VoiceError {
    VoiceError::Failed(format!("The microphone could not start: {error}"))
}

impl Microphone for WindowsMicrophone {
    fn authorization(&self) -> Authorization {
        Authorization::Authorized
    }

    fn request_authorization(&self) -> Authorization {
        Authorization::Authorized
    }

    fn open(&self, sink: Sink) -> Result<Box<dyn Stream>, VoiceError> {
        let closed = Arc::new(AtomicBool::new(false));
        let (ready, started) = mpsc::channel();
        let flag = closed.clone();
        let thread = std::thread::Builder::new()
            .name("voice-wasapi".into())
            .spawn(move || capture(sink, flag, ready))
            .map_err(failed_to_start)?;
        let mut stream = WasapiStream { closed, thread: Some(thread) };
        match started.recv_timeout(OPEN_WAIT) {
            Ok(Ok(())) => Ok(Box::new(stream)),
            Ok(Err(error)) => {
                stream.close();
                Err(error)
            }
            Err(_) => {
                // The thread ends at its next check of the flag; never leave it capturing.
                stream.close();
                Err(failed_to_start("the audio device did not answer"))
            }
        }
    }
}

/// Ends COM on the capture thread, after the COM objects it owns are released.
struct Com;

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// Capture threads alive in this process (tests only): proves that closing ends the thread.
#[cfg(test)]
static CAPTURE_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
struct LiveCapture;

#[cfg(test)]
impl LiveCapture {
    fn new() -> Self {
        CAPTURE_THREADS.fetch_add(1, Ordering::SeqCst);
        LiveCapture
    }
}

#[cfg(test)]
impl Drop for LiveCapture {
    fn drop(&mut self) {
        CAPTURE_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The capture thread: starts the default capture endpoint, says whether that worked, then
/// delivers packets until the stream is closed or the device fails.
fn capture(sink: Sink, closed: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), VoiceError>>) {
    #[cfg(test)]
    let _live = LiveCapture::new();
    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        let _ = ready.send(Err(failed_to_start(error.message())));
        return;
    }
    let _com = Com;
    let (client, capture) = match start() {
        Ok(started) => started,
        Err(error) => {
            let _ = ready.send(Err(open_error(error.code().0 as u32, &error.message())));
            return;
        }
    };
    // `open` gave up waiting: release the device at once.
    let gave_up = ready.send(Ok(())).is_err();
    let result = if gave_up { Ok(()) } else { drain(&capture, &sink, &closed) };
    unsafe {
        let _ = client.Stop();
    }
    // The device went away or failed while recording; a close is not a failure.
    if result.is_err() && !closed.load(Ordering::SeqCst) {
        sink.failed(&VoiceError::Interrupted.to_string());
    }
}

fn start() -> windows::core::Result<(IAudioClient, IAudioCaptureClient)> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = capture_format();
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_DURATION,
            0,
            &format,
            None,
        )?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok((client, capture))
    }
}

/// Copies every waiting packet to the sink, then sleeps a poll, until the stream is closed.
fn drain(capture: &IAudioCaptureClient, sink: &Sink, closed: &AtomicBool) -> windows::core::Result<()> {
    while !closed.load(Ordering::SeqCst) {
        while unsafe { capture.GetNextPacketSize()? } > 0 {
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
            // Copied out before the buffer goes back to WASAPI. Each frame is one f32 (mono).
            let samples = if silent {
                vec![0.0; frames as usize]
            } else {
                unsafe { std::slice::from_raw_parts(data as *const f32, frames as usize) }.to_vec()
            };
            unsafe { capture.ReleaseBuffer(frames)? };
            if !samples.is_empty() {
                sink.audio(&samples);
            }
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

struct WasapiStream {
    closed: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stream for WasapiStream {
    fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // A failure reported from the capture thread closes the stream on that same thread
            // (the machine discards the recording there); it is already on its way out.
            if thread.thread().id() != std::thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::fake::{FakeClock, FakeRecognizer, Recorder};
    use crate::voice::{Voice, VoiceError};
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    #[test]
    fn wasapi_converts_to_16_khz_mono_float() {
        let format = capture_format();
        let (tag, channels, rate, bytes, align, bits, extra) = (
            format.wFormatTag,
            format.nChannels,
            format.nSamplesPerSec,
            format.nAvgBytesPerSec,
            format.nBlockAlign,
            format.wBitsPerSample,
            format.cbSize,
        );
        assert_eq!((tag, channels, rate, bytes, align, bits, extra), (3, 1, 16_000, 64_000, 4, 32, 0));
    }

    #[test]
    fn open_failures_say_what_the_user_can_do() {
        assert_eq!(open_error(0x8007_0005, "denied"), VoiceError::MicrophoneDenied);
        assert_eq!(open_error(0x8007_0490, "none"), VoiceError::NoMicrophone);
        assert_eq!(open_error(0x8889_0004, "gone"), VoiceError::NoMicrophone);
        assert_eq!(
            open_error(0x8889_0008, "Unsupported format"),
            VoiceError::Failed("The microphone could not start: Unsupported format".into())
        );
    }

    #[test]
    fn there_is_no_prompt_for_desktop_apps() {
        assert_eq!(WindowsMicrophone.authorization(), Authorization::Authorized);
        assert_eq!(WindowsMicrophone.request_authorization(), Authorization::Authorized);
    }

    /// Records the real default microphone for a moment and keeps nothing.
    #[test]
    #[ignore = "uses the real default microphone; run explicitly on a Windows machine with one"]
    fn the_default_microphone_streams_16_khz_until_stopped_and_releases_the_device() {
        let recognizer = Arc::new(FakeRecognizer::granted());
        *recognizer.final_text.lock().unwrap() = Some("done".into());
        let voice = Voice::new(Arc::new(WindowsMicrophone), recognizer.clone(), Arc::new(FakeClock(AtomicU64::new(0))));
        for round in 0..2 {
            let started = Instant::now();
            voice.start(None, Recorder::default().events()).unwrap();
            while recognizer.log.lock().unwrap().fed.len() < 8_000 {
                assert!(started.elapsed() < Duration::from_secs(5), "round {round}: half a second of audio arrives");
                std::thread::sleep(Duration::from_millis(10));
            }
            let elapsed = started.elapsed().as_secs_f64();
            let fed = recognizer.log.lock().unwrap().fed.len() as f64;
            // Converted to 16 kHz: at most what that rate delivers in the time it took.
            assert!(fed <= 16_000.0 * elapsed + 3_200.0, "round {round}: {fed} samples in {elapsed:.2} s");
            let stopping = Instant::now();
            assert_eq!(voice.stop().unwrap().text, "done");
            assert!(stopping.elapsed() < Duration::from_secs(2), "round {round}: stop releases the capture");
            recognizer.log.lock().unwrap().fed.clear();
        }
        // The second round opened the device again, so the first one released it.
        voice.start(None, Recorder::default().events()).unwrap();
        voice.cancel().unwrap();
    }

    #[test]
    #[ignore = "uses the real default microphone; run explicitly on a Windows machine with one"]
    fn cancel_ends_the_capture_thread() {
        let recognizer = Arc::new(FakeRecognizer::granted());
        let voice = Voice::new(Arc::new(WindowsMicrophone), recognizer, Arc::new(FakeClock(AtomicU64::new(0))));
        let threads = || CAPTURE_THREADS.load(Ordering::SeqCst);
        let before = threads();
        voice.start(None, Recorder::default().events()).unwrap();
        assert_eq!(threads(), before + 1, "one capture thread while recording");
        voice.cancel().unwrap();
        assert_eq!(threads(), before, "cancel waits for the capture thread to end");
        assert_eq!(voice.stop(), Err(VoiceError::NotRecording));
    }
}
