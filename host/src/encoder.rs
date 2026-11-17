//! PipeWire → VA-API hardware encoder → encoded access units, via GStreamer.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use log::{debug, error, warn};
use prostir_proto::Codec;

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub bitrate_kbps: u32,
    /// Use the software x264 encoder instead of VA-API (H.264 only).
    pub software: bool,
    /// Slower VA-API preset: clearer small text at the same bitrate for about 2 ms more
    /// encoding per frame.
    pub sharp: bool,
}

impl EncoderConfig {
    /// VA-API `target-usage`: 4 is clearer, 6 is faster.
    pub fn target_usage(&self) -> u32 {
        if self.sharp { 4 } else { 6 }
    }
}

pub enum EncoderEvent {
    /// Negotiated stream size; emitted before the first frame and on every change.
    Size {
        width: u32,
        height: u32,
    },
    Frame {
        pts_us: u64,
        keyframe: bool,
        data: Vec<u8>,
    },
    Failed(String),
}

pub struct Encoder {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    stop: Arc<AtomicBool>,
    bus_thread: Option<JoinHandle<()>>,
}

fn pipeline_description(node: u32, cfg: &EncoderConfig) -> String {
    let source =
        format!("pipewiresrc path={node} do-timestamp=true keepalive-time=500 always-copy=false");
    let (encoder, parser, caps) = match (cfg.codec, cfg.software) {
        (Codec::H264, true) => (
            format!(
                "videoconvert name=enc_in ! video/x-raw,format=I420 ! x264enc name=enc tune=zerolatency speed-preset=superfast \
                 bitrate={} key-int-max=600 bframes=0",
                cfg.bitrate_kbps
            ),
            "h264parse config-interval=-1",
            "video/x-h264,stream-format=byte-stream,alignment=au,profile=high",
        ),
        (Codec::H264, false) => (
            format!(
                "vapostproc name=enc_in ! video/x-raw(memory:VAMemory),format=NV12 ! vah264enc name=enc rate-control=cbr bitrate={} \
                 key-int-max=600 b-frames=0 ref-frames=1 target-usage={}",
                cfg.bitrate_kbps,
                cfg.target_usage()
            ),
            "h264parse config-interval=-1",
            "video/x-h264,stream-format=byte-stream,alignment=au",
        ),
        (Codec::H265, _) => (
            format!(
                "vapostproc name=enc_in ! video/x-raw(memory:VAMemory),format=NV12 ! vah265enc name=enc rate-control=cbr bitrate={} \
                 key-int-max=600 b-frames=0 ref-frames=1 target-usage={}",
                cfg.bitrate_kbps,
                cfg.target_usage()
            ),
            "h265parse config-interval=-1",
            "video/x-h265,stream-format=byte-stream,alignment=au",
        ),
    };
    format!(
        "{source} ! queue max-size-buffers=2 leaky=downstream ! {encoder} ! {parser} ! {caps} ! appsink name=sink sync=false max-buffers=8"
    )
}

impl Encoder {
    /// `on_event` returns `true` when the receiver needs a keyframe (e.g. it had to drop frames).
    pub fn start(
        node: u32,
        cfg: &EncoderConfig,
        on_event: impl Fn(EncoderEvent) -> bool + Send + Sync + 'static,
    ) -> Result<Self> {
        let desc = pipeline_description(node, cfg);
        debug!("pipeline: {desc}");
        let pipeline = gst::parse::launch(&desc)
            .context("failed to build GStreamer pipeline (are gst-plugins-bad with VA and gst-plugin-pipewire installed?)")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("appsink missing"))?;

        let on_event = Arc::new(on_event);
        let last_size = std::sync::Mutex::new((0u32, 0u32));
        let probe = Arc::new(Mutex::new(Probe::default()));
        // Frames enter the encoder in order and (no B-frames) leave in order, so a FIFO
        // of arrival times pairs them up despite the encoder rewriting timestamps.
        if let Some(pad) = pipeline
            .by_name("enc_in")
            .and_then(|e| e.static_pad("sink"))
        {
            let probe = probe.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                probe.lock().unwrap().entered();
                gst::PadProbeReturn::Ok
            });
        }
        {
            let on_event = on_event.clone();
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        if let Some(info) =
                            sample.caps().and_then(|c| c.structure(0)).and_then(|s| {
                                Some((
                                    s.get::<i32>("width").ok()? as u32,
                                    s.get::<i32>("height").ok()? as u32,
                                ))
                            })
                        {
                            let mut last = last_size.lock().unwrap();
                            if *last != info {
                                *last = info;
                                if on_event(EncoderEvent::Size {
                                    width: info.0,
                                    height: info.1,
                                }) {
                                    request_keyframe(sink);
                                }
                            }
                        }
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                        let pts_us = buffer.pts().map(|t| t.useconds()).unwrap_or(0);
                        probe.lock().unwrap().left(node);
                        if on_event(EncoderEvent::Frame {
                            pts_us,
                            keyframe,
                            data: map.as_slice().to_vec(),
                        }) {
                            request_keyframe(sink);
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        let stop = Arc::new(AtomicBool::new(false));
        let bus = pipeline
            .bus()
            .ok_or_else(|| anyhow!("pipeline without bus"))?;
        let bus_thread = {
            let stop = stop.clone();
            let on_event = on_event.clone();
            std::thread::Builder::new()
                .name(format!("gst-bus-{node}"))
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
                            continue;
                        };
                        match msg.view() {
                            gst::MessageView::Error(e) => {
                                let text = format!("{} ({:?})", e.error(), e.debug());
                                error!("encoder for node {node}: {text}");
                                on_event(EncoderEvent::Failed(text));
                                break;
                            }
                            gst::MessageView::Warning(w) => {
                                warn!("encoder for node {node}: {}", w.error())
                            }
                            gst::MessageView::Eos(_) => {
                                on_event(EncoderEvent::Failed("stream ended".into()));
                                break;
                            }
                            _ => {}
                        }
                    }
                })?
        };

        pipeline
            .set_state(gst::State::Playing)
            .context("cannot start encoder pipeline")?;
        Ok(Self {
            pipeline,
            sink,
            stop,
            bus_thread: Some(bus_thread),
        })
    }

    /// Changes the target bitrate while running.
    pub fn set_bitrate(&self, kbps: u32) {
        if let Some(enc) = self.pipeline.by_name("enc") {
            enc.set_property("bitrate", kbps);
        }
    }

    /// Switches the VA-API speed/quality preset on the fly (no effect on x264).
    pub fn set_target_usage(&self, value: u32) {
        if let Some(enc) = self.pipeline.by_name("enc")
            && enc.find_property("target-usage").is_some()
        {
            enc.set_property("target-usage", value);
            self.force_keyframe();
        }
    }

    /// Stops capturing and encoding (headset not worn) or resumes with a fresh keyframe.
    pub fn set_paused(&self, paused: bool) {
        let state = if paused {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        let _ = self.pipeline.set_state(state);
        if !paused {
            self.force_keyframe();
        }
    }

    /// Asks the encoder to emit an IDR frame (with parameter sets) as soon as possible.
    pub fn force_keyframe(&self) {
        request_keyframe(&self.sink);
    }
}

fn request_keyframe(sink: &gst_app::AppSink) {
    let event = gst_video::UpstreamForceKeyUnitEvent::builder()
        .all_headers(true)
        .build();
    if let Some(pad) = sink.static_pad("sink") {
        pad.push_event(event);
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(t) = self.bus_thread.take() {
            let _ = t.join();
        }
    }
}

/// Time each frame spends in conversion + encoding, and gaps between captured frames.
#[derive(Default)]
struct Probe {
    inside: std::collections::VecDeque<Instant>,
    last_entered: Option<Instant>,
    max_gap_ms: f64,
    frames: u32,
    sum_ms: f64,
    max_ms: f64,
    since: Option<Instant>,
}

impl Probe {
    fn entered(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_entered.replace(now) {
            self.max_gap_ms = self.max_gap_ms.max((now - prev).as_secs_f64() * 1e3);
        }
        if self.inside.len() > 64 {
            self.inside.clear();
        }
        self.inside.push_back(now);
    }

    fn left(&mut self, node: u32) {
        let Some(t0) = self.inside.pop_front() else {
            return;
        };
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        self.frames += 1;
        self.sum_ms += ms;
        self.max_ms = self.max_ms.max(ms);
        if ms > 30.0 {
            debug!(
                "node {node}: frame spent {ms:.0} ms in the encoder, {} still inside",
                self.inside.len()
            );
        }
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed().as_secs() >= 5 {
            debug!(
                "node {node}: {} frames, encoder avg {:.1} max {:.1} ms, longest gap between captured frames {:.0} ms",
                self.frames,
                self.sum_ms / f64::from(self.frames),
                self.max_ms,
                self.max_gap_ms
            );
            let inside = std::mem::take(&mut self.inside);
            let last_entered = self.last_entered;
            *self = Self {
                inside,
                last_entered,
                ..Self::default()
            };
        }
    }
}
