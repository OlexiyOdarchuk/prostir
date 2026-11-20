//! Frames from the platform (a PipeWire node, a capture) → hardware encoder → encoded
//! access units, via GStreamer.
//!
//! Each output gets its own encoder, picked for where its frames live
//! ([`crate::platform::FrameSource::memory`]) so they stay on the graphics card
//! (`choice.rs`), from the encoders this PC was found to have (`probe.rs`, kept in the
//! cache directory), all sharing one graphics device per card (`gpu.rs`). An encoder
//! that does not open goes on to the next one for that output only: `Displays` hears
//! of it as [`EncoderEvent::Unopened`] and learns the encoder's session limit from it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use log::{debug, error, info, warn};
use prostir_proto::Codec;

mod choice;
mod gpu;
mod probe;

pub use choice::{Backend, Choice, Kind, Wanted, note};
pub use gpu::GpuDevices;
#[cfg(not(feature = "fake-desktop"))]
pub use probe::file_identity;
pub use probe::{Next, catalog, start_probing};

use crate::platform::AdapterId;

impl Backend {
    /// The GStreamer elements it encodes `codec` with; none for a codec it cannot
    /// encode (AV1: no encoder of Prostir makes it yet). The encoder is the last.
    fn elements(self, codec: Codec) -> Option<&'static [&'static str]> {
        Some(match (self, codec) {
            (Self::Va, Codec::H265) => &["vapostproc", "vah265enc"],
            (Self::Va, Codec::H264) => &["vapostproc", "vah264enc"],
            (Self::Nvenc, Codec::H265) => &["videoconvert", "nvh265enc"],
            (Self::Nvenc, Codec::H264) => &["videoconvert", "nvh264enc"],
            (Self::X264, Codec::H264) => &["videoconvert", "x264enc"],
            // Windows (step 11c), as the prototype ran them: the card's own encoders
            // take the capture's textures as they are, Media Foundation in NV12.
            (Self::NvencD3d11, Codec::H265) => &["nvd3d11h265enc"],
            (Self::NvencD3d11, Codec::H264) => &["nvd3d11h264enc"],
            (Self::Amf, Codec::H265) => &["amfh265enc"],
            (Self::Amf, Codec::H264) => &["amfh264enc"],
            (Self::Qsv, Codec::H265) => &["qsvh265enc"],
            (Self::Qsv, Codec::H264) => &["qsvh264enc"],
            (Self::MediaFoundation, Codec::H265) => &["d3d11convert", "mfh265enc"],
            (Self::MediaFoundation, Codec::H264) => &["d3d11convert", "mfh264enc"],
            #[cfg(feature = "fake-desktop")]
            (Self::Fake, Codec::H264) => &["videoconvert", "x264enc"],
            #[cfg(feature = "fake-desktop")]
            (Self::Fake, Codec::H265) => return None,
            (_, Codec::Av1) | (Self::X264, Codec::H265) => return None,
        })
    }
}

/// The parser after the encoder of `codec`.
fn parser(codec: Codec) -> &'static str {
    match codec {
        Codec::H265 => "h265parse",
        Codec::H264 => "h264parse",
        Codec::Av1 => "av1parse",
    }
}

/// What this PC can encode with: each codec, and whether on the graphics card, as the
/// encoders' probes found (see `probe.rs`).
pub fn available() -> Vec<(Codec, bool)> {
    catalog().available()
}

/// The encoder this PC encodes with when nothing is known of the frames: VA-API, then
/// NVENC, then x264 on the CPU, in the preferred codec first, as their probes found
/// them. `software` forces x264. The error names what to install. Each output then
/// gets its own encoder for where its frames are (`Catalog::rank`).
pub fn pick(prefer: Codec, software: bool) -> Result<(Backend, Codec)> {
    catalog().pick(prefer, software)
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub bitrate_kbps: u32,
    pub backend: Backend,
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
    /// The encoder itself failed before its first frame: it did not open (no more
    /// sessions, a size or format its driver does not take). Another encoder may.
    Unopened(NotOpened),
}

/// Why an encoder did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotOpened {
    pub why: String,
    /// It said it had no more sessions, or no room for one more (see
    /// [`short_of_sessions`]).
    pub short: bool,
    /// It was opening again after a pause (every stream does at once then).
    pub resumed: bool,
}

/// What encoders say when they have no session left: GStreamer's "no space left" and
/// "busy" resource errors (the fake encoder of the tests says the first), NVENC's out
/// of memory and client key errors (a consumer card's session cap), and VA-API's
/// allocation, limit and busy statuses.
const SHORT_OF_SESSIONS: &[&str] = &[
    "NV_ENC_ERR_OUT_OF_MEMORY",
    "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
    "NV_ENC_ERR_ENCODER_BUSY",
    "VA_STATUS_ERROR_ALLOCATION_FAILED",
    "VA_STATUS_ERROR_MAX_NUM_EXCEEDED",
    "VA_STATUS_ERROR_HW_BUSY",
];

/// Whether an error (`error`, with its debug text `debug`) says the encoder has no
/// session left, rather than anything else (a format, a size, a broken driver).
pub fn short_of_sessions(error: Option<&gst::glib::Error>, text: &str) -> bool {
    error.is_some_and(|e| {
        e.matches(gst::ResourceError::NoSpaceLeft) || e.matches(gst::ResourceError::Busy)
    }) || SHORT_OF_SESSIONS.iter().any(|s| text.contains(s))
}

pub struct Encoder {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    /// The source sends nothing after a pause until the screen changes (PipeWire from
    /// KWin): it is started over on resuming. A capture pushing its own frames
    /// (`appsrc`) repeats the last one by itself.
    restart_on_resume: bool,
    /// It takes frames straight from GPU buffers (dmabuf), so a capture hands it those.
    takes_dmabuf: bool,
    bus_thread: Option<JoinHandle<()>>,
    kind: Kind,
    codec: Codec,
    /// It has encoded a frame since it last started.
    encoded: Arc<AtomicBool>,
    /// It was resumed after a pause.
    resumed: Arc<AtomicBool>,
    /// What to say of the encoder this output got, when it is not the best for it.
    note: Option<String>,
    /// Its frames go through the CPU on their way to it.
    copies: bool,
    /// The graphics devices it took, let go after the pipeline stopped (the fields
    /// drop after `Drop::drop` and in this order).
    _devices: gpu::Lease,
    /// A fake hardware encoder that opened, counted until dropped.
    #[cfg(feature = "fake-desktop")]
    fake_open: bool,
}

/// Where the frames come from: the start of the pipeline, made by the platform (a
/// PipeWire node on KDE, frames pushed by a capture on Hyprland).
#[derive(Debug, Clone)]
pub struct Input {
    /// GStreamer source elements. Frames the platform pushes itself go into an `appsrc`
    /// named `src` (see [`Encoder::appsrc`]).
    pub source: String,
    /// What it is, for logs: `node 42`, `Prostir-1`.
    pub label: String,
}

fn pipeline_description(input: &Input, cfg: &EncoderConfig) -> Result<String> {
    Ok(format!(
        "{} ! appsink name=sink sync=false max-buffers=8",
        encoding(input, cfg)?
    ))
}

/// The pipeline from the source to the encoded frames, without the sink.
fn encoding(input: &Input, cfg: &EncoderConfig) -> Result<String> {
    let source = &input.source;
    let (encoder, parser, caps) = match (cfg.codec, cfg.backend) {
        (Codec::Av1, _) | (Codec::H265, Backend::X264) => {
            bail!("no encoder for {:?} on {:?}", cfg.codec, cfg.backend)
        }
        (_, Backend::NvencD3d11 | Backend::Amf | Backend::Qsv | Backend::MediaFoundation) => {
            bail!("{} is for Windows (step 11c)", cfg.backend.label())
        }
        // x264 underneath; one more than its sessions lets no frame through, and its
        // encoder says it did not open (see `Encoder::start`).
        #[cfg(feature = "fake-desktop")]
        (Codec::H265, Backend::Fake) => bail!("the fake encoder makes H.264 only"),
        #[cfg(feature = "fake-desktop")]
        (Codec::H264, Backend::Fake) if probe::fake::refuses() && !probe::fake::opens() => {
            bail!("no more encoder sessions (NV_ENC_ERR_OUT_OF_MEMORY)")
        }
        #[cfg(feature = "fake-desktop")]
        (Codec::H264, Backend::Fake) => (
            format!(
                "videoconvert name=enc_in ! video/x-raw,format=I420 ! valve drop={} ! x264enc name=enc tune=zerolatency speed-preset=superfast \
                 bitrate={} key-int-max=600 bframes=0",
                !probe::fake::opens(),
                cfg.bitrate_kbps
            ),
            "h264parse config-interval=-1",
            "video/x-h264,stream-format=byte-stream,alignment=au,profile=high",
        ),
        (Codec::H264, Backend::X264) => (
            format!(
                "videoconvert name=enc_in ! video/x-raw,format=I420 ! x264enc name=enc tune=zerolatency speed-preset=superfast \
                 bitrate={} key-int-max=600 bframes=0",
                cfg.bitrate_kbps
            ),
            "h264parse config-interval=-1",
            "video/x-h264,stream-format=byte-stream,alignment=au,profile=high",
        ),
        (Codec::H264, Backend::Nvenc) => (
            format!(
                "videoconvert name=enc_in ! video/x-raw,format=NV12 ! nvh264enc name=enc rc-mode=cbr \
                 bitrate={} gop-size=600 bframes=0 zerolatency=true",
                cfg.bitrate_kbps
            ),
            "h264parse config-interval=-1",
            "video/x-h264,stream-format=byte-stream,alignment=au",
        ),
        (Codec::H265, Backend::Nvenc) => (
            format!(
                "videoconvert name=enc_in ! video/x-raw,format=NV12 ! nvh265enc name=enc rc-mode=cbr \
                 bitrate={} gop-size=600 bframes=0 zerolatency=true",
                cfg.bitrate_kbps
            ),
            "h265parse config-interval=-1",
            "video/x-h265,stream-format=byte-stream,alignment=au",
        ),
        (Codec::H264, Backend::Va) => (
            format!(
                "vapostproc name=enc_in ! video/x-raw(memory:VAMemory),format=NV12 ! vah264enc name=enc rate-control=cbr bitrate={} \
                 key-int-max=600 b-frames=0 ref-frames=1 target-usage={}",
                cfg.bitrate_kbps,
                cfg.target_usage()
            ),
            "h264parse config-interval=-1",
            "video/x-h264,stream-format=byte-stream,alignment=au",
        ),
        (Codec::H265, Backend::Va) => (
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
    Ok(format!(
        "{source} ! queue max-size-buffers=2 leaky=downstream ! {encoder} ! {parser} ! {caps}"
    ))
}

impl Encoder {
    /// An encoder with `cfg` on card `adapter` (none: the one its driver picks), taking
    /// its graphics device from `gpu`. `on_event` returns `true` when the receiver needs
    /// a keyframe (e.g. it had to drop frames).
    pub fn start(
        input: &Input,
        cfg: &EncoderConfig,
        adapter: Option<AdapterId>,
        gpu: &GpuDevices,
        on_event: impl Fn(EncoderEvent) -> bool + Send + Sync + 'static,
    ) -> Result<Self> {
        let desc = pipeline_description(input, cfg)?;
        let node = input.label.clone();
        debug!("pipeline: {desc}");
        let pipeline = gst::parse::launch(&desc)
            .context("failed to build GStreamer pipeline (are gst-plugins-bad with VA and gst-plugin-pipewire installed?)")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("appsink missing"))?;
        let devices = gpu.share_with(&pipeline);

        let on_event = Arc::new(on_event);
        // Nothing encoded yet: an error of the encoder now means it did not open.
        let encoded = Arc::new(AtomicBool::new(false));
        let resumed = Arc::new(AtomicBool::new(false));
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
            let node = node.clone();
            let encoded = encoded.clone();
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        encoded.store(true, Ordering::Relaxed);
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
                        probe.lock().unwrap().left(&node);
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
            let encoded = encoded.clone();
            let resumed = resumed.clone();
            std::thread::Builder::new()
                .name(format!("gst-bus-{node}"))
                .spawn(move || {
                    // Sleeps until a message comes: dropping the encoder posts one.
                    while !stop.load(Ordering::Relaxed) {
                        let Some(msg) = bus.timed_pop(gst::ClockTime::NONE) else {
                            // The bus is flushing while the pipeline restarts: no message
                            // can come, so not a loop on the spot.
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            continue;
                        };
                        match msg.view() {
                            gst::MessageView::Error(e) => {
                                let text = format!("{} ({:?})", e.error(), e.debug());
                                // The encoder (or its converter) before its first frame:
                                // it did not open, and another encoder may.
                                let by_encoder = msg
                                    .src()
                                    .is_some_and(|s| matches!(s.name().as_str(), "enc" | "enc_in"));
                                if by_encoder && !encoded.load(Ordering::Relaxed) {
                                    warn!("encoder for {node} did not open: {text}");
                                    let error = e.error();
                                    on_event(EncoderEvent::Unopened(NotOpened {
                                        why: error.to_string(),
                                        short: short_of_sessions(Some(&error), &text),
                                        resumed: resumed.load(Ordering::Relaxed),
                                    }));
                                } else {
                                    error!("encoder for {node}: {text}");
                                    on_event(EncoderEvent::Failed(text));
                                }
                                break;
                            }
                            gst::MessageView::Warning(w) => {
                                warn!("encoder for {node}: {}", w.error())
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

        let restart_on_resume = pipeline.by_name("src").is_none();
        // Built first, so a failed start below stops the bus thread and the pipeline
        // through Drop instead of leaking them.
        #[cfg(feature = "fake-desktop")]
        let fake_open = cfg.backend == Backend::Fake && probe::fake::opens();
        let encoder = Self {
            pipeline,
            sink,
            stop,
            paused: Arc::new(AtomicBool::new(false)),
            restart_on_resume,
            takes_dmabuf: cfg.backend.takes_dmabuf(),
            bus_thread: Some(bus_thread),
            kind: (cfg.backend, adapter),
            codec: cfg.codec,
            encoded,
            resumed,
            note: None,
            copies: false,
            _devices: devices,
            #[cfg(feature = "fake-desktop")]
            fake_open,
        };
        #[cfg(feature = "fake-desktop")]
        if fake_open {
            probe::fake::OPEN.fetch_add(1, Ordering::Relaxed);
        }
        encoder
            .pipeline
            .set_state(gst::State::Playing)
            .context("cannot start encoder pipeline")?;
        // One more fake hardware encoder than it opens says so, as a real one does once
        // it has the first frame's format.
        #[cfg(feature = "fake-desktop")]
        if cfg.backend == Backend::Fake
            && !fake_open
            && let Some(enc) = encoder.pipeline.by_name("enc")
        {
            gst::element_error!(
                enc,
                gst::ResourceError::NoSpaceLeft,
                ("no more encoder sessions")
            );
        }
        Ok(encoder)
    }

    /// The encoder it runs, to count sessions by.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The codec it makes.
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Whether it has encoded a frame (so it opened).
    pub fn opened(&self) -> bool {
        self.encoded.load(Ordering::Relaxed)
    }

    /// Whether it is paused now (the headset is off).
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// What to say of the encoder this output got (see [`note`]).
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Whether its frames go through the CPU on their way to it.
    pub fn copies(&self) -> bool {
        self.copies
    }

    /// What it was picked as for its output, and what to say of that.
    pub fn set_choice(&mut self, choice: &Choice, note: Option<String>) {
        self.copies = choice.copies();
        self.note = note;
    }

    /// The `appsrc` to push frames into, when the input has one.
    #[cfg_attr(
        feature = "fake-desktop",
        expect(dead_code, reason = "the fake desktop pushes no frames")
    )]
    pub fn appsrc(&self) -> Option<gst_app::AppSrc> {
        self.pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
    }

    /// Whether it takes frames straight from GPU buffers (dmabuf).
    #[cfg_attr(
        feature = "fake-desktop",
        expect(dead_code, reason = "the fake desktop pushes no frames")
    )]
    pub fn takes_dmabuf(&self) -> bool {
        self.takes_dmabuf
    }

    /// Set while the encoder is paused, so a capture can rest too.
    #[cfg_attr(
        feature = "fake-desktop",
        expect(dead_code, reason = "the fake desktop pushes no frames")
    )]
    pub fn paused(&self) -> Arc<AtomicBool> {
        self.paused.clone()
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
        self.paused.store(paused, Ordering::Relaxed);
        if !paused {
            self.resumed.store(true, Ordering::Relaxed);
        }
        if !paused && self.restart_on_resume {
            // Resumed as it was, an idle screen would send no frame at all (the picture
            // in the headset stays frozen, or a headset that just connected gets no
            // monitors): connected again, the source sends the current picture at once.
            let _ = self.pipeline.set_state(gst::State::Null);
            // It opens again with the first frame.
            self.encoded.store(false, Ordering::Relaxed);
        }
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
        // Wakes the bus thread, which then sees `stop`.
        if let Some(bus) = self.pipeline.bus() {
            let _ = bus.post(gst::message::Application::new(gst::Structure::new_empty(
                "prostir-stop",
            )));
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(t) = self.bus_thread.take() {
            let _ = t.join();
        }
        #[cfg(feature = "fake-desktop")]
        if self.fake_open {
            probe::fake::OPEN.fetch_sub(1, Ordering::Relaxed);
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

    fn left(&mut self, node: &str) {
        let Some(t0) = self.inside.pop_front() else {
            return;
        };
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        self.frames += 1;
        self.sum_ms += ms;
        self.max_ms = self.max_ms.max(ms);
        if ms > 30.0 {
            debug!(
                "{node}: frame spent {ms:.0} ms in the encoder, {} still inside",
                self.inside.len()
            );
        }
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed().as_secs() >= 5 {
            debug!(
                "{node}: {} frames, encoder avg {:.1} max {:.1} ms, longest gap between captured frames {:.0} ms",
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

/// Decodes one keyframe of a monitor's stream into a PNG in the Pictures folder (a
/// screenshot of just that monitor where the desktop's tool cannot take one output).
pub fn keyframe_to_png(data: &[u8], codec: Codec) -> Result<std::path::PathBuf> {
    let path = crate::platform::screenshot_path()?;
    let (caps, parse) = match codec {
        Codec::H264 => ("video/x-h264", "h264parse"),
        Codec::H265 => ("video/x-h265", "h265parse"),
        Codec::Av1 => ("video/x-av1", "av1parse"),
    };
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=src caps={caps},stream-format=byte-stream,alignment=au ! {parse} ! \
         decodebin ! videoconvert ! pngenc snapshot=true ! filesink name=sink"
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let sink = pipeline.by_name("sink").context("no filesink")?;
    sink.set_property("location", path.to_string_lossy().as_ref());
    let src = pipeline
        .by_name("src")
        .context("no appsrc")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| anyhow::anyhow!("not an appsrc"))?;
    pipeline.set_state(gst::State::Playing)?;
    let pushed = src
        .push_buffer(gst::Buffer::from_slice(data.to_vec()))
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .and_then(|_| src.end_of_stream().map_err(|e| anyhow::anyhow!("{e:?}")));
    let bus = pipeline.bus().context("no bus")?;
    let ended = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    pushed?;
    match ended.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) if path.exists() => {
            info!("screenshot saved to {path:?}");
            Ok(path)
        }
        Some(gst::MessageView::Error(e)) => bail!("{}", e.error()),
        _ => bail!("the frame was not decoded in time"),
    }
}

#[cfg(test)]
mod live {
    /// A keyframe from the graphics card's encoder becomes a PNG (run by hand:
    /// `cargo test -- --ignored keyframe`; needs the VA plugin and a GPU).
    #[test]
    #[ignore]
    fn keyframe_becomes_png() {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        gst::init().unwrap();
        let pipeline = gst::parse::launch(
            "videotestsrc num-buffers=1 ! video/x-raw,width=640,height=360 ! vapostproc ! \
             vah265enc ! h265parse config-interval=-1 ! \
             video/x-h265,stream-format=byte-stream,alignment=au ! appsink name=out",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let out = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let sample = out.pull_sample().unwrap();
        let data = sample.buffer().unwrap().map_readable().unwrap().to_vec();
        pipeline.set_state(gst::State::Null).unwrap();
        let path = super::keyframe_to_png(&data, prostir_proto::Codec::H265).unwrap();
        let png = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        eprintln!("decoded {} bytes into a {} byte PNG", data.len(), png.len());
    }
}
