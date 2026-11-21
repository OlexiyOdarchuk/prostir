//! Hardware decoding with MediaCodec straight into the compositor's surface.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use log::{error, info, warn};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use prostir_proto::state::{DisplayId, Gen};
use prostir_proto::stats::{Histogram, Period};
use prostir_proto::{ClientMsg, Codec};

use crate::net::{Feedback, Frame, Frames};

/// The stream a decoder decodes: its display and generation (see `StreamStart`).
#[derive(Debug, Clone, Copy)]
pub struct Stream {
    pub id: DisplayId,
    pub generation: Gen,
}

/// Decodes `frames` into `window` until the channel closes or `stop` is set.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    stream: Stream,
    codec: Codec,
    width: u32,
    height: u32,
    window: NativeWindow,
    frames: Frames,
    feedback: Feedback,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let id = stream.id.0;
    std::thread::Builder::new()
        .name(format!("decoder-{id}"))
        .spawn(move || {
            if let Err(e) = run(
                stream, codec, width, height, &window, &frames, &feedback, &stop,
            ) {
                error!("decoder {id}: {e:#}");
            }
            info!("decoder {id} stopped");
        })
        .expect("spawn decoder thread")
}

#[allow(clippy::too_many_arguments)]
fn run(
    stream: Stream,
    codec: Codec,
    width: u32,
    height: u32,
    window: &NativeWindow,
    frames: &Frames,
    feedback: &Feedback,
    stop: &AtomicBool,
) -> Result<()> {
    let mut timing = Timing {
        stream: Some(stream),
        feedback: Some(feedback.clone()),
        started: Some(Instant::now()),
        ..Default::default()
    };
    let mut gate = KeyframeGate::new(stream, feedback.clone());
    // A decoder error (a lost surface, a bad frame) recreates the decoder instead of
    // freezing the monitor; only repeated failures in a row give up.
    let mut failures = 0;
    loop {
        let decoder = start_decoder(codec, width, height, window)?;
        gate.wait();
        let result = (|| -> Result<()> {
            while !stop.load(Ordering::Relaxed) {
                let got = frames.recv_timeout(Duration::from_millis(4));
                if got.is_ok() {
                    timing.taken(frames.waiting());
                }
                match got {
                    Ok(frame) if gate.passes(&frame) => {
                        if !queue(&decoder, &frame, &mut timing)? {
                            gate.wait();
                        } else {
                            failures = 0;
                        }
                    }
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
                drain(&decoder, &mut timing)?;
                timing.report();
            }
            Ok(())
        })();
        let _ = decoder.stop();
        match result {
            Ok(()) => return Ok(()),
            Err(e) if failures < 5 => {
                failures += 1;
                warn!("decoder {}: {e:#}; starting it again", stream.id.0);
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    }
}

fn start_decoder(
    codec: Codec,
    width: u32,
    height: u32,
    window: &NativeWindow,
) -> Result<MediaCodec> {
    let mime = codec.mime();
    let decoder =
        MediaCodec::from_decoder_type(mime).with_context(|| format!("no decoder for {mime}"))?;
    let mut format = MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", width as i32);
    format.set_i32("height", height as i32);
    format.set_i32("max-input-size", (width * height).max(2 << 20) as i32);
    // Output each frame as soon as it is decoded instead of buffering for smooth playback.
    format.set_i32("low-latency", 1);
    format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
    format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
    format.set_i32("priority", 0);
    decoder
        .configure(&format, Some(window), MediaCodecDirection::Decoder)
        .context("configure decoder")?;
    decoder.start().context("start decoder")?;
    info!(
        "decoder {} started for {width}x{height}",
        decoder.name().unwrap_or_default()
    );
    Ok(decoder)
}

/// After a frame is lost, the frames that follow it until the next keyframe would
/// decode into garbage: skip them and ask the PC for a keyframe (not on every frame).
struct KeyframeGate {
    stream: Stream,
    feedback: Feedback,
    waiting: bool,
    asked: Option<Instant>,
}

impl KeyframeGate {
    fn new(stream: Stream, feedback: Feedback) -> Self {
        Self {
            stream,
            feedback,
            waiting: false,
            asked: None,
        }
    }

    fn wait(&mut self) {
        self.waiting = true;
        self.ask();
    }

    fn ask(&mut self) {
        if self
            .asked
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(300))
        {
            self.asked = Some(Instant::now());
            let _ = self.feedback.send(ClientMsg::RequestKeyframe {
                id: self.stream.id,
                generation: self.stream.generation,
            });
        }
    }

    fn passes(&mut self, frame: &Frame) -> bool {
        if frame.keyframe {
            self.waiting = false;
        } else if self.waiting {
            self.ask();
        }
        !self.waiting
    }
}

/// Hands a frame to the decoder; `false` when it had to be dropped.
fn queue(decoder: &MediaCodec, frame: &Frame, timing: &mut Timing) -> Result<bool> {
    for _ in 0..200 {
        match decoder.dequeue_input_buffer(Duration::from_millis(5))? {
            DequeuedInputBufferResult::Buffer(mut buffer) => {
                let dst = buffer.buffer_mut();
                if frame.data.len() > dst.len() {
                    warn!(
                        "frame of {} bytes does not fit the {} byte input buffer",
                        frame.data.len(),
                        dst.len()
                    );
                    decoder.queue_input_buffer(buffer, 0, 0, frame.pts_us, 0)?;
                    return Ok(false);
                }
                for (d, s) in dst.iter_mut().zip(&frame.data) {
                    d.write(*s);
                }
                decoder.queue_input_buffer(buffer, 0, frame.data.len(), frame.pts_us, 0)?;
                timing.queued(frame);
                return Ok(true);
            }
            // All input buffers busy: hand finished frames to the compositor to free one.
            DequeuedInputBufferResult::TryAgainLater => drain(decoder, timing)?,
        }
    }
    warn!(
        "decoder stalled, dropping a frame (keyframe: {})",
        frame.keyframe
    );
    Ok(false)
}

fn drain(decoder: &MediaCodec, timing: &mut Timing) -> Result<()> {
    loop {
        match decoder.dequeue_output_buffer(Duration::ZERO)? {
            DequeuedOutputBufferInfoResult::Buffer(buffer) => {
                timing.decoded(buffer.info().presentation_time_us() as u64);
                decoder.release_output_buffer(buffer, true)?;
            }
            DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(()),
            DequeuedOutputBufferInfoResult::OutputFormatChanged
            | DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
        }
    }
}

/// How often each decoder logs its timing.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Where time goes per frame: network arrival → decoder input → decoded output, and
/// how many frames wait for the decoder.
struct Timing {
    stream: Option<Stream>,
    /// Reports shown frames so the host can measure the whole trip.
    feedback: Option<Feedback>,
    /// pts → (received, queued, the frame's number) for frames inside the decoder.
    in_flight: HashMap<u64, (Instant, Instant, u32)>,
    /// When a shown frame was last reported to the PC.
    last_ack: Option<Instant>,
    /// When the stream started here, until its first picture is out (for the log).
    started: Option<Instant>,
    /// Received → handed to the decoder.
    wait: Histogram,
    /// Handed to the decoder → decoded.
    decode: Histogram,
    /// Frames still waiting behind each one taken: (sum, max, taken).
    queue: (u64, usize, u32),
    report: Period,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            stream: None,
            feedback: None,
            in_flight: HashMap::new(),
            last_ack: None,
            started: None,
            wait: Histogram::default(),
            decode: Histogram::default(),
            queue: (0, 0, 0),
            report: Period::new(REPORT_EVERY),
        }
    }
}

impl Timing {
    /// A frame was taken from the network with `behind` more waiting.
    fn taken(&mut self, behind: usize) {
        let (sum, max, n) = &mut self.queue;
        *sum += behind as u64;
        *max = (*max).max(behind);
        *n += 1;
    }

    fn queued(&mut self, frame: &Frame) {
        if self.in_flight.len() > 64 {
            self.in_flight.clear();
        }
        self.in_flight
            .insert(frame.pts_us, (frame.received, Instant::now(), frame.seq));
    }

    fn decoded(&mut self, pts_us: u64) {
        let Some((received, queued, seq)) = self.in_flight.remove(&pts_us) else {
            return;
        };
        if let (Some(started), Some(stream)) = (self.started.take(), self.stream) {
            info!(
                "stream {} {:?}: first picture {} ms after it started",
                stream.id.0,
                stream.generation,
                started.elapsed().as_millis()
            );
        }
        // Enough for the PC's latency figures: one frame every 100 ms, not every frame
        // as its own network packet.
        if let (Some(f), Some(stream)) = (&self.feedback, self.stream)
            && self
                .last_ack
                .is_none_or(|t| t.elapsed() >= Duration::from_millis(100))
        {
            self.last_ack = Some(Instant::now());
            let _ = f.send(ClientMsg::FrameShown {
                id: stream.id,
                generation: stream.generation,
                seq,
            });
        }
        let (wait, decode) = (queued - received, queued.elapsed());
        self.wait.add(wait);
        self.decode.add(decode);
        #[cfg(all(feature = "selftest", not(feature = "store")))]
        if let Some(stream) = self.stream {
            crate::selftest::decoded(stream.id.0, stream.generation.0, seq, wait, decode);
        }
    }

    fn report(&mut self) {
        if self.report.due(Instant::now()).is_none() {
            return;
        }
        let (sum, max, taken) = std::mem::take(&mut self.queue);
        let wait = std::mem::take(&mut self.wait);
        let decode = std::mem::take(&mut self.decode);
        // Quiet while no frames come (headset taken off, say).
        if taken == 0 && decode.count() == 0 {
            return;
        }
        info!(
            "decoder {}: {} frames decoded; net → decoder {}; decode {}; waiting for the decoder avg {:.1} max {max}; inside the decoder {}",
            self.stream.map_or(0, |s| s.id.0),
            decode.count(),
            wait.summary_ms(),
            decode.summary_ms(),
            sum as f64 / f64::from(taken.max(1)),
            self.in_flight.len()
        );
    }
}
