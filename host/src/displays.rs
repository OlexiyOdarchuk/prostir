//! The set of virtual monitors, their encoders, and delivery to the headset.

use crate::i18n::t;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{debug, error, info, warn};
use prostir_proto::stats::{Histogram, Period};
use prostir_proto::{
    Codec, HostMsg, LayoutEntry, LayoutRow, MAX_MONITORS, MIRROR_ID, PcSettings, WINDOW_IDS,
};

use crate::encoder::{Encoder, EncoderConfig, EncoderEvent};
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

/// How often the router logs what each stream sent (see [`Router::report`]).
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Per-stream delivery state for the currently connected client.
struct StreamSlot {
    size: Option<(u32, u32)>,
    /// What the encoder makes, announced with the stream.
    codec: Codec,
    /// The current size and codec were announced to the headset.
    announced: bool,
    /// The headset has this stream (announced on this connection, perhaps by an
    /// encoder since replaced), so it is told when the stream ends.
    known: bool,
    need_keyframe: bool,
    /// When a keyframe was last asked for: asking again on every frame while waiting
    /// would make the encoder emit one huge keyframe after another.
    keyframe_asked: Option<Instant>,
    /// pts → when the encoded frame was queued for sending.
    sent: BTreeMap<u64, Instant>,
    /// Encoded on the PC → handed to the headset's compositor, as the headset reports
    /// back with `FrameShown` (for some frames only).
    shown: Histogram,
    /// Frames not queued because the stream waited for a keyframe.
    skipped: u32,
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

    fn new(codec: Codec, known: bool) -> Self {
        Self {
            size: None,
            codec,
            announced: false,
            known,
            need_keyframe: true,
            keyframe_asked: None,
            sent: BTreeMap::new(),
            shown: Histogram::default(),
            skipped: 0,
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

/// How many bytes a second the connection may send for `streams` encoders of
/// `kbps_each`: one and a half times what they make, plus 4 Mbit/s for sound and
/// control, and never under 20 Mbit/s. Without a limit TCP pushes a whole keyframe at
/// once into the queue of the Wi-Fi access point, and sound waits behind it there,
/// where nothing on the PC can overtake it; paced, a keyframe takes some 30 to 50 ms,
/// and sound goes between its packets. A rule of thumb until the link is measured
/// (an estimate of the link replaces it later).
pub fn pace_for(streams: usize, kbps_each: u32) -> u64 {
    let video = streams as u64 * u64::from(kbps_each) * 1000;
    let bits = (video * 3 / 2 + 4_000_000).max(20_000_000);
    bits / 8
}

/// Delivers encoder output to whichever client is connected.
pub struct Router {
    /// A screenshot waiting for the next keyframe of stream `id` (see `screenshot`).
    snapshot: Mutex<Option<(u8, std::sync::mpsc::SyncSender<Vec<u8>>)>>,
    inner: Mutex<RouterInner>,
    failed: AtomicBool,
    mirror_failed: AtomicBool,
}

struct RouterInner {
    tx: Option<Arc<Outbox>>,
    streams: BTreeMap<u8, StreamSlot>,
    report: Period,
}

impl Router {
    fn new() -> Self {
        Self {
            inner: Mutex::new(RouterInner {
                tx: None,
                streams: BTreeMap::new(),
                report: Period::new(REPORT_EVERY),
            }),
            failed: AtomicBool::new(false),
            mirror_failed: AtomicBool::new(false),
            snapshot: Mutex::new(None),
        }
    }

    pub fn attach(&self, tx: Arc<Outbox>) {
        let mut inner = self.inner.lock().unwrap();
        inner.tx = Some(tx);
        inner.report = Period::new(REPORT_EVERY);
        for s in inner.streams.values_mut() {
            s.announced = false;
            s.known = false;
            s.need_keyframe = true;
            s.shown = Histogram::default();
            s.skipped = 0;
        }
    }

    /// Logs, every [`REPORT_EVERY`] while a headset is connected, one line per stream:
    /// what the outbox sent and dropped, how long frames waited there, keyframes, and
    /// encoded → shown in the headset. Then one line for sound and one for the round
    /// trip of the pings.
    pub fn report(&self) {
        let (window, mut stats, streams) = {
            let mut inner = self.inner.lock().unwrap();
            let RouterInner {
                tx,
                streams,
                report,
            } = &mut *inner;
            let Some(tx) = tx else { return };
            let Some(window) = report.due(Instant::now()) else {
                return;
            };
            let streams: Vec<(u8, Histogram, u32)> = streams
                .iter_mut()
                .map(|(id, slot)| {
                    let shown = std::mem::take(&mut slot.shown);
                    (*id, shown, std::mem::take(&mut slot.skipped))
                })
                .collect();
            (window, tx.take_stats(), streams)
        };
        // Logged without the lock, which every encoded frame takes.
        for (id, shown, skipped) in streams {
            let video = stats.video.remove(&id).unwrap_or_default();
            if video.frames == 0 && video.dropped.total() == 0 && skipped == 0 {
                continue;
            }
            let shown = match shown.count() {
                0 => "no reports".to_owned(),
                n => format!("{} ({n} reports)", shown.summary_ms()),
            };
            info!(
                "{}: {}; {skipped} skipped waiting for a keyframe; encoded → shown in headset {shown}",
                stream_name(id),
                video.summary(window),
            );
        }
        if stats.audio_wait.count() > 0 || stats.audio_dropped > 0 {
            info!(
                "sound: sent {} slices, waited in outbox {}; dropped {}",
                stats.audio_wait.count(),
                stats.audio_wait.summary_ms(),
                stats.audio_dropped
            );
        }
        info!(
            "link: round trip {} ({} pings answered)",
            stats.round_trip.summary_ms(),
            stats.round_trip.count()
        );
    }

    pub fn detach(&self) {
        self.inner.lock().unwrap().tx = None;
    }

    /// How fast the connection may send (see [`pace_for`]).
    pub fn set_pace(&self, bytes_per_sec: u64) {
        if let Some(tx) = &self.inner.lock().unwrap().tx {
            tx.set_pace(bytes_per_sec);
        }
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

    /// Stream `id` starts, or starts over with a new encoder making `codec`.
    fn add_stream(&self, id: u8, codec: Codec) {
        let mut inner = self.inner.lock().unwrap();
        let known = inner.streams.get(&id).is_some_and(|s| s.known);
        inner.streams.insert(id, StreamSlot::new(codec, known));
    }

    fn remove_stream(&self, id: u8) {
        let mut inner = self.inner.lock().unwrap();
        let known = inner.streams.remove(&id).is_some_and(|s| s.known);
        if known
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
            Some(sent) => slot.shown.add(sent.elapsed()),
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
        let RouterInner { tx, streams, .. } = &mut *inner;
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
                        codec: slot.codec,
                        name: stream_name(id),
                    };
                    if !sender.control(start) {
                        *tx = None;
                        return false;
                    }
                    slot.announced = true;
                    slot.known = true;
                }
                if slot.need_keyframe && !keyframe {
                    slot.skipped += 1;
                    return slot.ask_keyframe();
                }
                let msg = HostMsg::Frame {
                    id,
                    pts_us,
                    keyframe,
                    data,
                };
                if sender.video(id, keyframe, msg) {
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

/// Saved monitor set: `row` starts a row (top to bottom), then one
/// `WIDTHxHEIGHT[@scale][ at CENTER[ MIDDLE]][ *]` per monitor, left to right: where
/// the headset showed it (`next` for an unknown centre) and `*` for the primary one.
/// Lines like `mirror on|off` keep the other choices.
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
    /// Rows from top to bottom, covering `specs` in order: where each monitor in it is
    /// seen, (centre, middle) as in [`LayoutEntry`]. Empty: one row.
    pub rows: Vec<Vec<(Option<i32>, Option<i32>)>>,
}

impl Default for SavedMonitors {
    fn default() -> Self {
        Self {
            specs: Vec::new(),
            primary: None,
            mirror: true,
            audio: true,
            mic: true,
            bitrate: None,
            sharp: false,
            codec: None,
            software: false,
            rows: Vec::new(),
        }
    }
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

impl SavedMonitors {
    /// What was saved last, with or without monitors; the defaults before the first
    /// save.
    pub fn load(parse: impl Fn(&str) -> Result<MonitorSpec, String>) -> Self {
        config_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| Self::parse(&text, parse))
            .unwrap_or_default()
    }

    fn parse(text: &str, parse: impl Fn(&str) -> Result<MonitorSpec, String>) -> Self {
        let mut saved = Self::default();
        let on = |v: &str| v.trim() == "on";
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if line == "row" || line.starts_with("row ") {
                saved.rows.push(Vec::new());
                continue;
            }
            if let Some(v) = line.strip_prefix("sharp ") {
                saved.sharp = on(v);
                continue;
            }
            if let Some(v) = line.strip_prefix("codec ") {
                saved.codec = match v.trim() {
                    "h264" => Some(Codec::H264),
                    "h265" => Some(Codec::H265),
                    _ => None,
                };
                continue;
            }
            if let Some(v) = line.strip_prefix("software ") {
                saved.software = on(v);
                continue;
            }
            if let Some(v) = line.strip_prefix("bitrate ") {
                saved.bitrate = v.trim().parse().ok();
                continue;
            }
            if let Some(v) = line.strip_prefix("mic ") {
                saved.mic = v.trim() != "off";
                continue;
            }
            if let Some(v) = line.strip_prefix("audio ") {
                saved.audio = v.trim() != "off";
                continue;
            }
            if let Some(v) = line.strip_prefix("mirror ") {
                saved.mirror = v.trim() != "off";
                continue;
            }
            let (line, star) = match line.strip_suffix('*') {
                Some(s) => (s.trim(), true),
                None => (line, false),
            };
            // `2560x1440 at -1300 200`: where the headset showed it (files from before
            // 0.1.1 have no middle).
            let (spec, seen) = match line.split_once(" at ") {
                Some((spec, at)) => {
                    // `next`: no centre, right after the previous monitor.
                    let mut v = at.split_whitespace().map(|v| v.parse().ok());
                    (spec.trim(), (v.next().flatten(), v.next().flatten()))
                }
                None => (line, (None, None)),
            };
            if let Ok(spec) = parse(spec) {
                if star {
                    saved.primary = Some(saved.specs.len());
                }
                if let Some(r) = saved.rows.last_mut() {
                    r.push(seen);
                }
                saved.specs.push(spec);
            }
        }
        saved
    }

    fn to_text(&self) -> String {
        let on = |v: bool| if v { "on" } else { "off" };
        let mut text = String::new();
        let mut specs = self.specs.iter().enumerate();
        // Monitors not in a row: one more row of them.
        let rest = self.specs.len()
            - self
                .rows
                .iter()
                .map(Vec::len)
                .sum::<usize>()
                .min(self.specs.len());
        let rows = self
            .rows
            .iter()
            .cloned()
            .chain((rest > 0).then(|| vec![(None, None); rest]));
        for row in rows {
            text.push_str("row\n");
            for ((center, middle), (i, spec)) in row.into_iter().zip(specs.by_ref()) {
                text.push_str(&spec.to_string());
                if center.is_some() || middle.is_some() {
                    let center = center.map_or("next".to_owned(), |c| c.to_string());
                    text.push_str(&format!(" at {center}"));
                    if let Some(middle) = middle {
                        text.push_str(&format!(" {middle}"));
                    }
                }
                if self.primary == Some(i) {
                    text.push_str(" *");
                }
                text.push('\n');
            }
        }
        text.push_str(&format!(
            "mirror {}\naudio {}\nmic {}\n",
            on(self.mirror),
            on(self.audio),
            on(self.mic)
        ));
        if let Some(mbps) = self.bitrate {
            text.push_str(&format!("bitrate {mbps}\n"));
        }
        text.push_str(&format!("sharp {}\n", on(self.sharp)));
        if let Some(codec) = self.codec {
            let name = match codec {
                Codec::H264 => "h264",
                Codec::H265 => "h265",
            };
            text.push_str(&format!("codec {name}\n"));
        }
        text.push_str(&format!("software {}\n", on(self.software)));
        text
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| prostir_proto::secure::write_atomic(&path, self.to_text().as_bytes()));
        if let Err(e) = result {
            warn!("cannot save {}: {e}", path.display());
        }
    }
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
    /// Left-to-right order of the monitors, in the headset and on the desktop.
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
            router: Arc::new(Router::new()),
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
        for seen in &saved.rows {
            let (row, tail) = rest.split_at(seen.len().min(rest.len()));
            rest = tail;
            if !row.is_empty() {
                displays.rows.push(LayoutRow {
                    entries: row
                        .iter()
                        .zip(seen)
                        .map(|(&id, &(center, middle))| LayoutEntry { id, center, middle })
                        .collect(),
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
        let encoder = self.start_stream(id, &mut output)?;
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

    /// Starts stream `id`: its slot in the router and an encoder fed by `output`. When
    /// that fails, nothing of the stream stays registered.
    fn start_stream(&self, id: u8, output: &mut Output) -> Result<Encoder> {
        self.start_encoder(id, output, &self.cfg)
            .inspect_err(|_| self.router.remove_stream(id))
    }

    /// A new encoder with `cfg` for stream `id`, fed by `output`. The stream's slot
    /// starts over with it, and stays when this fails.
    fn start_encoder(&self, id: u8, output: &mut Output, cfg: &EncoderConfig) -> Result<Encoder> {
        self.router.add_stream(id, cfg.codec);
        let router = self.router.clone();
        let encoder = Encoder::start(&output.input(), cfg, move |ev| router.on_event(id, ev))?;
        output.attach(&encoder, cfg.backend.takes_dmabuf())?;
        Ok(encoder)
    }

    /// Housekeeping, about twice a second: logs what the streams sent now and then,
    /// and keeps the mirror in step with the physical display (it disappears when the
    /// laptop lid closes and comes back when it opens).
    pub fn tick(&mut self) {
        self.router.report();
        self.router.set_pace(self.pace());
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
                let encoder = self.start_stream(MIRROR_ID, &mut output)?;
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
        let switched = if wanted.codec != current.codec || wanted.software != current.software {
            crate::encoder::pick(wanted.codec, wanted.software).and_then(|(backend, codec)| {
                info!("encoding with {backend:?} {codec:?} (from the headset menu)");
                self.switch_encoding(backend, codec)
            })
        } else {
            Ok(())
        };
        // What is in use now, the codec as before if it could not change.
        self.save();
        self.announce_pc_settings();
        switched
    }

    /// Encodes with `backend` and `codec` from now on: every monitor's stream starts
    /// over with them. When one cannot, all go back to how they encoded before.
    fn switch_encoding(&mut self, backend: crate::encoder::Backend, codec: Codec) -> Result<()> {
        let before = self.cfg.clone();
        self.cfg.backend = backend;
        self.cfg.codec = codec;
        let ids: Vec<u8> = self.monitors.keys().copied().collect();
        for (i, &id) in ids.iter().enumerate() {
            let Err(e) = self.restart_monitor(id, Some(&before)) else {
                continue;
            };
            warn!(
                "cannot encode with {backend:?} {codec:?}: {e:#}; back to {:?} {:?}",
                before.backend, before.codec
            );
            self.cfg = before;
            for &id in &ids[..i] {
                if let Err(e) = self.restart_monitor(id, None) {
                    error!("{}: {e:#}", stream_name(id));
                }
            }
            return Err(e);
        }
        // Window panels are made again when wanted; the mirror restarts by itself.
        self.close_window_panels();
        self.stop_mirror();
        self.mirror_retry = Instant::now();
        Ok(())
    }

    /// A new encoder for monitor `id` with the current settings, or else with
    /// `fallback`; the old one stops before, so the headset never gets frames of two
    /// codecs for one stream. Returns why the current settings did not work. A monitor
    /// whose stream cannot start at all goes away, as if removed.
    fn restart_monitor(&mut self, id: u8, fallback: Option<&EncoderConfig>) -> Result<()> {
        let Some(Monitor {
            encoder,
            mut output,
            spec,
        }) = self.monitors.remove(&id)
        else {
            return Ok(());
        };
        output.detach();
        drop(encoder);
        let (encoder, result) = match self.start_encoder(id, &mut output, &self.cfg) {
            Ok(encoder) => (Some(encoder), Ok(())),
            Err(e) => {
                let again = fallback.and_then(|cfg| {
                    self.start_encoder(id, &mut output, cfg)
                        .inspect_err(|e| error!("{}: {e:#}", stream_name(id)))
                        .ok()
                });
                (again, Err(e))
            }
        };
        match encoder {
            Some(encoder) => {
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
            None => {
                error!("{}: no stream, removing the monitor", stream_name(id));
                drop(output);
                self.forget(id);
            }
        }
        result
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

    /// A headset just connected (the router is attached): it counts as worn until it
    /// says otherwise, and every stream starts over once, so each monitor sends its
    /// picture now, monitors kept from the last session too.
    pub fn headset_connected(&mut self) {
        self.router.set_pace(self.pace());
        self.taken_off_at = None;
        // Paused ones (kept from before) start over as `set_streaming` resumes them;
        // running ones (just made) are resumed here, which starts them over too.
        if self.streaming {
            for (_, e) in self.encoders() {
                e.set_paused(false);
            }
        }
        self.set_streaming(true);
    }

    /// How fast the connection may send, from what the encoders make now.
    fn pace(&self) -> u64 {
        pace_for(self.encoders().count(), self.cfg.bitrate_kbps)
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
        self.monitors.remove(&id);
        self.forget(id);
        Ok(())
    }

    /// Monitor `id`, no longer in `monitors`, leaves the router, the order and the
    /// desktop's arrangement.
    fn forget(&mut self, id: u8) {
        self.router.remove_stream(id);
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
                r.entries
                    .iter()
                    .filter_map(|e| {
                        let spec = self.monitors.get(&e.id).map(|m| &m.spec);
                        let (w, h) = size(names.get(&e.id)?, spec)?;
                        Some(Seat {
                            id: e.id,
                            center: e.center,
                            middle: e.middle,
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
        let arranged = arrange_free(&seats, main).unwrap_or_else(|| arrange(&rows, main));
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
                    self.monitors
                        .get(id)
                        .is_some_and(|m| m.spec.width == s.width && m.spec.height == s.height)
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

    /// The headset's rows, kept in step with the monitors (see [`rows_in_step`]).
    fn effective_rows(&self) -> Vec<LayoutRow> {
        rows_in_step(
            &self.rows,
            &self.order,
            |id| self.monitors.contains_key(&id),
            // The laptop screen too, where the headset put it (while it shows).
            |id| id == MIRROR_ID && self.mirror.is_some(),
        )
    }

    /// Arranges the desktop's outputs in the rows the headset shows them in.
    pub fn set_layout(&mut self, mut rows: Vec<LayoutRow>) {
        // Offsets are desktop pixels; anything this far out is nonsense (and would
        // overflow the arithmetic below).
        for e in rows.iter_mut().flat_map(|r| &mut r.entries) {
            for c in [&mut e.center, &mut e.middle].into_iter().flatten() {
                *c = (*c).clamp(-100_000, 100_000);
            }
        }
        // The laptop screen's stream is cut for a moment whenever KDE rearranges its
        // outputs, and a headset that sends a layout meanwhile leaves it out. Its spot
        // is kept, or it would come back elsewhere, the headset would move it back, and
        // that would cut it again, round and round.
        let has_mirror = |rows: &[LayoutRow]| {
            rows.iter()
                .flat_map(LayoutRow::ids)
                .any(|id| id == MIRROR_ID)
        };
        if self.mirror_wanted && self.mirror.is_none() && !has_mirror(&rows) {
            for (i, old) in self.rows.iter().enumerate() {
                let Some(spot) = old.entries.iter().find(|e| e.id == MIRROR_ID) else {
                    continue;
                };
                let at = if i < rows.len() {
                    i
                } else {
                    rows.push(LayoutRow::default());
                    rows.len() - 1
                };
                rows[at].entries.push(*spot);
                break;
            }
        }
        self.rows = rows;
        let rows = self.effective_rows();
        self.order = rows
            .iter()
            .flat_map(LayoutRow::ids)
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
            entries: ids
                .iter()
                .map(|&id| LayoutEntry {
                    id,
                    center: None,
                    middle: None,
                })
                .collect(),
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
        let encoder = self.start_stream(id, &mut output)?;
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
            match frame
                .map_err(anyhow::Error::from)
                .and_then(|data| crate::encoder::keyframe_to_png(&data, self.cfg.codec))
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

    /// Every monitor streams. Without any left (none could start again), the set is
    /// made anew for the next session.
    pub fn healthy(&self) -> bool {
        !self.monitors.is_empty()
            && !self.router.failed.load(Ordering::Relaxed)
            && !self
                .monitors
                .values()
                .chain(self.windows.values())
                .any(|m| m.output.closed())
    }

    /// Remembers the set for the next session.
    pub fn save(&self) {
        // Left without monitors (none could stream again), the set saved before stays
        // for the next start.
        if self.monitors.is_empty() {
            return;
        }
        self.saved().save();
    }

    /// The set and the choices as they are now, to save.
    fn saved(&self) -> SavedMonitors {
        let mut saved = SavedMonitors {
            mirror: self.mirror_wanted,
            audio: self.audio_wanted,
            mic: self.mic_wanted,
            bitrate: Some(self.cfg.bitrate_kbps / 1000),
            sharp: self.cfg.sharp,
            codec: Some(self.cfg.codec),
            software: self.cfg.backend == crate::encoder::Backend::X264,
            ..SavedMonitors::default()
        };
        for row in self.effective_rows() {
            let mut seen = Vec::new();
            for e in &row.entries {
                // The laptop screen is not part of the set.
                let Some(m) = self.monitors.get(&e.id) else {
                    continue;
                };
                if self.primary == Some(e.id) {
                    saved.primary = Some(saved.specs.len());
                }
                saved.specs.push(m.spec.clone());
                seen.push((e.center, e.middle));
            }
            if !seen.is_empty() {
                saved.rows.push(seen);
            }
        }
        saved
    }
}

/// `rows` kept in step with the outputs: only monitors (`monitor`) and other outputs
/// that show (`shown`) stay, each in the first place it is listed in, and monitors of
/// `order` not placed yet go to the end of the bottom row. So every monitor is listed
/// exactly once.
fn rows_in_step(
    rows: &[LayoutRow],
    order: &[u8],
    monitor: impl Fn(u8) -> bool,
    shown: impl Fn(u8) -> bool,
) -> Vec<LayoutRow> {
    let mut placed: Vec<u8> = Vec::new();
    let mut kept: Vec<LayoutRow> = rows
        .iter()
        .map(|r| LayoutRow {
            entries: r
                .entries
                .iter()
                .filter(|e| {
                    let new = (monitor(e.id) || shown(e.id)) && !placed.contains(&e.id);
                    if new {
                        placed.push(e.id);
                    }
                    new
                })
                .copied()
                .collect(),
        })
        .filter(|r| !r.entries.is_empty())
        .collect();
    let mut missing = Vec::new();
    for &id in order {
        if monitor(id) && !placed.contains(&id) {
            placed.push(id);
            missing.push(LayoutEntry {
                id,
                center: None,
                middle: None,
            });
        }
    }
    if !missing.is_empty() {
        match kept.last_mut() {
            Some(last) => last.entries.extend(missing),
            None => kept.push(LayoutRow { entries: missing }),
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(outbox: &Outbox) -> Vec<HostMsg> {
        outbox.sent()
    }

    fn keyframe(router: &Router, id: u8) {
        router.on_event(
            id,
            EncoderEvent::Size {
                width: 64,
                height: 32,
            },
        );
        router.on_event(
            id,
            EncoderEvent::Frame {
                pts_us: 0,
                keyframe: true,
                data: vec![0],
            },
        );
    }

    /// A stream that starts over with another encoder is announced with its codec, and
    /// the headset hears when it ends, even before the new encoder sent anything.
    #[test]
    fn a_restarted_stream_is_announced_and_ended() {
        let router = Router::new();
        let outbox = Arc::new(Outbox::default());
        router.attach(outbox.clone());
        router.add_stream(0, Codec::H265);
        keyframe(&router, 0);
        assert!(matches!(
            sent(&outbox)[..],
            [
                HostMsg::StreamStart {
                    id: 0,
                    codec: Codec::H265,
                    ..
                },
                HostMsg::Frame { .. }
            ]
        ));
        router.add_stream(0, Codec::H264);
        keyframe(&router, 0);
        assert!(matches!(
            sent(&outbox)[..],
            [
                HostMsg::StreamStart {
                    id: 0,
                    codec: Codec::H264,
                    ..
                },
                HostMsg::Frame { .. }
            ]
        ));
        router.add_stream(0, Codec::H265);
        router.remove_stream(0);
        assert!(matches!(sent(&outbox)[..], [HostMsg::StreamEnd { id: 0 }]));
        // Never shown: nothing to end.
        router.add_stream(1, Codec::H265);
        router.remove_stream(1);
        assert!(sent(&outbox).is_empty());
    }

    /// One and a half times what the encoders make plus room for sound, never under
    /// 20 Mbit/s.
    #[test]
    fn the_pace_follows_the_bitrate() {
        let mbit = |bits: u64| bits * 1_000_000 / 8;
        assert_eq!(pace_for(3, 30_000), mbit(139));
        assert_eq!(pace_for(1, 8_000), mbit(20));
        assert_eq!(pace_for(0, 30_000), mbit(20));
        assert_eq!(pace_for(6, 50_000), mbit(454));
    }

    fn spec(s: &str) -> Result<MonitorSpec, String> {
        let (w, h) = s.split_once('x').ok_or("no x")?;
        Ok(MonitorSpec {
            width: w.parse().map_err(|_| "width")?,
            height: h.parse().map_err(|_| "height")?,
            scale: 1.0,
        })
    }

    /// Every monitor is in the rows once, in the first place listed: an order made
    /// from them never repeats one (or grows past what a message holds).
    #[test]
    fn rows_list_each_monitor_once() {
        let at = |id, center| LayoutEntry {
            id,
            center: Some(center),
            middle: None,
        };
        let rows = vec![
            LayoutRow {
                entries: vec![at(0, -100), at(0, 0), at(MIRROR_ID, 50)],
            },
            LayoutRow {
                entries: vec![at(1, 10), at(0, 20), at(9, 30)],
            },
        ];
        let monitor = |id| id < 3;
        let kept = rows_in_step(&rows, &[2, 0, 1, 2], monitor, |id| id == MIRROR_ID);
        let ids: Vec<Vec<u8>> = kept.iter().map(|r| r.ids().collect()).collect();
        assert_eq!(ids, vec![vec![0, MIRROR_ID], vec![1, 2]]);
        assert_eq!(kept[0].entries[0], at(0, -100), "the first place counts");
        let many = vec![LayoutRow {
            entries: vec![at(0, 0); 16],
        }];
        let kept = rows_in_step(&many, &[0, 1], monitor, |_| false);
        assert_eq!(kept[0].ids().collect::<Vec<_>>(), vec![0, 1]);
    }

    /// Where the headset showed each monitor, across and up and down, survives a
    /// restart of the PC app: the desktop is then laid out as before at once.
    #[test]
    fn saved_set_keeps_where_monitors_were_seen() {
        let saved = SavedMonitors {
            specs: vec![spec("2560x1440").unwrap(), spec("1440x2560").unwrap()],
            primary: Some(1),
            rows: vec![vec![(Some(-1300), Some(200))], vec![(None, Some(-40))]],
            mic: false,
            bitrate: Some(40),
            codec: Some(Codec::H264),
            ..SavedMonitors::default()
        };
        let text = saved.to_text();
        let back = SavedMonitors::parse(&text, spec);
        assert_eq!(back.specs.len(), 2);
        assert_eq!(back.primary, Some(1));
        assert_eq!(back.rows, saved.rows);
        assert!(!back.mic && back.audio && back.mirror);
        assert_eq!(back.bitrate, Some(40));
        assert_eq!(back.codec, Some(Codec::H264));
        assert_eq!(back.to_text(), text);
    }

    /// Files from before vertical positions were kept still load.
    #[test]
    fn old_saved_sets_load() {
        let old = "row\n2560x1440 at -1300 *\n1920x1080\nmirror off\nbitrate 30\n";
        let saved = SavedMonitors::parse(old, spec);
        assert_eq!(saved.specs.len(), 2);
        assert_eq!(saved.primary, Some(0));
        assert_eq!(saved.rows, vec![vec![(Some(-1300), None), (None, None)]]);
        assert!(!saved.mirror);
        // Before rows were saved at all: one row.
        let older = SavedMonitors::parse("2560x1440\n1920x1080\n", spec);
        assert!(older.rows.is_empty());
        assert_eq!(SavedMonitors::parse(&older.to_text(), spec).specs.len(), 2);
    }
}
