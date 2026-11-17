//! The set of virtual monitors, their encoders, and delivery to the headset.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use log::{debug, error, info, warn};
use prostir_proto::{Codec, HostMsg, MIRROR_ID};

use crate::audio::AudioOut;
use crate::encoder::{Encoder, EncoderConfig, EncoderEvent};
use crate::virtual_output::{self, MonitorSpec, Screencast, VirtualOutput};

/// More streams than this overwhelm the headset's hardware decoders.
pub const MAX_MONITORS: usize = 6;

/// Per-stream delivery state for the currently connected client.
struct StreamSlot {
    size: Option<(u32, u32)>,
    announced: bool,
    need_keyframe: bool,
    /// pts → when the encoded frame was queued for sending.
    sent: HashMap<u64, Instant>,
    latency: LatencyStats,
}

impl StreamSlot {
    fn new() -> Self {
        Self {
            size: None,
            announced: false,
            need_keyframe: true,
            sent: HashMap::new(),
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
        format!("Prostir-{}", id + 1)
    }
}

/// Delivers encoder output to whichever client is connected.
pub struct Router {
    codec: Codec,
    inner: Mutex<RouterInner>,
    failed: AtomicBool,
    mirror_failed: AtomicBool,
}

struct RouterInner {
    tx: Option<SyncSender<HostMsg>>,
    streams: BTreeMap<u8, StreamSlot>,
}

impl Router {
    fn new(codec: Codec) -> Self {
        Self {
            codec,
            inner: Mutex::new(RouterInner {
                tx: None,
                streams: BTreeMap::new(),
            }),
            failed: AtomicBool::new(false),
            mirror_failed: AtomicBool::new(false),
        }
    }

    pub fn attach(&self, tx: SyncSender<HostMsg>) {
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

    /// Sends a control message to the connected client, if any.
    pub fn send(&self, msg: HostMsg) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(tx) = &inner.tx
            && tx.send(msg).is_err()
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
            && tx.send(HostMsg::StreamEnd { id }).is_err()
        {
            inner.tx = None;
        }
    }

    /// Forwards a slice of PC sound; dropped if the network is backed up (a tiny gap
    /// in sound beats growing latency).
    pub fn send_audio(&self, pcm: Vec<u8>) {
        if let Some(tx) = &self.inner.lock().unwrap().tx {
            let _ = tx.try_send(HostMsg::Audio { pcm });
        }
    }

    pub fn frame_shown(&self, id: u8, pts_us: u64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(slot) = inner.streams.get_mut(&id) else {
            return;
        };
        match slot.sent.remove(&pts_us) {
            Some(sent) => slot.latency.add(id, sent.elapsed().as_secs_f64() * 1e3),
            None => debug!("shown frame {pts_us} of Prostir-{} is unknown", id + 1),
        }
    }

    /// Returns `true` when the encoder should produce a keyframe.
    fn on_event(&self, id: u8, event: EncoderEvent) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let RouterInner { tx, streams } = &mut *inner;
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
                let Some(sender) = tx else { return false };
                if !slot.announced {
                    let Some((width, height)) = slot.size else {
                        return true;
                    };
                    let start = HostMsg::StreamStart {
                        id,
                        width,
                        height,
                        codec: self.codec,
                        name: stream_name(id),
                    };
                    if sender.send(start).is_err() {
                        *tx = None;
                        return false;
                    }
                    slot.announced = true;
                }
                if slot.need_keyframe && !keyframe {
                    return true;
                }
                match sender.try_send(HostMsg::Frame {
                    id,
                    pts_us,
                    keyframe,
                    data,
                }) {
                    Ok(()) => {
                        slot.need_keyframe = false;
                        if slot.sent.len() > 256 {
                            slot.sent.clear();
                        }
                        slot.sent.insert(pts_us, Instant::now());
                        false
                    }
                    Err(TrySendError::Full(_)) => {
                        // The network can't keep up: drop until the next keyframe so the
                        // decoder never sees a broken reference chain.
                        slot.need_keyframe = true;
                        true
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        *tx = None;
                        false
                    }
                }
            }
        }
    }
}

struct Monitor {
    // Field order matters: the encoder must stop before the output goes away.
    encoder: Encoder,
    output: VirtualOutput,
    spec: MonitorSpec,
}

/// The physical display (laptop panel) streamed as-is.
struct Mirror {
    // Field order matters: encoder, then stream, then its own KWin connection.
    encoder: Encoder,
    output: VirtualOutput,
    _connection: Screencast,
}

/// Saved monitor set: one `WIDTHxHEIGHT[@scale]` per line, `*` marks the primary one;
/// a `mirror on|off` line remembers the physical display mirror.
pub struct SavedMonitors {
    pub specs: Vec<MonitorSpec>,
    pub primary: Option<usize>,
    pub mirror: bool,
    /// PC sound goes to the headset while it is connected.
    pub audio: bool,
    /// Mbit/s per monitor, if chosen in the tray.
    pub bitrate: Option<u32>,
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
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
    let mut bitrate = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(v) = line.strip_prefix("bitrate ") {
            bitrate = v.trim().parse().ok();
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
        let (spec, star) = match line.strip_suffix('*') {
            Some(s) => (s.trim(), true),
            None => (line, false),
        };
        if let Ok(spec) = parse(spec) {
            if star {
                primary = Some(specs.len());
            }
            specs.push(spec);
        }
    }
    (!specs.is_empty()).then_some(SavedMonitors {
        specs,
        primary,
        mirror,
        audio,
        bitrate,
    })
}

pub struct Displays {
    // Field order matters: streams close before the KWin connection.
    monitors: BTreeMap<u8, Monitor>,
    /// Left-to-right order of the monitors, in the headset and in KDE.
    order: Vec<u8>,
    mirror: Option<Mirror>,
    mirror_wanted: bool,
    mirror_retry: Instant,
    /// PC sound to the headset, while one is connected.
    audio: Option<AudioOut>,
    audio_wanted: bool,
    /// Last time the laptop panel was switched back on (to avoid fighting the user).
    woke_display_at: Option<Instant>,
    screencast: Screencast,
    pub router: Arc<Router>,
    cfg: EncoderConfig,
    primary: Option<u8>,
    /// Whether to pick KDE's primary monitor ourselves.
    manage_primary: bool,
}

impl Displays {
    pub fn create(saved: &SavedMonitors, cfg: EncoderConfig, manage_primary: bool) -> Result<Self> {
        let mut displays = Self {
            monitors: BTreeMap::new(),
            order: Vec::new(),
            mirror: None,
            mirror_wanted: saved.mirror,
            audio: None,
            audio_wanted: saved.audio,
            mirror_retry: Instant::now(),
            woke_display_at: None,
            screencast: Screencast::connect()?,
            router: Arc::new(Router::new(cfg.codec)),
            cfg,
            primary: None,
            manage_primary,
        };
        for spec in saved.specs.iter().take(MAX_MONITORS) {
            displays.add(spec.clone())?;
        }
        let ids = displays.order.clone();
        if let Some(&id) = saved.primary.and_then(|i| ids.get(i)) {
            displays.set_primary(id);
        } else {
            displays.ensure_primary();
        }
        displays.tick();
        sync_wallpapers();
        Ok(displays)
    }

    /// Adds a monitor and returns its id; ids are reused so `Prostir-N` keeps the
    /// position and scale KScreen remembers for that name.
    pub fn add(&mut self, spec: MonitorSpec) -> Result<u8> {
        if self.monitors.len() >= MAX_MONITORS {
            bail!("Максимум {MAX_MONITORS} моніторів");
        }
        let id = (0..=u8::MAX)
            .find(|id| !self.monitors.contains_key(id))
            .unwrap();
        let output = self.screencast.create(u32::from(id) + 1, &spec)?;
        let encoder = self.start_encoder(id, output.node)?;
        self.monitors.insert(
            id,
            Monitor {
                encoder,
                output,
                spec,
            },
        );
        self.order.push(id);
        self.apply_kde_layout();
        self.router.send(HostMsg::Order {
            ids: self.order.clone(),
        });
        Ok(id)
    }

    fn start_encoder(&self, id: u8, node: u32) -> Result<Encoder> {
        self.router.add_stream(id);
        let router = self.router.clone();
        Encoder::start(node, &self.cfg, move |ev| router.on_event(id, ev))
            .inspect_err(|_| self.router.remove_stream(id))
    }

    /// Keeps the mirror in step with the physical display: it disappears when the
    /// laptop lid closes and comes back when it opens.
    pub fn tick(&mut self) {
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
        let Some(name) = self.screencast.physical_output() else {
            self.wake_physical_display();
            return;
        };
        let started = Screencast::connect().and_then(|connection| {
            let output = connection.mirror(u32::from(MIRROR_ID) + 1, &name)?;
            let encoder = self.start_encoder(MIRROR_ID, output.node)?;
            Ok(Mirror {
                encoder,
                output,
                _connection: connection,
            })
        });
        match started {
            Ok(m) => {
                m.encoder.force_keyframe();
                self.mirror = Some(m);
                self.apply_kde_layout();
            }
            Err(e) => warn!("cannot mirror {name}: {e:#}"),
        }
    }

    /// KDE sometimes leaves the laptop panel off after the lid is opened again while
    /// virtual monitors exist. With the mirror wanted and the lid open, switch it on.
    fn wake_physical_display(&mut self) {
        if self
            .woke_display_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(30))
            || !virtual_output::lid_open()
        {
            return;
        }
        let off = virtual_output::disabled_physical_outputs();
        if off.is_empty() {
            return;
        }
        self.woke_display_at = Some(Instant::now());
        info!("lid is open but {off:?} is off: switching it on for the mirror");
        let settings: Vec<String> = off.iter().map(|n| format!("output.{n}.enable")).collect();
        virtual_output::kscreen_apply(&settings);
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

    /// Starts or stops sending PC sound (only while a headset is connected).
    pub fn set_audio_active(&mut self, active: bool) {
        let active = active && self.audio_wanted;
        if active && self.audio.is_none() {
            match AudioOut::start(self.router.clone()) {
                Ok(a) => self.audio = Some(a),
                Err(e) => warn!("no sound in the headset: {e:#}"),
            }
        } else if !active {
            self.audio = None;
        }
    }

    pub fn set_audio_wanted(&mut self, enabled: bool) {
        self.audio_wanted = enabled;
        self.set_audio_active(enabled);
        self.router.send(HostMsg::AudioWanted { enabled });
    }

    pub fn audio_wanted(&self) -> bool {
        self.audio_wanted
    }

    pub fn mirror_wanted(&self) -> bool {
        self.mirror_wanted
    }

    pub fn remove(&mut self, id: u8) -> Result<()> {
        if !self.monitors.contains_key(&id) {
            bail!("Монітора {} немає", id + 1);
        }
        if self.monitors.len() == 1 {
            bail!("Останній монітор прибрати не можна");
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
        self.apply_kde_layout();
    }

    /// Lays the outputs out in KDE like they stand in the headset: Quest monitors side
    /// by side in `order`, vertically centred, and the mirrored display under the
    /// primary monitor. The mouse then crosses screens where the eyes expect it to.
    fn apply_kde_layout(&self) {
        let geometries = virtual_output::output_geometries();
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
        let row: Vec<(u8, String, (i32, i32))> = self
            .order
            .iter()
            .filter_map(|id| {
                let m = self.monitors.get(id)?;
                let name = m.output.output_name().to_owned();
                let wh = size(&name, Some(&m.spec))?;
                Some((*id, name, wh))
            })
            .collect();
        let tallest = row.iter().map(|r| r.2.1).max().unwrap_or(0);
        let mut settings = Vec::new();
        // Every applied change makes KWin reconfigure its outputs, which also cuts the
        // laptop mirror's stream: only send what actually differs.
        let mut position = |name: &str, x: i32, y: i32| {
            if geometries.get(name).is_none_or(|g| (g.x, g.y) != (x, y)) {
                settings.push(format!("output.{name}.position.{x},{y}"));
            }
        };
        let mut placed = HashMap::new();
        let mut x = 0;
        for (id, name, (w, h)) in &row {
            let y = (tallest - h) / 2;
            position(name, x, y);
            placed.insert(*id, (x, y, *w, *h));
            x += w;
        }
        if let (Some(mirror), Some(&(px, py, pw, ph))) =
            (&self.mirror, self.primary.and_then(|p| placed.get(&p)))
        {
            let name = mirror.output.output_name();
            if let Some((w, _)) = size(name, None) {
                position(name, px + (pw - w) / 2, py + ph);
            }
        }
        if self.manage_primary
            && let Some(m) = self.primary.and_then(|p| self.monitors.get(&p))
            && geometries
                .get(m.output.output_name())
                .is_none_or(|g| g.priority != Some(1))
        {
            settings.push(format!("output.{}.priority.1", m.output.output_name()));
        }
        virtual_output::kscreen_apply(&settings);
    }

    /// Changes the left-to-right order (ids of all current monitors).
    pub fn set_order(&mut self, ids: &[u8]) -> Result<()> {
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        let mut current: Vec<u8> = self.monitors.keys().copied().collect();
        current.sort_unstable();
        if sorted != current {
            bail!("Порядок не збігається з моніторами");
        }
        self.order = ids.to_vec();
        self.apply_kde_layout();
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
            self.apply_kde_layout();
        }
    }

    pub fn primary(&self) -> Option<u8> {
        self.primary
    }

    fn encoders(&self) -> impl Iterator<Item = (u8, &Encoder)> {
        let mirror = self.mirror.iter().map(|m| (MIRROR_ID, &m.encoder));
        self.monitors
            .iter()
            .map(|(id, m)| (*id, &m.encoder))
            .chain(mirror)
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
            && !self.monitors.values().any(|m| m.output.closed())
    }

    /// Remembers the set for the next session.
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        let text: String = self
            .order
            .iter()
            .filter_map(|id| Some((id, self.monitors.get(id)?)))
            .map(|(id, m)| {
                let star = if self.primary == Some(*id) { " *" } else { "" };
                format!("{}{star}\n", m.spec)
            })
            .chain(std::iter::once(format!(
                "mirror {}\naudio {}\nbitrate {}\n",
                if self.mirror_wanted { "on" } else { "off" },
                if self.audio_wanted { "on" } else { "off" },
                self.cfg.bitrate_kbps / 1000
            )))
            .collect();
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, text));
        if let Err(e) = result {
            warn!("cannot save {}: {e}", path.display());
        }
    }
}

/// Gives every screen the wallpaper of the primary desktop. Plasma configures
/// wallpapers per screen, so new monitors would otherwise show the default one.
/// Runs a little later because Plasma sets up desktops for new screens asynchronously.
pub fn sync_wallpapers() {
    const SCRIPT: &str = r#"
        var src = desktopForScreen(0);
        if (src) {
            src.currentConfigGroup = ["Wallpaper", src.wallpaperPlugin, "General"];
            var image = src.readConfig("Image");
            desktops().forEach(function (d) {
                if (d.screen < 0 || d.screen == 0) return;
                d.wallpaperPlugin = src.wallpaperPlugin;
                d.currentConfigGroup = ["Wallpaper", src.wallpaperPlugin, "General"];
                if (d.readConfig("Image") != image) d.writeConfig("Image", image);
            });
        }"#;
    let _ = std::thread::Builder::new()
        .name("wallpapers".into())
        .spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            for qdbus in ["qdbus6", "qdbus"] {
                let ran = std::process::Command::new(qdbus)
                    .args([
                        "org.kde.plasmashell",
                        "/PlasmaShell",
                        "org.kde.PlasmaShell.evaluateScript",
                        SCRIPT,
                    ])
                    .output();
                match ran {
                    Ok(out) if out.status.success() => return,
                    Ok(out) => {
                        warn!(
                            "wallpaper sync failed: {}",
                            String::from_utf8_lossy(&out.stderr).trim()
                        );
                        return;
                    }
                    Err(_) => continue, // try the next qdbus name
                }
            }
            warn!("qdbus not found, wallpapers stay per screen");
        });
}
