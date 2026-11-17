//! Plays PC sound received over the connection through AAudio (low-latency mode).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use log::{info, warn};
use ndk::audio::{
    AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode, AudioStream,
    AudioStreamBuilder,
};
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
            .spawn(move || match open() {
                Ok(stream) => play(&stream, &rx),
                Err(e) => warn!("no sound: {e}"),
            })
            .expect("spawn audio thread");
        Self { tx }
    }

    pub fn push(&self, pcm: Vec<u8>) {
        // Full means the player is far behind: drop rather than add latency.
        let _ = self.tx.try_send(pcm);
    }
}

fn open() -> ndk::audio::Result<AudioStream> {
    let stream = AudioStreamBuilder::new()?
        .direction(AudioDirection::Output)
        .format(AudioFormat::PCM_I16)
        .channel_count(AUDIO_CHANNELS as i32)
        .sample_rate(AUDIO_RATE as i32)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .sharing_mode(AudioSharingMode::Shared)
        .open_stream()?;
    // A few bursts of headroom absorb network jitter without much delay.
    let burst = stream.frames_per_burst().max(96);
    let _ = stream.set_buffer_size_in_frames(burst * 4);
    stream.request_start()?;
    info!(
        "sound: {} Hz, buffer {} frames",
        stream.sample_rate(),
        burst * 4
    );
    Ok(stream)
}

fn play(stream: &AudioStream, rx: &Receiver<Vec<u8>>) {
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
            let frames = (pcm.len() / FRAME_BYTES) as i32;
            // SAFETY: `pcm` holds `frames` interleaved i16 stereo frames.
            let written = unsafe { stream.write(pcm.as_ptr().cast(), frames, 50_000_000) };
            if let Err(e) = written {
                warn!("sound output: {e}");
                return;
            }
        }
    }
    let _ = stream.request_stop();
}
