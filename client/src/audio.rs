//! Plays PC sound received over the connection through AAudio (low-latency mode).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use log::{info, warn};
use ndk_sys as ffi;
use prostir_proto::{AUDIO_CHANNELS, AUDIO_RATE};

/// Most audio kept waiting before older slices are dropped (10 ms each), so sound
/// never drifts behind the picture after a Wi-Fi hiccup.
const MAX_QUEUED: usize = 6;
const FRAME_BYTES: usize = 2 * AUDIO_CHANNELS as usize;

/// Plays until dropped.
pub struct Player {
    tx: SyncSender<Vec<u8>>,
}

impl Player {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::sync_channel(32);
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || match Stream::open() {
                Ok(stream) => play(&stream, &rx),
                Err(e) => warn!("no sound: AAudio error {e}"),
            })
            .expect("spawn audio thread");
        Self { tx }
    }

    pub fn push(&self, pcm: Vec<u8>) {
        // Full means the player is far behind: drop rather than add latency.
        let _ = self.tx.try_send(pcm);
    }
}

/// An AAudio output stream. Called through `ndk-sys` directly: `ndk` 0.9 reports a
/// successful write (the frame count) as an error.
struct Stream(*mut ffi::AAudioStream);

// SAFETY: AAudio streams may be used from any one thread at a time; this one is
// owned by the audio thread.
unsafe impl Send for Stream {}

impl Stream {
    fn open() -> Result<Self, i32> {
        // SAFETY: plain AAudio calls; the builder is deleted on every path.
        unsafe {
            let mut builder = std::ptr::null_mut();
            check(ffi::AAudio_createStreamBuilder(&mut builder))?;
            ffi::AAudioStreamBuilder_setDirection(builder, ffi::AAUDIO_DIRECTION_OUTPUT as i32);
            ffi::AAudioStreamBuilder_setFormat(builder, ffi::AAUDIO_FORMAT_PCM_I16);
            ffi::AAudioStreamBuilder_setChannelCount(builder, AUDIO_CHANNELS as i32);
            ffi::AAudioStreamBuilder_setSampleRate(builder, AUDIO_RATE as i32);
            ffi::AAudioStreamBuilder_setPerformanceMode(
                builder,
                ffi::AAUDIO_PERFORMANCE_MODE_LOW_LATENCY as i32,
            );
            ffi::AAudioStreamBuilder_setSharingMode(
                builder,
                ffi::AAUDIO_SHARING_MODE_SHARED as i32,
            );
            let mut stream = std::ptr::null_mut();
            let opened = ffi::AAudioStreamBuilder_openStream(builder, &mut stream);
            ffi::AAudioStreamBuilder_delete(builder);
            check(opened)?;
            // A few bursts of headroom absorb network jitter without much delay.
            let burst = ffi::AAudioStream_getFramesPerBurst(stream).max(96);
            ffi::AAudioStream_setBufferSizeInFrames(stream, burst * 4);
            let s = Self(stream);
            check(ffi::AAudioStream_requestStart(stream))?;
            info!(
                "sound: {} Hz, buffer {} frames",
                ffi::AAudioStream_getSampleRate(stream),
                burst * 4
            );
            Ok(s)
        }
    }

    /// Writes interleaved i16 frames, waiting up to 50 ms for room.
    fn write(&self, pcm: &[u8]) -> Result<(), i32> {
        let frames = (pcm.len() / FRAME_BYTES) as i32;
        // SAFETY: `pcm` holds `frames` whole frames; the stream is open.
        let n = unsafe { ffi::AAudioStream_write(self.0, pcm.as_ptr().cast(), frames, 50_000_000) };
        check(n)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream was opened by `Stream::open` and is closed once.
        unsafe {
            ffi::AAudioStream_requestStop(self.0);
            ffi::AAudioStream_close(self.0);
        }
    }
}

fn check(result: i32) -> Result<(), i32> {
    if result < 0 { Err(result) } else { Ok(()) }
}

fn play(stream: &Stream, rx: &Receiver<Vec<u8>>) {
    let mut queue: VecDeque<Vec<u8>> = VecDeque::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(pcm) => queue.push_back(pcm),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(pcm) = rx.try_recv() {
            queue.push_back(pcm);
        }
        while queue.len() > MAX_QUEUED {
            queue.pop_front();
        }
        while let Some(pcm) = queue.pop_front() {
            if let Err(e) = stream.write(&pcm) {
                warn!("sound output: AAudio error {e}");
                return;
            }
        }
    }
}
