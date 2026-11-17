//! Hardware decoding with MediaCodec straight into the compositor's surface.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use log::{debug, error, info, warn};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use prostir_proto::{ClientMsg, Codec};

use crate::net::{Feedback, Frame};

/// Decodes `frames` into `window` until the channel closes or `stop` is set.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    id: u8,
    codec: Codec,
    width: u32,
    height: u32,
    window: NativeWindow,
    frames: Receiver<Frame>,
    feedback: Feedback,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("decoder-{id}"))
        .spawn(move || {
            if let Err(e) = run(id, codec, width, height, &window, &frames, &feedback, &stop) {
                error!("decoder {id}: {e:#}");
            }
            info!("decoder {id} stopped");
        })
        .expect("spawn decoder thread")
}

#[allow(clippy::too_many_arguments)]
fn run(
    id: u8,
    codec: Codec,
    width: u32,
    height: u32,
    window: &NativeWindow,
    frames: &Receiver<Frame>,
    feedback: &Feedback,
    stop: &AtomicBool,
) -> Result<()> {
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

    let mut timing = Timing {
        id,
        feedback: Some(feedback.clone()),
        ..Default::default()
    };
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            match frames.recv_timeout(Duration::from_millis(4)) {
                Ok(frame) => queue(&decoder, &frame, &mut timing)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            drain(&decoder, &mut timing)?;
            timing.report();
        }
        Ok(())
    })();
    let _ = decoder.stop();
    result
}

fn queue(decoder: &MediaCodec, frame: &Frame, timing: &mut Timing) -> Result<()> {
    for _ in 0..200 {
        match decoder.dequeue_input_buffer(Duration::from_millis(5))? {
            DequeuedInputBufferResult::Buffer(mut buffer) => {
                let dst = buffer.buffer_mut();
                if frame.data.len() > dst.len() {
                    bail!(
                        "frame of {} bytes does not fit the {} byte input buffer",
                        frame.data.len(),
                        dst.len()
                    );
                }
                for (d, s) in dst.iter_mut().zip(&frame.data) {
                    d.write(*s);
                }
                decoder.queue_input_buffer(buffer, 0, frame.data.len(), frame.pts_us, 0)?;
                timing.queued(frame);
                return Ok(());
            }
            // All input buffers busy: hand finished frames to the compositor to free one.
            DequeuedInputBufferResult::TryAgainLater => drain(decoder, timing)?,
        }
    }
    warn!(
        "decoder stalled, dropping a frame (keyframe: {})",
        frame.keyframe
    );
    Ok(())
}

fn drain(decoder: &MediaCodec, timing: &mut Timing) -> Result<()> {
    loop {
        match decoder.dequeue_output_buffer(Duration::ZERO)? {
            DequeuedOutputBufferInfoResult::Buffer(buffer) => {
                timing.decoded(buffer.info().presentation_time_us() as u64);
                decoder.release_output_buffer(buffer, true)?
            }
            DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(()),
            DequeuedOutputBufferInfoResult::OutputFormatChanged
            | DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
        }
    }
}

/// Where time goes per frame: network arrival → decoder input → decoded output.
#[derive(Default)]
struct Timing {
    id: u8,
    /// Reports shown frames so the host can measure the whole trip.
    feedback: Option<Feedback>,
    /// pts → (received, queued) for frames inside the decoder.
    in_flight: HashMap<u64, (Instant, Instant)>,
    frames: u32,
    wait_ms: f32,
    wait_max: f32,
    decode_ms: f32,
    decode_max: f32,
    since: Option<Instant>,
}

impl Timing {
    fn queued(&mut self, frame: &Frame) {
        if self.in_flight.len() > 64 {
            self.in_flight.clear();
        }
        self.in_flight
            .insert(frame.pts_us, (frame.received, Instant::now()));
    }

    fn decoded(&mut self, pts_us: u64) {
        if let Some(f) = &self.feedback {
            let _ = f.send(&ClientMsg::FrameShown {
                id: self.id,
                pts_us,
            });
        }
        let Some((received, queued)) = self.in_flight.remove(&pts_us) else {
            return;
        };
        let wait = (queued - received).as_secs_f32() * 1e3;
        let decode = queued.elapsed().as_secs_f32() * 1e3;
        self.frames += 1;
        self.wait_ms += wait;
        self.wait_max = self.wait_max.max(wait);
        self.decode_ms += decode;
        self.decode_max = self.decode_max.max(decode);
    }

    fn report(&mut self) {
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() < Duration::from_secs(2) {
            return;
        }
        let n = self.frames.max(1) as f32;
        debug!(
            "timing: {} frames, net→decoder avg {:.1} max {:.1} ms, decode avg {:.1} max {:.1} ms, still inside decoder {}",
            self.frames,
            self.wait_ms / n,
            self.wait_max,
            self.decode_ms / n,
            self.decode_max,
            self.in_flight.len()
        );
        *self = Timing {
            id: self.id,
            feedback: self.feedback.take(),
            in_flight: std::mem::take(&mut self.in_flight),
            ..Default::default()
        };
    }
}
