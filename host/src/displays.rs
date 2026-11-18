//! The set of virtual monitors, their encoders, and delivery to the headset.

use crate::i18n::t;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{debug, error, info, warn};
use prostir_proto::{Codec, HostMsg, LayoutRow, MIRROR_ID, NEXT, PcSettings, WINDOW_IDS};
use std::sync::atomic::AtomicU8;

use crate::encoder::{Encoder, EncoderConfig, EncoderEvent, Input};
use crate::layout::{Seat, arrange, arrange_free, panel_size};
use crate::outbox::Outbox;
use crate::platform::audio::{AudioOut, MicIn};
use crate::platform::windows;
use crate::platform::{
    ActiveWindow, Desktop, DesktopBackend, Geometry, MonitorSpec, Output, OutputBackend,
};

/// Retry timing after failures: 2 s, then doubling up to 2 min.
#[derive(Default)]
struct Backoff {
    failures: u32,
    next: Option<Instant>,
}

impl Backoff {
    fn due(&self) -> bool {
        self.next.is_none_or(|t| Instant::now() >= t)
    }

    fn failed(&mut self) {
        self.failures += 1;
        let secs = (2u64 << self.failures.min(6)).min(120);
        self.next = Some(Instant::now() + Duration::from_secs(secs));
    }

    fn succeeded(&mut self) {
        *self = Self::default();
    }
}

/// More streams than this overwhelm the headset's hardware decoders.
pub const MAX_MONITORS: usize = 6;

/// Per-stream delivery state for the currently connected client.
struct StreamSlot {
    size: Option<(u32, u32)>,
    announced: bool,
    need_keyframe: bool,
    /// When a keyframe was last asked for: asking again on every frame while waiting
    /// would make the encoder emit one huge keyframe after another.
    keyframe_asked: Option<Instant>,
    /// pts → when the encoded frame was queued for sending.
    sent: BTreeMap<u64, Instant>,
    latency: LatencyStats,
}

impl StreamSlot {
    /// Whether to ask the encoder for a keyframe now (at most every 300 ms).
    fn ask_keyframe(&mut self) -> bool {
        let due = self
            .keyframe_asked
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(300));
        if due {
            self.keyframe_asked = Some(Instant::now());
        }
        due
    }

    fn new() -> Self {
        Self {
            size: None,
            announced: false,
            need_keyframe: true,
            keyframe_asked: None,
            sent: BTreeMap::new(),
            latency: LatencyStats::default(),
        }
    }
}

/// Encoded on the PC → handed to the headset's compositor, as reported back by the headset.
#[derive(Default)]
struct LatencyStats {
    frames: u32,
    sum_ms: f64,
    max_ms: f64,
    since: Option<Instant>,
}

impl LatencyStats {
    fn add(&mut self, monitor: u8, ms: f64) {
        self.frames += 1;
        self.sum_ms += ms;
        self.max_ms = self.max_ms.max(ms);
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= Duration::from_secs(5) {
            info!(
                "{}: encoded → shown in headset avg {:.1} ms, max {:.1} ms ({} frames)",
                stream_name(monitor),
                self.sum_ms / f64::from(self.frames),
                self.max_ms,
                self.frames
            );
            *self = Self::default();
        }
    }
}

fn stream_name(id: u8) -> String {
    if id == MIRROR_ID {
        "mirror".into()
    } else {
        format!("Prostir-{}", u32::from(id) + 1)
    }
}

/// Delivers encoder output to whichever client is connected.
pub struct Router {
    /// The codec announced with new streams (it can change: see `set_pc_settings`).
    codec: AtomicU8,
    /// A screenshot waiting for the next keyframe of stream `id` (see `screenshot`).
    snapshot: Mutex<Option<(u8, std::sync::mpsc::SyncSender<Vec<u8>>)>>,
    inner: Mutex<RouterInner>,
    failed: AtomicBool,
    mirror_failed: AtomicBool,
}

struct RouterInner {
    tx: Option<Arc<Outbox>>,
    streams: BTreeMap<u8, StreamSlot>,
}

impl Router {
    fn new(codec: Codec) -> Self {
        Self {
            codec: AtomicU8::new(codec as u8),
            inner: Mutex::new(RouterInner {
                tx: None,
                streams: BTreeMap::new(),
            }),
            failed: AtomicBool::new(false),
            mirror_failed: AtomicBool::new(false),
            snapshot: Mutex::new(None),
        }
    }

    pub fn attach(&self, tx: Arc<Outbox>) {
        let mut inner = self.inner.lock().unwrap();
        inner.tx = Some(tx);
        for s in inner.streams.values_mut() {
            s.announced = false;
            s.need_keyframe = true;
        }
    }

    pub fn detach(&self) {
        self.inner.lock().unwrap().tx = None;
    }

    /// Sends a control message to the connected client, if any. Never blocks.
    pub fn send(&self, msg: HostMsg) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(tx) = &inner.tx
            && !tx.control(msg)
        {
            inner.tx = None;
        }
    }

    fn add_stream(&self, id: u8) {
        self.inner
            .lock()
            .unwrap()
            .streams
            .insert(id, StreamSlot::new());
    }

    fn remove_stream(&self, id: u8) {
        let mut inner = self.inner.lock().unwrap();
        let announced = inner.streams.remove(&id).is_some_and(|s| s.announced);
        if announced
            && let Some(tx) = &inner.tx
            && !tx.control(HostMsg::StreamEnd { id })
        {
            inner.tx = None;
        }
    }

    /// Forwards a slice of PC sound; dropped if the network is backed up (a tiny gap
    /// in sound beats growing latency).
    pub fn send_audio(&self, pcm: Vec<u8>) {
        if let Some(tx) = &self.inner.lock().unwrap().tx {
            tx.audio(HostMsg::Audio { pcm });
        }
    }

    pub fn frame_shown(&self, id: u8, pts_us: u64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(slot) = inner.streams.get_mut(&id) else {
            return;
        };
        match slot.sent.remove(&pts_us) {
            Some(sent) => slot.latency.add(id, sent.elapsed().as_secs_f64() * 1e3),
            None => debug!(
                "shown frame {pts_us} of Prostir-{} is unknown",
                u32::from(id) + 1
            ),
        }
        // The headset reports only some frames: older ones are done too.
        slot.sent = slot.sent.split_off(&pts_us);
    }

    /// Returns `true` when the encoder should produce a keyframe.
    fn on_event(&self, id: u8, event: EncoderEvent) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let RouterInner { tx, streams } = &mut *inner;
        // Streams whose queued frames were too late to send: they need a keyframe.
        if let Some(outbox) = tx {
            for lost in outbox.take_dropped() {
                if let Some(s) = streams.get_mut(&lost) {
                    s.need_keyframe = true;
                }
            }
        }
        let Some(slot) = streams.get_mut(&id) else {
            return false;
        };
        match event {
            EncoderEvent::Size { width, height } => {
                info!("{}: streaming {width}x{height}", stream_name(id));
                slot.size = Some((width, height));
                slot.announced = false;
                slot.need_keyframe = true;
                true
            }
            EncoderEvent::Failed(reason) => {
                error!("{}: {reason}", stream_name(id));
                // A broken mirror is restarted by `Displays::tick`; anything else ends the session.
                if id == MIRROR_ID {
                    self.mirror_failed.store(true, Ordering::Relaxed);
                } else {
                    self.failed.store(true, Ordering::Relaxed);
                }
                false
            }
            EncoderEvent::Frame {
                pts_us,
                keyframe,
                data,
            } => {
                if keyframe {
                    let mut wanted = self.snapshot.lock().unwrap();
                    if wanted.as_ref().is_some_and(|(want, _)| *want == id)
                        && let Some((_, tx)) = wanted.take()
                    {
                        let _ = tx.try_send(data.clone());
                    }
                }
                let Some(sender) = tx else { return false };
                if !slot.announced {
                    let Some((width, height)) = slot.size else {
                        return true;
                    };
                    let start = HostMsg::StreamStart {
                        id,
                        width,
                        height,
                        codec: if self.codec.load(Ordering::Relaxed) == Codec::H264 as u8 {
                            Codec::H264
                        } else {
                            Codec::H265
                        },
                        name: stream_name(id),
                    };
                    if !sender.control(start) {
                        *tx = None;
                        return false;
                    }
                    slot.announced = true;
                }
                if slot.need_keyframe && !keyframe {
                    return slot.ask_keyframe();
                }
                let bytes = data.len();
                let msg = HostMsg::Frame {
                    id,
                    pts_us,
                    keyframe,
                    data,
                };
                if sender.video(id, keyframe, bytes, msg) {
                    slot.need_keyframe = false;
                    if keyframe {
                        slot.keyframe_asked = None;
                    }
                    if slot.sent.len() > 256 {
                        slot.sent.clear();
                    }
                    slot.sent.insert(pts_us, Instant::now());
                    false
                } else {
                    // Too much waiting already: drop until the next keyframe so the
                    // decoder never sees a broken reference chain.
                    slot.need_keyframe = true;
                    slot.ask_keyframe()
                }
            }
        }
    }
}

struct Monitor {
    // Field order matters: the encoder must stop before the output goes away.
    encoder: Encoder,
    output: Output,
    spec: MonitorSpec,
}

/// The physical display (laptop panel) streamed as-is.
struct Mirror {
    // Field order matters: the encoder stops before the stream.
    encoder: Encoder,
    output: Output,
}

/// Saved monitor set: one `WIDTHxHEIGHT[@scale]` per line, `*` marks the primary one;
/// a `mirror on|off` line remembers the physical display mirror.
pub struct SavedMonitors {
    pub specs: Vec<MonitorSpec>,
    pub primary: Option<usize>,
    pub mirror: bool,
    /// PC sound goes to the headset while it is connected.
    pub audio: bool,
    /// The headset microphone is the PC's microphone while it is connected.
    pub mic: bool,
    /// Mbit/s per monitor, if chosen in the tray.
    pub bitrate: Option<u32>,
    /// Clearer-text encoder preset.
    pub sharp: bool,
    /// Codec chosen in the headset menu, if any.
    pub codec: Option<Codec>,
    /// Encode on the CPU, chosen in the headset menu.
    pub software: bool,
    /// Rows from top to bottom, covering `specs` in order: the seen centre of each
    /// monitor in it ([`NEXT`] if unknown). Empty: one row.
    pub rows: Vec<Vec<i32>>,
}

fn config_path() -> Option<PathBuf> {
    let base = crate::platform::config_home()?;
    let dir = base.join("prostir");
    // Settings from before the rename.
    let old = base.join("questmon");
    if !dir.exists() && old.exists() {
        let _ = std::fs::rename(&old, &dir);
    }
    Some(dir.join("monitors.txt"))
}

pub fn load_saved(parse: impl Fn(&str) -> Result<MonitorSpec, String>) -> Option<SavedMonitors> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    let mut specs = Vec::new();
    let mut primary = None;
    let mut mirror = true;
    let mut audio = true;
    let mut mic = true;
    let mut bitrate = None;
    let mut sharp = false;
    let mut codec = None;
    let mut software = false;
    let mut rows: Vec<Vec<i32>> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line == "row" || line.starts_with("row ") {
            rows.push(Vec::new());
            continue;
        }
        if let Some(v) = line.strip_prefix("sharp ") {
            sharp = v.trim() == "on";
            continue;
        }
        if let Some(v) = line.strip_prefix("codec ") {
            codec = match v.trim() {
                "h264" => Some(Codec::H264),
                "h265" => Some(Codec::H265),
                _ => None,
            };
            continue;
        }
        if let Some(v) = line.strip_prefix("software ") {
            software = v.trim() == "on";
            continue;
        }
        if let Some(v) = line.strip_prefix("bitrate ") {
            bitrate = v.trim().parse().ok();
            continue;
        }
        if let Some(v) = line.strip_prefix("mic ") {
            mic = v.trim() != "off";
            continue;
        }
        if let Some(v) = line.strip_prefix("audio ") {
            audio = v.trim() != "off";
            continue;
        }
        if let Some(v) = line.strip_prefix("mirror ") {
            mirror = v.trim() != "off";
            continue;
        }
        let (line, star) = match line.strip_suffix('*') {
            Some(s) => (s.trim(), true),
            None => (line, false),
        };
        // `2560x1440 at -1300`: where the headset showed it.
        let (spec, center) = match line.split_once(" at ") {
            Some((spec, c)) => (spec.trim(), c.trim().parse().unwrap_or(NEXT)),
            None => (line, NEXT),
        };
        if let Ok(spec) = parse(spec) {
            if star {
                primary = Some(specs.len());
            }
            if let Some(r) = rows.last_mut() {
                r.push(center);
            }
            specs.push(spec);
        }
    }
    (!specs.is_empty()).then_some(SavedMonitors {
        specs,
        primary,
        mirror,
        audio,
        mic,
        bitrate,
        sharp,
        codec,
        software,
        rows,
    })
}

pub struct Displays {
    // Field order matters: streams close before the desktop connection.
    monitors: BTreeMap<u8, Monitor>,
    /// Window panels: an output made for one window each (ids in `WINDOW_IDS`).
    windows: BTreeMap<u8, Monitor>,
    /// When the window panels are next checked for a closed window.
    windows_checked: Instant,
    /// The window on each panel, as it was before (to put it back).
    windows_origin: HashMap<u8, ActiveWindow>,
    /// A new panel output to place once the compositor lists it.
    windows_unplaced: bool,
    /// Left-to-right order of the monitors, in the headset and in KDE.
    order: Vec<u8>,
    /// How the headset arranges the monitors in rows (see `effective_rows`).
    rows: Vec<LayoutRow>,
    mirror: Option<Mirror>,
    mirror_wanted: bool,
    mirror_retry: Instant,
    /// PC sound to the headset, while one is connected.
    audio: Option<AudioOut>,
    audio_wanted: bool,
    mic: Option<MicIn>,
    mic_wanted: bool,
    /// After a failed start, sound and microphone wait longer and longer before the
    /// next attempt (starting them blocks the session for up to 2 s).
    audio_retry: Backoff,
    mic_retry: Backoff,
    /// Last state told to the headset: a PC app is recording.
    mic_recording: bool,
    /// Connected and worn (encoders running).
    streaming: bool,
    /// When the headset was taken off (as last reported by it).
    taken_off_at: Option<Instant>,
    /// When KDE's primary output was last compared with ours.
    primary_checked: Instant,
    desktop: Desktop,
    pub router: Arc<Router>,
    cfg: EncoderConfig,
    primary: Option<u8>,
    /// Whether to pick KDE's primary monitor ourselves.
    manage_primary: bool,
}

impl Displays {
    pub fn create(saved: &SavedMonitors, cfg: EncoderConfig, manage_primary: bool) -> Result<Self> {
        let desktop = Desktop::connect()?;
        // Only some desktops (KDE) have a primary output.
        let manage_primary = manage_primary && desktop.has_primary();
        let mut displays = Self {
            monitors: BTreeMap::new(),
            windows: BTreeMap::new(),
            windows_checked: Instant::now(),
            windows_origin: HashMap::new(),
            windows_unplaced: false,
            order: Vec::new(),
            rows: Vec::new(),
            mirror: None,
            mirror_wanted: saved.mirror,
            audio: None,
            audio_wanted: saved.audio,
            mic: None,
            mic_wanted: saved.mic,
            audio_retry: Backoff::default(),
            mic_retry: Backoff::default(),
            mic_recording: false,
            streaming: true,
            taken_off_at: None,
            mirror_retry: Instant::now(),
            primary_checked: Instant::now(),
            desktop,
            router: Arc::new(Router::new(cfg.codec)),
            cfg,
            primary: None,
            manage_primary,
        };
        for spec in saved.specs.iter().take(MAX_MONITORS) {
            displays.add(spec.clone())?;
        }
        let ids = displays.order.clone();
        // Rebuild the saved rows over the new ids (same order as the saved specs).
        let mut rest = ids.as_slice();
        for centers in &saved.rows {
            let (row, tail) = rest.split_at(centers.len().min(rest.len()));
            rest = tail;
            if !row.is_empty() {
                displays.rows.push(LayoutRow {
                    ids: row.to_vec(),
                    centers: centers[..row.len()].to_vec(),
                    middles: vec![NEXT; row.len()],
                });
            }
        }
        if let Some(&id) = saved.primary.and_then(|i| ids.get(i)) {
            displays.set_primary(id);
        } else {
            displays.ensure_primary();
        }
        displays.tick();
        displays.sync_wallpapers();
        Ok(displays)
    }

    /// Adds a monitor and returns its id; ids are reused so `Prostir-N` keeps the
    /// position and scale KScreen remembers for that name.
    pub fn add(&mut self, spec: MonitorSpec) -> Result<u8> {
        if self.monitors.len() >= MAX_MONITORS {
            bail!(
                "{} {MAX_MONITORS} {}",
                t("Максимум", "At most"),
                t("моніторів", "monitors")
            );
        }
        let id = (0..=u8::MAX)
            .find(|id| !self.monitors.contains_key(id))
            .expect("fewer than MAX_MONITORS ids are taken");
        let mut output = self.desktop.create(u32::from(id) + 1, &spec)?;
        let encoder = self.start_encoder(id, &output.input())?;
        output.attach(&encoder, self.cfg.backend.takes_dmabuf())?;
        self.monitors.insert(
            id,
            Monitor {
                encoder,
                output,
                spec,
            },
        );
        self.order.push(id);
        self.apply_layout();
        self.router.send(HostMsg::Order {
            ids: self.order.clone(),
        });
        Ok(id)
    }

    fn start_encoder(&self, id: u8, input: &Input) -> Result<Encoder> {
        self.router.add_stream(id);
        let router = self.router.clone();
        Encoder::start(input, &self.cfg, move |ev| router.on_event(id, ev))
            .inspect_err(|_| self.router.remove_stream(id))
    }

    /// Keeps the mirror in step with the physical display: it disappears when the
    /// laptop lid closes and comes back when it opens.
    pub fn tick(&mut self) {
        self.sync_audio();
        self.check_primary();
        self.check_window_panels();
        let broken = self.router.mirror_failed.swap(false, Ordering::Relaxed);
        if self
            .mirror
            .as_ref()
            .is_some_and(|m| broken || m.output.closed())
        {
            info!("mirror stream stopped, restarting it shortly");
            self.stop_mirror();
            self.mirror_retry = Instant::now() + Duration::from_secs(2);
        }
        if !self.mirror_wanted || self.mirror.is_some() || self.mirror_retry > Instant::now() {
            return;
        }
        self.mirror_retry = Instant::now() + Duration::from_secs(2);
        let Some(name) = self.desktop.physical_output() else {
            self.desktop.wake_physical_display();
            return;
        };
        let started = self
            .desktop
            .mirror(u32::from(MIRROR_ID) + 1, &name)
            .and_then(|mut output| {
                let encoder = self.start_encoder(MIRROR_ID, &output.input())?;
                output.attach(&encoder, self.cfg.backend.takes_dmabuf())?;
                Ok(Mirror { encoder, output })
            });
        match started {
            Ok(m) => {
                m.encoder.force_keyframe();
                self.mirror = Some(m);
                self.apply_layout();
            }
            Err(e) => warn!("cannot mirror {name}: {e:#}"),
        }
    }

    /// Makes sure the desktop's primary output (where the Plasma panel lives) is ours.
    /// Right after outputs come and go (a profile switch) KDE can end up with another one.
    fn check_primary(&mut self) {
        if !self.manage_primary || self.primary_checked.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.primary_checked = Instant::now();
        let Some(m) = self.primary.and_then(|id| self.monitors.get(&id)) else {
            return;
        };
        self.desktop.keep_primary(&m.output);
    }

    fn stop_mirror(&mut self) {
        if self.mirror.take().is_some() {
            self.router.remove_stream(MIRROR_ID);
        }
    }

    pub fn set_mirror(&mut self, enabled: bool) {
        self.mirror_wanted = enabled;
        self.mirror_retry = Instant::now();
        if !enabled {
            self.stop_mirror();
        }
        self.tick();
        self.router.send(HostMsg::MirrorWanted { enabled });
    }

    /// Clearer text (slower preset) for every running encoder and monitors added later.
    pub fn set_sharp(&mut self, sharp: bool) {
        self.cfg.sharp = sharp;
        let value = self.cfg.target_usage();
        for (_, e) in self.encoders() {
            e.set_target_usage(value);
        }
    }

    /// How the PC encodes, for the headset menu.
    pub fn pc_settings(&self) -> PcSettings {
        PcSettings {
            quality: self.cfg.bitrate_kbps / 1000,
            codec: self.cfg.codec,
            software: self.cfg.backend == crate::encoder::Backend::X264,
            sharp: self.cfg.sharp,
        }
    }

    /// Tells the headset how the PC encodes and what it could encode with.
    pub fn announce_pc_settings(&self) {
        self.router.send(HostMsg::PcSettings {
            settings: self.pc_settings(),
            encoders: crate::encoder::available(),
        });
        let mut bits = 0;
        if windows::can_move_cursor() {
            bits |= prostir_proto::abilities::MOVE_MOUSE;
        }
        self.router.send(HostMsg::Abilities { bits });
    }

    /// Our outputs (monitors, window panels, the mirror) with their desktop geometry.
    fn output_geometries(&self) -> Vec<(u8, Geometry)> {
        let geometries = self.desktop.geometries();
        let mirror = self.mirror.iter().map(|m| (MIRROR_ID, &m.output));
        self.monitors
            .iter()
            .chain(&self.windows)
            .map(|(id, m)| (*id, &m.output))
            .chain(mirror)
            .filter_map(|(id, o)| Some((id, *geometries.get(o.output_name())?)))
            .collect()
    }

    /// Where the mouse is: the monitor and the point across its image (0..=65535).
    pub fn find_mouse(&self) -> Option<(u8, u16, u16)> {
        let (x, y) = windows::cursor()?;
        let frac = |v: i32, start: i32, len: i32| {
            ((f64::from(v - start) / f64::from(len.max(1))).clamp(0.0, 1.0) * 65535.0) as u16
        };
        self.output_geometries()
            .into_iter()
            .find(|(_, g)| x >= g.x && x < g.x + g.w && y >= g.y && y < g.y + g.h)
            .map(|(id, g)| (id, frac(x, g.x, g.w), frac(y, g.y, g.h)))
    }

    /// Puts the mouse on monitor `id` at `x`/`y` across its image (0..=65535).
    pub fn mouse_to(&self, id: u8, x: u16, y: u16) {
        let Some((_, g)) = self.output_geometries().into_iter().find(|(i, _)| *i == id) else {
            warn!("mouse: no monitor {id} on the desktop");
            return;
        };
        let at =
            |start: i32, len: i32, f: u16| start + (f64::from(len) * f64::from(f) / 65535.0) as i32;
        let (cx, cy) = (at(g.x, g.w - 1, x), at(g.y, g.h - 1, y));
        if windows::move_cursor(cx, cy) {
            info!("mouse moved to monitor {id} at {cx},{cy}");
        } else {
            warn!("could not move the mouse to {cx},{cy}");
        }
    }

    /// Settings changed in the headset menu. Quality and the text preset apply on the
    /// fly; another codec or the CPU encoder restarts the encoders (the monitors and
    /// their windows stay).
    pub fn set_pc_settings(&mut self, wanted: PcSettings) -> Result<()> {
        let quality = wanted.quality.clamp(5, 100);
        if quality != self.cfg.bitrate_kbps / 1000 {
            self.set_bitrate(quality);
        }
        if wanted.sharp != self.cfg.sharp {
            self.set_sharp(wanted.sharp);
        }
        let current = self.pc_settings();
        if wanted.codec != current.codec || wanted.software != current.software {
            let (backend, codec) = crate::encoder::pick(wanted.codec, wanted.software)?;
            info!("encoding with {backend:?} {codec:?} (from the headset menu)");
            self.cfg.backend = backend;
            self.cfg.codec = codec;
            self.router.codec.store(codec as u8, Ordering::Relaxed);
            self.restart_encoders()?;
        }
        self.save();
        self.announce_pc_settings();
        Ok(())
    }

    /// New encoders for every stream with the current settings. Each old one stops
    /// before its replacement starts, so the headset never gets frames of two codecs
    /// for one stream.
    fn restart_encoders(&mut self) -> Result<()> {
        let ids: Vec<u8> = self.monitors.keys().copied().collect();
        for id in ids {
            let Some(Monitor {
                encoder,
                mut output,
                spec,
            }) = self.monitors.remove(&id)
            else {
                continue;
            };
            output.detach();
            drop(encoder);
            let encoder = self.start_encoder(id, &output.input())?;
            output.attach(&encoder, self.cfg.backend.takes_dmabuf())?;
            encoder.force_keyframe();
            self.monitors.insert(
                id,
                Monitor {
                    encoder,
                    output,
                    spec,
                },
            );
        }
        // Window panels are made again when wanted; the mirror restarts by itself.
        self.close_window_panels();
        self.stop_mirror();
        self.mirror_retry = Instant::now();
        Ok(())
    }

    /// Applies a new bitrate to every running encoder and to monitors added later.
    pub fn set_bitrate(&mut self, mbps: u32) {
        self.cfg.bitrate_kbps = mbps * 1000;
        for (_, e) in self.encoders() {
            e.set_bitrate(self.cfg.bitrate_kbps);
        }
    }

    pub fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    pub fn mirror_active(&self) -> bool {
        self.mirror.is_some()
    }

    /// Streams while a headset is connected and worn; otherwise the encoders pause and
    /// sound and microphone go back to the PC (the monitors and windows stay).
    pub fn set_streaming(&mut self, on: bool) {
        if on != self.streaming {
            self.streaming = on;
            info!(
                "{}",
                if on {
                    "streaming"
                } else {
                    "paused: headset not worn or gone"
                }
            );
            for (_, e) in self.encoders() {
                e.set_paused(!on);
            }
        }
        self.sync_audio();
    }

    /// The headset reports being put on or taken off.
    pub fn set_worn(&mut self, worn: bool) {
        self.taken_off_at = match (worn, self.taken_off_at) {
            (true, _) => None,
            (false, Some(t)) => Some(t),
            (false, None) => Some(Instant::now()),
        };
        self.set_streaming(worn);
    }

    /// How long the headset has been off the head, if it is.
    pub fn taken_off_for(&self) -> Option<Duration> {
        self.taken_off_at.map(|t| t.elapsed())
    }

    pub fn streaming(&self) -> bool {
        self.streaming
    }

    /// Starts or stops PC sound and microphone to match the wishes and the state.
    fn sync_audio(&mut self) {
        let audio = self.streaming && self.audio_wanted;
        if audio && self.audio.is_none() && self.audio_retry.due() {
            let router = self.router.clone();
            match AudioOut::start(move |pcm| router.send_audio(pcm)) {
                Ok(a) => {
                    self.audio = Some(a);
                    self.audio_retry.succeeded();
                }
                Err(e) => {
                    warn!("no sound in the headset: {e:#}");
                    self.audio_retry.failed();
                }
            }
        } else if !audio {
            self.audio = None;
        }
        let mic = self.streaming && self.mic_wanted;
        if mic && self.mic.is_none() && self.mic_retry.due() {
            match MicIn::start() {
                Ok(m) => {
                    self.mic = Some(m);
                    self.mic_retry.succeeded();
                }
                Err(e) => {
                    warn!("no headset microphone: {e:#}");
                    self.mic_retry.failed();
                }
            }
        } else if !mic {
            self.mic = None;
        }
        // The headset records only while some PC app listens.
        let recording = self.mic.as_ref().is_some_and(MicIn::in_use);
        if recording != self.mic_recording {
            self.mic_recording = recording;
            self.router.send(HostMsg::MicActive { active: recording });
        }
    }

    pub fn push_mic(&self, pcm: Vec<u8>) {
        if let Some(m) = &self.mic {
            m.push(pcm);
        }
    }

    pub fn set_audio_wanted(&mut self, enabled: bool) {
        self.audio_wanted = enabled;
        // Switched on by hand: try right away.
        self.audio_retry.succeeded();
        self.sync_audio();
        self.router.send(HostMsg::AudioWanted { enabled });
    }

    pub fn set_mic_wanted(&mut self, enabled: bool) {
        self.mic_wanted = enabled;
        self.mic_retry.succeeded();
        self.sync_audio();
        self.router.send(HostMsg::MicWanted { enabled });
    }

    /// The id the headset knows an output by (`MIRROR_ID` for the physical display).
    pub fn id_of_output(&self, name: &str) -> Option<u8> {
        if let Some((id, _)) = self
            .monitors
            .iter()
            .chain(&self.windows)
            .find(|(_, m)| m.output.output_name() == name)
        {
            return Some(*id);
        }
        self.mirror
            .as_ref()
            .filter(|m| m.output.output_name() == name)
            .map(|_| MIRROR_ID)
    }

    pub fn mic_wanted(&self) -> bool {
        self.mic_wanted
    }

    pub fn audio_wanted(&self) -> bool {
        self.audio_wanted
    }

    pub fn mirror_wanted(&self) -> bool {
        self.mirror_wanted
    }

    pub fn remove(&mut self, id: u8) -> Result<()> {
        if !self.monitors.contains_key(&id) {
            bail!(
                "{} {}",
                t("Немає монітора", "No monitor"),
                u32::from(id) + 1
            );
        }
        if self.monitors.len() == 1 {
            bail!(
                "{}",
                t(
                    "Останній монітор прибрати не можна",
                    "The last monitor cannot be removed"
                )
            );
        }
        self.router.remove_stream(id);
        self.monitors.remove(&id);
        self.order.retain(|o| *o != id);
        self.router.send(HostMsg::Order {
            ids: self.order.clone(),
        });
        if self.primary == Some(id) {
            self.primary = None;
        }
        self.ensure_primary();
        // Close the gap the monitor leaves.
        self.apply_layout();
        Ok(())
    }

    pub fn set_primary(&mut self, id: u8) {
        let Some(m) = self.monitors.get(&id) else {
            return;
        };
        if self.manage_primary {
            m.output.make_primary();
        }
        self.primary = Some(id);
        self.router.send(HostMsg::PrimaryChanged { id });
        self.apply_layout();
    }

    /// Lays the outputs out on the desktop like they stand in the headset: Quest
    /// monitors side by side in `order`, vertically centred, and the mirrored display
    /// under the primary monitor. The mouse then crosses screens where the eyes expect
    /// it to.
    fn apply_layout(&self) {
        let geometries = self.desktop.geometries();
        let size = |name: &str, spec: Option<&MonitorSpec>| {
            geometries.get(name).map(|g| (g.w, g.h)).or_else(|| {
                spec.map(|s| {
                    (
                        (s.width as f64 / s.scale).round() as i32,
                        (s.height as f64 / s.scale).round() as i32,
                    )
                })
            })
        };
        // As the headset shows them (see `arrange`).
        let mut names: HashMap<u8, String> = self
            .monitors
            .iter()
            .map(|(id, m)| (*id, m.output.output_name().to_owned()))
            .collect();
        if let Some(m) = &self.mirror {
            names.insert(MIRROR_ID, m.output.output_name().to_owned());
        }
        let rows: Vec<Vec<Seat>> = self
            .effective_rows()
            .into_iter()
            .map(|r| {
                r.ids
                    .iter()
                    .zip(&r.centers)
                    .zip(&r.middles)
                    .filter_map(|((id, center), middle)| {
                        let spec = self.monitors.get(id).map(|m| &m.spec);
                        let (w, h) = size(names.get(id)?, spec)?;
                        Some(Seat {
                            id: *id,
                            center: *center,
                            middle: *middle,
                            w,
                            h,
                        })
                    })
                    .collect()
            })
            .collect();
        let main = self
            .primary
            .or_else(|| self.order.first().copied())
            .unwrap_or(0);
        let mut planned: Vec<(u8, String, i32, i32, i32, i32)> = Vec::new();
        let mut top = 0;
        // The headset said where each monitor is seen: the desktop follows that
        // directly. Older headsets (or a saved layout): rows.
        let seats: Vec<Seat> = rows.iter().flatten().copied().collect();
        let arranged =
            if !seats.is_empty() && seats.iter().all(|m| m.center != NEXT && m.middle != NEXT) {
                arrange_free(&seats, main)
            } else {
                arrange(&rows, main)
            };
        for (id, x, y) in arranged {
            if let Some(seat) = rows.iter().flatten().find(|m| m.id == id) {
                planned.push((id, names[&id].clone(), x, y, seat.w, seat.h));
                top = top.max(y + seat.h);
            }
        }
        // KDE wants the desktop to start at 0,0.
        let left = planned.iter().map(|p| p.2).min().unwrap_or(0);
        // Every applied change makes KWin reconfigure its outputs, which also cuts the
        // laptop mirror's stream: `place` only sends what actually differs.
        let mut positions = Vec::new();
        let mut placed = HashMap::new();
        for (id, name, x, y, w, _) in &planned {
            positions.push((name.clone(), x - left, *y));
            placed.insert(*id, (x - left, *w));
        }
        // The laptop screen under the primary monitor, below every row.
        let primary_at = self.primary.and_then(|p| placed.get(&p)).copied();
        let mut bottom_x = primary_at.map_or(0, |(px, _)| px);
        let mirror_name = self
            .mirror
            .as_ref()
            .map(|m| m.output.output_name().to_owned());
        // Unless the headset put it somewhere itself (it is then in a row).
        let mirror_placed = planned.iter().any(|p| p.0 == MIRROR_ID);
        if let (Some(name), Some((px, pw)), false) = (&mirror_name, primary_at, mirror_placed)
            && let Some((w, _)) = size(name, None)
        {
            let x = (px + (pw - w) / 2).max(0);
            positions.push((name.clone(), x, top));
            bottom_x = x + w;
        }
        // Other real displays (the laptop panel next to a mirrored external monitor,
        // say) go on the bottom row too, so nothing overlaps the Prostir monitors.
        let ours: Vec<&str> = self
            .monitors
            .values()
            .map(|m| m.output.output_name())
            .collect();
        let mut others: Vec<(&String, &Geometry)> = geometries
            .iter()
            .filter(|(name, g)| {
                g.enabled
                    && g.w > 0
                    && !ours.contains(&name.as_str())
                    && Some(name.as_str()) != mirror_name.as_deref()
                    && !self.desktop.is_virtual(name)
            })
            .collect();
        others.sort_by_key(|(name, _)| name.as_str());
        for (name, g) in others {
            positions.push((name.clone(), bottom_x, top));
            bottom_x += g.w;
        }
        // Window panels in a column right of everything: the mouse reaches them
        // past the right edge.
        let right = planned.iter().map(|p| p.2 - left + p.4).max().unwrap_or(0);
        let mut panel_y = 0;
        for m in self.windows.values() {
            let name = m.output.output_name();
            if let Some((_, h)) = size(name, Some(&m.spec)) {
                positions.push((name.to_owned(), right, panel_y));
                panel_y += h;
            }
        }
        let primary = self
            .primary
            .and_then(|p| self.monitors.get(&p))
            .filter(|_| self.manage_primary)
            .map(|m| m.output.output_name());
        self.desktop.place(&positions, primary, &geometries);
    }

    /// Changes the left-to-right order (ids of all current monitors).
    /// Makes the monitors match `sizes` left to right (a headset profile). Monitors of a
    /// matching size are kept, so their windows stay where they are.
    pub fn set_monitors(&mut self, sizes: &[(u32, u32)], primary: usize) -> Result<()> {
        if sizes.is_empty() || sizes.len() > MAX_MONITORS {
            bail!("1–{MAX_MONITORS} monitors");
        }
        let specs: Vec<MonitorSpec> = sizes
            .iter()
            .map(|&(width, height)| MonitorSpec {
                width,
                height,
                scale: 1.0,
            })
            .collect();
        for s in &specs {
            s.validate()?;
        }
        let mut unused: Vec<u8> = self.order.clone();
        let mut plan: Vec<Option<u8>> = specs
            .iter()
            .map(|s| {
                let i = unused.iter().position(|id| {
                    let have = &self.monitors[id].spec;
                    have.width == s.width && have.height == s.height
                })?;
                Some(unused.remove(i))
            })
            .collect();
        // Remove before adding (to stay under the limit), but never the last one.
        let mut last = None;
        for id in unused {
            if self.monitors.len() > 1 {
                self.remove(id)?;
            } else {
                last = Some(id);
            }
        }
        for (slot, spec) in plan.iter_mut().zip(&specs) {
            if slot.is_none() {
                *slot = Some(self.add(spec.clone())?);
            }
        }
        if let Some(id) = last {
            self.remove(id)?;
        }
        let ids: Vec<u8> = plan.into_iter().flatten().collect();
        self.set_order(&ids)?;
        if let Some(&p) = ids.get(primary) {
            self.set_primary(p);
        }
        Ok(())
    }

    /// The headset's rows, kept in step with the monitors: gone ones dropped, new ones
    /// (not placed by the headset yet) at the end of the bottom row.
    fn effective_rows(&self) -> Vec<LayoutRow> {
        let mut rows: Vec<LayoutRow> = self
            .rows
            .iter()
            .map(|r| {
                let mut row = LayoutRow {
                    ids: Vec::new(),
                    centers: Vec::new(),
                    middles: Vec::new(),
                };
                let centers = r.centers.iter().chain(std::iter::repeat(&NEXT));
                let middles = r.middles.iter().chain(std::iter::repeat(&NEXT));
                for ((id, c), m) in r.ids.iter().zip(centers).zip(middles) {
                    // The laptop screen too, where the headset put it (while it shows).
                    if self.monitors.contains_key(id) || (*id == MIRROR_ID && self.mirror.is_some())
                    {
                        row.ids.push(*id);
                        row.centers.push(*c);
                        row.middles.push(*m);
                    }
                }
                row
            })
            .filter(|r| !r.ids.is_empty())
            .collect();
        let placed: Vec<u8> = rows.iter().flat_map(|r| r.ids.clone()).collect();
        let missing: Vec<u8> = self
            .order
            .iter()
            .copied()
            .filter(|id| !placed.contains(id) && self.monitors.contains_key(id))
            .collect();
        if !missing.is_empty() {
            let centers = vec![NEXT; missing.len()];
            match rows.last_mut() {
                Some(last) => {
                    last.ids.extend(missing);
                    last.middles.extend(centers.iter().map(|_| NEXT));
                    last.centers.extend(centers);
                }
                None => rows.push(LayoutRow {
                    middles: vec![NEXT; missing.len()],
                    ids: missing,
                    centers,
                }),
            }
        }
        rows
    }

    /// Arranges KDE's outputs in the rows the headset shows them in.
    pub fn set_layout(&mut self, mut rows: Vec<LayoutRow>) {
        // Offsets are desktop pixels; anything this far out is nonsense (and would
        // overflow the arithmetic below).
        for r in &mut rows {
            for c in r.centers.iter_mut().chain(r.middles.iter_mut()) {
                if *c != NEXT {
                    *c = (*c).clamp(-100_000, 100_000);
                }
            }
        }
        // The laptop screen's stream is cut for a moment whenever KDE rearranges its
        // outputs, and a headset that sends a layout meanwhile leaves it out. Its spot
        // is kept, or it would come back elsewhere, the headset would move it back, and
        // that would cut it again, round and round.
        let has_mirror = |rows: &[LayoutRow]| rows.iter().any(|r| r.ids.contains(&MIRROR_ID));
        if self.mirror_wanted && self.mirror.is_none() && !has_mirror(&rows) {
            for (i, old) in self.rows.iter().enumerate() {
                let Some(k) = old.ids.iter().position(|id| *id == MIRROR_ID) else {
                    continue;
                };
                let (center, middle) = (
                    old.centers.get(k).copied().unwrap_or(NEXT),
                    old.middles.get(k).copied().unwrap_or(NEXT),
                );
                let at = if i < rows.len() {
                    i
                } else {
                    rows.push(LayoutRow::default());
                    rows.len() - 1
                };
                let row = &mut rows[at];
                row.ids.push(MIRROR_ID);
                row.centers.push(center);
                row.middles.push(middle);
                break;
            }
        }
        self.rows = rows;
        let rows = self.effective_rows();
        self.order = rows
            .iter()
            .flat_map(|r| r.ids.clone())
            .filter(|id| *id != MIRROR_ID)
            .collect();
        self.apply_layout();
        self.router.send(HostMsg::Order {
            ids: self.order.clone(),
        });
    }

    pub fn set_order(&mut self, ids: &[u8]) -> Result<()> {
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        let mut current: Vec<u8> = self.monitors.keys().copied().collect();
        current.sort_unstable();
        if sorted != current {
            bail!(
                "{}",
                t(
                    "Порядок не збігається з моніторами",
                    "The order does not match the monitors"
                )
            );
        }
        self.order = ids.to_vec();
        // One row, in this order.
        self.rows = vec![LayoutRow {
            ids: ids.to_vec(),
            centers: vec![NEXT; ids.len()],
            middles: vec![NEXT; ids.len()],
        }];
        self.apply_layout();
        self.router.send(HostMsg::Order {
            ids: self.order.clone(),
        });
        Ok(())
    }

    pub fn order(&self) -> &[u8] {
        &self.order
    }

    /// Without a chosen primary, the middle monitor gets the Plasma panel.
    fn ensure_primary(&mut self) {
        if self.primary.is_some_and(|p| self.monitors.contains_key(&p)) {
            return;
        }
        let ids = self.order.clone();
        if let Some(&id) = ids.get(ids.len().saturating_sub(1) / 2) {
            self.set_primary(id);
        } else {
            self.apply_layout();
        }
    }

    pub fn primary(&self) -> Option<u8> {
        self.primary
    }

    /// Gives every screen the wallpaper of the primary desktop (Plasma configures
    /// wallpapers per screen, so new monitors would otherwise show the default one).
    pub fn sync_wallpapers(&self) {
        self.desktop.sync_wallpapers();
    }

    fn encoders(&self) -> impl Iterator<Item = (u8, &Encoder)> {
        let mirror = self.mirror.iter().map(|m| (MIRROR_ID, &m.encoder));
        self.monitors
            .iter()
            .chain(&self.windows)
            .map(|(id, m)| (*id, &m.encoder))
            .chain(mirror)
    }

    /// Puts the PC's active window on a panel of its own: an output its size, which
    /// the headset shows as a floating panel. Returns the panel's id.
    pub fn detach_window(&mut self) -> Result<u8> {
        let Some(id) = WINDOW_IDS.clone().find(|id| !self.windows.contains_key(id)) else {
            bail!(
                "{}",
                t(
                    "Відкріплених вікон уже три: закрийте одне",
                    "Three windows are detached already: close one first"
                )
            );
        };
        let window = windows::active().context(t(
            "Немає активного вікна на ПК",
            "No active window on the PC",
        ))?;
        let (width, height) = panel_size(&window);
        let spec = MonitorSpec {
            width,
            height,
            scale: window.scale,
        };
        let mut output = self.desktop.create(u32::from(id) + 1, &spec)?;
        let encoder = self.start_encoder(id, &output.input())?;
        if let Err(e) = output.attach(&encoder, self.cfg.backend.takes_dmabuf()) {
            self.router.remove_stream(id);
            return Err(e);
        }
        let name = output.output_name().to_owned();
        self.windows.insert(
            id,
            Monitor {
                encoder,
                output,
                spec,
            },
        );
        self.apply_layout();
        if let Err(e) = windows::move_to(&window, &name, true) {
            self.close_window_panel(id);
            return Err(e);
        }
        if !self.streaming {
            self.windows[&id].encoder.set_paused(true);
        }
        self.windows_origin.insert(id, window);
        self.windows_checked = Instant::now() + Duration::from_secs(1);
        // Hyprland lists a new output a moment later: placed again at the next check.
        self.windows_unplaced = true;
        info!("window panel {id} on {name} ({width}x{height})");
        Ok(id)
    }

    /// Removes a window panel; its window goes back where it was detached from (or,
    /// when that monitor is gone, to the others).
    pub fn close_window_panel(&mut self, id: u8) {
        self.close_panel(id, true);
    }

    fn close_panel(&mut self, id: u8, put_back: bool) {
        let origin = self.windows_origin.remove(&id);
        if let Some(m) = self.windows.remove(&id) {
            windows::forget_panel(m.output.output_name());
            if put_back
                && let Some(w) = origin.filter(|w| !w.output.is_empty())
                && let Err(e) = windows::put_back(&w, &w.output)
            {
                warn!("cannot put the window back on {}: {e:#}", w.output);
            }
            self.router.remove_stream(id);
            drop(m);
            self.apply_layout();
        }
    }

    fn close_window_panels(&mut self) {
        let ids: Vec<u8> = self.windows.keys().copied().collect();
        for id in ids {
            self.close_window_panel(id);
        }
    }

    /// A panel whose window was closed (or moved away) goes too.
    fn check_window_panels(&mut self) {
        if self.windows.is_empty() || self.windows_checked > Instant::now() {
            return;
        }
        self.windows_checked = Instant::now() + Duration::from_secs(1);
        if std::mem::take(&mut self.windows_unplaced) {
            self.apply_layout();
        }
        let names: Vec<String> = self
            .windows
            .values()
            .map(|m| m.output.output_name().to_owned())
            .collect();
        for name in windows::vacated(&names) {
            if let Some((&id, _)) = self
                .windows
                .iter()
                .find(|(_, m)| m.output.output_name() == name)
            {
                info!("the window on panel {id} is gone: closing the panel");
                self.close_panel(id, false);
            }
        }
    }

    /// Moves the PC's active window to monitor `id` (the one the user looks at).
    pub fn window_here(&self, id: u8) -> Result<()> {
        let output = self
            .monitors
            .get(&id)
            .map(|m| m.output.output_name().to_owned())
            .or_else(|| {
                self.mirror
                    .as_ref()
                    .filter(|_| id == MIRROR_ID)
                    .map(|m| m.output.output_name().to_owned())
            })
            .context(t("Немає такого монітора", "No such monitor"))?;
        let window = windows::active().context(t(
            "Немає активного вікна на ПК",
            "No active window on the PC",
        ))?;
        windows::move_to(&window, &output, false)
    }

    /// Saves a picture of monitor `id` (all of them where only that is possible).
    pub fn screenshot(&self, id: u8) -> Result<std::path::PathBuf> {
        let monitor = self.monitors.get(&id).or_else(|| self.windows.get(&id));
        // Where the desktop's screenshot tool cannot take one output by name, the
        // monitor's own stream can. A fresh keyframe, decoded into a PNG; the whole
        // desktop if not.
        if !self.desktop.screenshots_one_output()
            && let Some(m) = monitor
            && self.streaming
        {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            *self.router.snapshot.lock().unwrap() = Some((id, tx));
            m.encoder.force_keyframe();
            let frame = rx.recv_timeout(Duration::from_secs(2));
            self.router.snapshot.lock().unwrap().take();
            let codec = if self.router.codec.load(Ordering::Relaxed) == Codec::H264 as u8 {
                Codec::H264
            } else {
                Codec::H265
            };
            match frame
                .map_err(anyhow::Error::from)
                .and_then(|data| crate::encoder::keyframe_to_png(&data, codec))
            {
                Ok(path) => return Ok(path),
                Err(e) => warn!("screenshot from the stream: {e:#}; taking the whole desktop"),
            }
        }
        windows::screenshot(monitor.map(|m| m.output.output_name()))
    }

    pub fn force_keyframe(&self, id: u8) {
        if let Some((_, e)) = self.encoders().find(|(i, _)| *i == id) {
            e.force_keyframe();
        }
    }

    pub fn force_keyframes(&self) {
        for (_, e) in self.encoders() {
            e.force_keyframe();
        }
    }

    pub fn healthy(&self) -> bool {
        !self.router.failed.load(Ordering::Relaxed)
            && !self
                .monitors
                .values()
                .chain(self.windows.values())
                .any(|m| m.output.closed())
    }

    /// Remembers the set for the next session.
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        let text: String = self
            .effective_rows()
            .iter()
            .flat_map(|row| {
                std::iter::once("row\n".to_owned()).chain(
                    row.ids
                        .iter()
                        .zip(&row.centers)
                        .filter_map(|(id, c)| Some((id, c, self.monitors.get(id)?)))
                        .map(|(id, c, m)| {
                            let star = if self.primary == Some(*id) { " *" } else { "" };
                            let at = if *c == NEXT {
                                String::new()
                            } else {
                                format!(" at {c}")
                            };
                            format!("{}{at}{star}\n", m.spec)
                        }),
                )
            })
            .chain(std::iter::once(format!(
                "mirror {}\naudio {}\nmic {}\nbitrate {}\nsharp {}\ncodec {}\nsoftware {}\n",
                if self.mirror_wanted { "on" } else { "off" },
                if self.audio_wanted { "on" } else { "off" },
                if self.mic_wanted { "on" } else { "off" },
                self.cfg.bitrate_kbps / 1000,
                if self.cfg.sharp { "on" } else { "off" },
                match self.cfg.codec {
                    Codec::H264 => "h264",
                    Codec::H265 => "h265",
                },
                if self.cfg.backend == crate::encoder::Backend::X264 {
                    "on"
                } else {
                    "off"
                }
            )))
            .collect();
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| prostir_proto::secure::write_atomic(&path, text.as_bytes()));
        if let Err(e) = result {
            warn!("cannot save {}: {e}", path.display());
        }
    }
}
