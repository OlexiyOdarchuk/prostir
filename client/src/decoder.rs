//! Hardware decoding with MediaCodec straight into the compositor's surface.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{error, info, warn};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use qmon_proto::Codec;

use crate::net::Frame;

/// Decodes `frames` into `window` until the channel closes or `stop` is set.
pub fn spawn(
    id: u8,
    codec: Codec,
    width: u32,
    height: u32,
    window: NativeWindow,
    frames: Receiver<Frame>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("decoder-{id}"))
        .spawn(move || {
            if let Err(e) = run(codec, width, height, &window, &frames, &stop) {
                error!("decoder {id}: {e:#}");
            }
            info!("decoder {id} stopped");
        })
        .expect("spawn decoder thread")
}

fn run(
    codec: Codec,
    width: u32,
    height: u32,
    window: &NativeWindow,
    frames: &Receiver<Frame>,
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

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            match frames.recv_timeout(Duration::from_millis(4)) {
                Ok(frame) => queue(&decoder, &frame)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            drain(&decoder)?;
        }
        Ok(())
    })();
    let _ = decoder.stop();
    result
}

fn queue(decoder: &MediaCodec, frame: &Frame) -> Result<()> {
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
                return Ok(());
            }
            // All input buffers busy: hand finished frames to the compositor to free one.
            DequeuedInputBufferResult::TryAgainLater => drain(decoder)?,
        }
    }
    warn!(
        "decoder stalled, dropping a frame (keyframe: {})",
        frame.keyframe
    );
    Ok(())
}

fn drain(decoder: &MediaCodec) -> Result<()> {
    loop {
        match decoder.dequeue_output_buffer(Duration::ZERO)? {
            DequeuedOutputBufferInfoResult::Buffer(buffer) => {
                decoder.release_output_buffer(buffer, true)?
            }
            DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(()),
            DequeuedOutputBufferInfoResult::OutputFormatChanged
            | DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
        }
    }
}
