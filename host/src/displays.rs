//! The set of virtual monitors, their encoders, and delivery to the headset.

use crate::i18n::t;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{debug, error, info, warn};
use prostir_layout::desk;
use prostir_proto::stats::{Histogram, Period};
use prostir_proto::{Codec, HostMsg, LayoutRow, MAX_MONITORS, PcSettings};

use crate::encoder::{Encoder, EncoderConfig, EncoderEvent};
use crate::layout::panel_size;
use crate::outbox::Outbox;
use crate::platform::audio::{AudioOut, MicIn};
use crate::platform::windows;
use crate::platform::{Desktop, DesktopBackend, Geometry, MonitorSpec, Output, OutputBackend};
use crate::screen::{Screen, ScreenId, ScreenKind, wire_ids};
use crate::store::{SavedScreens, Settings};

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

/// Delivers encoder output to whichever client is connected.
pub struct Router {
    /// A screenshot waiting for the next keyframe of stream `id` (see `screenshot`).
    snapshot: Mutex<Option<(ScreenId, std::sync::mpsc::SyncSender<Vec<u8>>)>>,
    inner: Mutex<RouterInner>,
    failed: AtomicBool,
    mirror_failed: AtomicBool,
}

struct RouterInner {
    tx: Option<Arc<Outbox>>,
    streams: BTreeMap<ScreenId, StreamSlot>,
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
            let streams: Vec<(ScreenId, Histogram, u32)> = streams
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
            let video = stats.video.remove(&id.wire()).unwrap_or_default();
            if video.frames == 0 && video.dropped.total() == 0 && skipped == 0 {
                continue;
            }
            let shown = match shown.count() {
                0 => "no reports".to_owned(),
                n => format!("{} ({n} reports)", shown.summary_ms()),
            };
            info!(
                "{id}: {}; {skipped} skipped waiting for a keyframe; encoded → shown in headset {shown}",
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
    fn add_stream(&self, id: ScreenId, codec: Codec) {
        let mut inner = self.inner.lock().unwrap();
        let known = inner.streams.get(&id).is_some_and(|s| s.known);
        inner.streams.insert(id, StreamSlot::new(codec, known));
    }

    fn remove_stream(&self, id: ScreenId) {
        let mut inner = self.inner.lock().unwrap();
        let known = inner.streams.remove(&id).is_some_and(|s| s.known);
        if known
            && let Some(tx) = &inner.tx
            && !tx.control(HostMsg::StreamEnd { id: id.wire() })
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

    pub fn frame_shown(&self, id: ScreenId, pts_us: u64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(slot) = inner.streams.get_mut(&id) else {
            return;
        };
        match slot.sent.remove(&pts_us) {
            Some(sent) => slot.shown.add(sent.elapsed()),
            None => debug!("shown frame {pts_us} of {id} is unknown"),
        }
        // The headset reports only some frames: older ones are done too.
        slot.sent = slot.sent.split_off(&pts_us);
    }

    /// Returns `true` when the encoder should produce a keyframe.
    fn on_event(&self, id: ScreenId, event: EncoderEvent) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let RouterInner { tx, streams, .. } = &mut *inner;
        // Streams whose queued frames were too late to send: they need a keyframe.
        if let Some(outbox) = tx {
            for lost in outbox.take_dropped() {
                if let Some(s) = streams.get_mut(&ScreenId::from_wire(lost)) {
                    s.need_keyframe = true;
                }
            }
        }
        let Some(slot) = streams.get_mut(&id) else {
            return false;
        };
        match event {
            EncoderEvent::Size { width, height } => {
                info!("{id}: streaming {width}x{height}");
                slot.size = Some((width, height));
                slot.announced = false;
                slot.need_keyframe = true;
                true
            }
            EncoderEvent::Failed(reason) => {
                error!("{id}: {reason}");
                // A broken mirror is restarted by `Displays::tick`; anything else ends the session.
                if id == ScreenId::BUILTIN {
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
                        id: id.wire(),
                        width,
                        height,
                        codec: slot.codec,
                        name: id.name(),
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
                    id: id.wire(),
                    pts_us,
                    keyframe,
                    data,
                };
                if sender.video(id.wire(), keyframe, msg) {
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

pub struct Displays {
    // Field order matters: streams close before the desktop connection.
    /// Every screen shown in the headset: virtual monitors, window panels, the laptop
    /// screen.
    screens: BTreeMap<ScreenId, Screen>,
    /// When the window panels are next checked for a closed window.
    windows_checked: Instant,
    /// A new panel output to place once the compositor lists it.
    windows_unplaced: bool,
    /// Left-to-right order of the monitors, in the headset and on the desktop.
    order: Vec<ScreenId>,
    /// How the headset arranges the monitors in rows (see `effective_rows`).
    rows: Vec<Vec<desk::Entry<ScreenId>>>,
    mirror_retry: Instant,
    /// The settings as last applied from the store (see [`Displays::apply`]).
    settings: Settings,
    /// PC sound to the headset, while one is connected.
    audio: Option<AudioOut>,
    mic: Option<MicIn>,
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
    primary: Option<ScreenId>,
    /// Whether to pick KDE's primary monitor ourselves.
    manage_primary: bool,
}

impl Displays {
    /// The screens of `set`, each with its own number, streaming with `cfg`; sound,
    /// microphone and the laptop screen as `settings` say.
    pub fn create(
        set: &SavedScreens,
        settings: &Settings,
        cfg: EncoderConfig,
        manage_primary: bool,
    ) -> Result<Self> {
        let desktop = Desktop::connect()?;
        // Only some desktops (KDE) have a primary output.
        let manage_primary = manage_primary && desktop.has_primary();
        let mut displays = Self {
            screens: BTreeMap::new(),
            windows_checked: Instant::now(),
            windows_unplaced: false,
            order: Vec::new(),
            rows: set.rows.clone(),
            settings: settings.clone(),
            audio: None,
            mic: None,
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
        // In the order of the rows, then any the rows leave out.
        let mut ids: Vec<ScreenId> = set
            .rows
            .iter()
            .flatten()
            .map(|e| e.id)
            .filter(|id| set.specs.contains_key(id))
            .collect();
        let rest: Vec<ScreenId> = set
            .specs
            .keys()
            .filter(|id| !ids.contains(id))
            .copied()
            .collect();
        ids.extend(rest);
        for id in ids.into_iter().take(MAX_MONITORS) {
            displays.add_as(id, set.specs[&id].clone())?;
        }
        match set.primary {
            Some(id) if displays.is_monitor(id) => displays.set_primary(id),
            _ => displays.ensure_primary(),
        }
        displays.tick();
        displays.sync_wallpapers();
        Ok(displays)
    }

    /// The virtual monitors.
    fn monitors(&self) -> impl Iterator<Item = &Screen> {
        self.screens.values().filter(|s| s.is_virtual())
    }

    /// The window panels.
    fn windows(&self) -> impl Iterator<Item = &Screen> {
        self.screens.values().filter(|s| s.is_window())
    }

    /// The laptop screen, while it is streamed.
    fn mirror(&self) -> Option<&Screen> {
        self.screens.get(&ScreenId::BUILTIN)
    }

    /// Whether `id` is one of the virtual monitors.
    pub fn is_monitor(&self, id: ScreenId) -> bool {
        self.screens.get(&id).is_some_and(Screen::is_virtual)
    }

    /// Adds a monitor and returns its id; ids are reused so `Prostir-N` keeps the
    /// position and scale KScreen remembers for that name.
    pub fn add(&mut self, spec: MonitorSpec) -> Result<ScreenId> {
        if self.monitor_count() >= MAX_MONITORS {
            bail!(
                "{} {MAX_MONITORS} {}",
                t("Максимум", "At most"),
                t("моніторів", "monitors")
            );
        }
        let id = ScreenId::first_free(|id| self.screens.contains_key(&id));
        self.add_as(id, spec)?;
        Ok(id)
    }

    /// Adds monitor `id`.
    fn add_as(&mut self, id: ScreenId, spec: MonitorSpec) -> Result<()> {
        let mut output = self.desktop.create(id.number() + 1, &spec)?;
        let encoder = self.start_stream(id, &mut output)?;
        self.screens.insert(
            id,
            Screen::running(id, ScreenKind::Virtual { spec }, output, encoder),
        );
        self.order.push(id);
        self.apply_layout();
        self.send_order();
        Ok(())
    }

    /// Starts stream `id`: its slot in the router and an encoder fed by `output`. When
    /// that fails, nothing of the stream stays registered.
    fn start_stream(&self, id: ScreenId, output: &mut Output) -> Result<Encoder> {
        self.start_encoder(id, output, &self.cfg)
            .inspect_err(|_| self.router.remove_stream(id))
    }

    /// A new encoder with `cfg` for stream `id`, fed by `output`. The stream's slot
    /// starts over with it, and stays when this fails.
    fn start_encoder(
        &self,
        id: ScreenId,
        output: &mut Output,
        cfg: &EncoderConfig,
    ) -> Result<Encoder> {
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
        self.sync_audio();
        self.check_primary();
        self.check_window_panels();
        self.sync_mirror();
    }

    /// Streams the laptop screen while it is wanted and there, and starts it again
    /// when it stopped (the laptop lid closed and opened, say).
    fn sync_mirror(&mut self) {
        let broken = self.router.mirror_failed.swap(false, Ordering::Relaxed);
        if let Some(m) = self.mirror()
            && (broken || m.output.as_ref().is_none_or(|o| o.closed()))
        {
            if let ScreenKind::Physical { connector, .. } = &m.kind {
                info!("mirror of {connector} stopped, restarting it shortly");
            }
            self.stop_mirror();
            self.mirror_retry = Instant::now() + Duration::from_secs(2);
        }
        if !self.settings.mirror || self.mirror().is_some() || self.mirror_retry > Instant::now() {
            return;
        }
        self.mirror_retry = Instant::now() + Duration::from_secs(2);
        let Some(name) = self.desktop.physical_output() else {
            self.desktop.wake_physical_display();
            return;
        };
        let id = ScreenId::BUILTIN;
        let started = self
            .desktop
            .mirror(id.number() + 1, &name)
            .and_then(|mut output| {
                let encoder = self.start_stream(id, &mut output)?;
                Ok((output, encoder))
            });
        match started {
            Ok((output, encoder)) => {
                encoder.force_keyframe();
                let kind = ScreenKind::Physical {
                    connector: name,
                    builtin: true,
                };
                self.screens
                    .insert(id, Screen::running(id, kind, output, encoder));
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
        let Some(output) = self
            .primary
            .and_then(|id| self.screens.get(&id))
            .and_then(|s| s.output.as_ref())
        else {
            return;
        };
        self.desktop.keep_primary(output);
    }

    fn stop_mirror(&mut self) {
        if self.screens.remove(&ScreenId::BUILTIN).is_some() {
            self.router.remove_stream(ScreenId::BUILTIN);
        }
    }

    /// Follows the settings `wanted` (as the store has them now): quality and the text
    /// preset on every encoder, sound, microphone and the laptop screen. Another codec
    /// is [`Displays::encode_with`].
    pub fn apply(&mut self, wanted: &Settings) {
        let was = std::mem::replace(&mut self.settings, wanted.clone());
        if wanted.bitrate * 1000 != self.cfg.bitrate_kbps {
            self.cfg.bitrate_kbps = wanted.bitrate * 1000;
            for (_, e) in self.encoders() {
                e.set_bitrate(self.cfg.bitrate_kbps);
            }
        }
        if wanted.sharp != self.cfg.sharp {
            self.cfg.sharp = wanted.sharp;
            let value = self.cfg.target_usage();
            for (_, e) in self.encoders() {
                e.set_target_usage(value);
            }
        }
        // Switched on by hand: tried right away.
        if wanted.audio != was.audio {
            self.audio_retry.succeeded();
            self.router.send(HostMsg::AudioWanted {
                enabled: wanted.audio,
            });
        }
        if wanted.mic != was.mic {
            self.mic_retry.succeeded();
            self.router.send(HostMsg::MicWanted {
                enabled: wanted.mic,
            });
        }
        self.sync_audio();
        if wanted.mirror != was.mirror {
            self.mirror_retry = Instant::now();
            if !wanted.mirror {
                self.stop_mirror();
            }
            self.sync_mirror();
            self.router.send(HostMsg::MirrorWanted {
                enabled: wanted.mirror,
            });
        }
    }

    /// Tells a headset that just connected what the settings are.
    pub fn announce_settings(&self) {
        self.router.send(HostMsg::MirrorWanted {
            enabled: self.settings.mirror,
        });
        self.router.send(HostMsg::AudioWanted {
            enabled: self.settings.audio,
        });
        self.router.send(HostMsg::MicWanted {
            enabled: self.settings.mic,
        });
        self.announce_pc_settings();
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
    fn output_geometries(&self) -> Vec<(ScreenId, Geometry)> {
        let geometries = self.desktop.geometries();
        self.screens
            .values()
            .filter_map(|s| Some((s.id, *geometries.get(s.output_name()?)?)))
            .collect()
    }

    /// Where the mouse is: the screen and the point across its image (0..=65535).
    pub fn find_mouse(&self) -> Option<(ScreenId, u16, u16)> {
        let (x, y) = windows::cursor()?;
        let frac = |v: i32, start: i32, len: i32| {
            ((f64::from(v - start) / f64::from(len.max(1))).clamp(0.0, 1.0) * 65535.0) as u16
        };
        self.output_geometries()
            .into_iter()
            .find(|(_, g)| x >= g.x && x < g.x + g.w && y >= g.y && y < g.y + g.h)
            .map(|(id, g)| (id, frac(x, g.x, g.w), frac(y, g.y, g.h)))
    }

    /// Puts the mouse on screen `id` at `x`/`y` across its image (0..=65535).
    pub fn mouse_to(&self, id: ScreenId, x: u16, y: u16) {
        let Some((_, g)) = self.output_geometries().into_iter().find(|(i, _)| *i == id) else {
            warn!("mouse: no {id} on the desktop");
            return;
        };
        let at =
            |start: i32, len: i32, f: u16| start + (f64::from(len) * f64::from(f) / 65535.0) as i32;
        let (cx, cy) = (at(g.x, g.w - 1, x), at(g.y, g.h - 1, y));
        if windows::move_cursor(cx, cy) {
            info!("mouse moved to {id} at {cx},{cy}");
        } else {
            warn!("could not move the mouse to {cx},{cy}");
        }
    }

    /// Another codec or the CPU encoder, asked for in the headset menu: every stream
    /// starts over with what the PC can make of it (the monitors and their windows
    /// stay). Nothing changes when that is what encodes now.
    pub fn encode_with(&mut self, codec: Codec, software: bool) -> Result<()> {
        let current = self.pc_settings();
        if (codec, software) == (current.codec, current.software) {
            return Ok(());
        }
        let (backend, codec) = crate::encoder::pick(codec, software)?;
        info!("encoding with {backend:?} {codec:?} (from the headset menu)");
        self.switch_encoding(backend, codec)
    }

    /// Encodes with `backend` and `codec` from now on: every monitor's stream starts
    /// over with them. When one cannot, all go back to how they encoded before.
    fn switch_encoding(&mut self, backend: crate::encoder::Backend, codec: Codec) -> Result<()> {
        let before = self.cfg.clone();
        self.cfg.backend = backend;
        self.cfg.codec = codec;
        let ids: Vec<ScreenId> = self.monitors().map(|s| s.id).collect();
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
                    error!("{id}: {e:#}");
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
    fn restart_monitor(&mut self, id: ScreenId, fallback: Option<&EncoderConfig>) -> Result<()> {
        let Some(screen) = self.screens.get_mut(&id) else {
            return Ok(());
        };
        let Some(mut output) = screen.output.take() else {
            return Ok(());
        };
        output.detach();
        drop(screen.stream.take());
        let (encoder, result) = match self.start_encoder(id, &mut output, &self.cfg) {
            Ok(encoder) => (Some(encoder), Ok(())),
            Err(e) => {
                let again = fallback.and_then(|cfg| {
                    self.start_encoder(id, &mut output, cfg)
                        .inspect_err(|e| error!("{id}: {e:#}"))
                        .ok()
                });
                (again, Err(e))
            }
        };
        match encoder {
            Some(encoder) => {
                encoder.force_keyframe();
                if let Some(screen) = self.screens.get_mut(&id) {
                    screen.stream = Some(encoder);
                    screen.output = Some(output);
                }
            }
            None => {
                error!("{id}: no stream, removing the monitor");
                drop(output);
                self.screens.remove(&id);
                self.forget(id);
            }
        }
        result
    }

    pub fn monitor_count(&self) -> usize {
        self.monitors().count()
    }

    pub fn mirror_active(&self) -> bool {
        self.mirror().is_some()
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
        let audio = self.streaming && self.settings.audio;
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
        let mic = self.streaming && self.settings.mic;
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

    /// The screen whose output is `name`.
    pub fn id_of_output(&self, name: &str) -> Option<ScreenId> {
        self.screens
            .values()
            .find(|s| s.output_name() == Some(name))
            .map(|s| s.id)
    }

    /// Removes monitor `id`, or closes window panel `id`.
    pub fn remove(&mut self, id: ScreenId) -> Result<()> {
        if self.screens.get(&id).is_some_and(Screen::is_window) {
            self.close_window_panel(id);
            return Ok(());
        }
        if !self.is_monitor(id) {
            bail!("{} {}", t("Немає монітора", "No monitor"), id.number() + 1);
        }
        if self.monitor_count() == 1 {
            bail!(
                "{}",
                t(
                    "Останній монітор прибрати не можна",
                    "The last monitor cannot be removed"
                )
            );
        }
        self.screens.remove(&id);
        self.forget(id);
        Ok(())
    }

    /// Monitor `id`, no longer among the screens, leaves the router, the order and the
    /// desktop's arrangement.
    fn forget(&mut self, id: ScreenId) {
        self.router.remove_stream(id);
        self.order.retain(|o| *o != id);
        self.send_order();
        if self.primary == Some(id) {
            self.primary = None;
        }
        self.ensure_primary();
        // Close the gap the monitor leaves.
        self.apply_layout();
    }

    /// Tells the headset the order of the monitors.
    pub fn send_order(&self) {
        self.router.send(HostMsg::Order {
            ids: wire_ids(&self.order),
        });
    }

    pub fn set_primary(&mut self, id: ScreenId) {
        let Some(s) = self.screens.get(&id).filter(|s| s.is_virtual()) else {
            return;
        };
        if self.manage_primary
            && let Some(o) = &s.output
        {
            o.make_primary();
        }
        self.primary = Some(id);
        self.router.send(HostMsg::PrimaryChanged { id: id.wire() });
        self.apply_layout();
    }

    /// Lays the outputs out on the desktop like they stand in the headset (see
    /// `prostir_layout::desk`): the monitors in the headset's rows, the laptop screen
    /// under the primary monitor unless the headset placed it, other real displays
    /// next to it and window panels right of everything. The mouse then crosses
    /// screens where the eyes expect it to.
    fn apply_layout(&self) {
        /// An output on the desktop: a screen, or another real display.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum Out {
            Screen(ScreenId),
            Other(usize),
        }
        let geometries = self.desktop.geometries();
        let mut names: BTreeMap<Out, String> = self
            .screens
            .values()
            .filter_map(|s| Some((Out::Screen(s.id), s.output_name()?.to_owned())))
            .collect();
        // Other real displays (the laptop panel next to a mirrored external monitor,
        // say) go on the bottom row too, so nothing overlaps the Prostir monitors.
        let mut others: Vec<&String> = geometries
            .iter()
            .filter(|(name, g)| {
                g.enabled
                    && g.w > 0
                    && !names.values().any(|n| n == *name)
                    && !self.desktop.is_virtual(name)
            })
            .map(|(name, _)| name)
            .collect();
        others.sort();
        for (i, name) in others.into_iter().enumerate() {
            names.insert(Out::Other(i), name.clone());
        }
        let size = |out: Out| {
            let spec = match out {
                Out::Screen(id) => self.screens.get(&id).and_then(|s| s.kind.spec()),
                Out::Other(_) => None,
            };
            let name = names.get(&out)?;
            geometries
                .get(name)
                .map(|g| (g.w, g.h))
                .or_else(|| {
                    spec.map(|s| {
                        (
                            (f64::from(s.width) / s.scale).round() as i32,
                            (f64::from(s.height) / s.scale).round() as i32,
                        )
                    })
                })
                .map(|(w, h)| desk::Size { w, h })
        };
        let plan = desk::plan(
            &desk::Desk {
                rows: self
                    .effective_rows()
                    .into_iter()
                    .map(|r| {
                        r.into_iter()
                            .map(|e| desk::Entry {
                                id: Out::Screen(e.id),
                                center: e.center,
                                middle: e.middle,
                            })
                            .collect()
                    })
                    .collect(),
                main: self
                    .primary
                    .or_else(|| self.order.first().copied())
                    .map(Out::Screen),
                under_main: self.mirror().map(|_| Out::Screen(ScreenId::BUILTIN)),
                bottom: names
                    .keys()
                    .filter(|o| matches!(o, Out::Other(_)))
                    .copied()
                    .collect(),
                column: self.windows().map(|s| Out::Screen(s.id)).collect(),
            },
            size,
        );
        // Every applied change makes KWin reconfigure its outputs, which also cuts the
        // laptop mirror's stream: `place` only sends what actually differs.
        let positions: Vec<(String, i32, i32)> = plan
            .iter()
            .map(|p| (names[&p.id].clone(), p.x, p.y))
            .collect();
        let primary = self
            .primary
            .and_then(|p| self.screens.get(&p))
            .filter(|_| self.manage_primary)
            .and_then(Screen::output_name);
        self.desktop.place(&positions, primary, &geometries);
    }

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
        let mut unused: Vec<ScreenId> = self.order.clone();
        let mut plan: Vec<Option<ScreenId>> = specs
            .iter()
            .map(|s| {
                let i = unused.iter().position(|id| {
                    self.screens
                        .get(id)
                        .and_then(|m| m.kind.spec())
                        .is_some_and(|m| m.width == s.width && m.height == s.height)
                })?;
                Some(unused.remove(i))
            })
            .collect();
        // Remove before adding (to stay under the limit), but never the last one.
        let mut last = None;
        for id in unused {
            if self.monitor_count() > 1 {
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
        let ids: Vec<ScreenId> = plan.into_iter().flatten().collect();
        self.set_order(&ids)?;
        if let Some(&p) = ids.get(primary) {
            self.set_primary(p);
        }
        Ok(())
    }

    /// The headset's rows, kept in step with the monitors (see [`rows_in_step`]).
    fn effective_rows(&self) -> Vec<Vec<desk::Entry<ScreenId>>> {
        rows_in_step(
            &self.rows,
            &self.order,
            |id| self.is_monitor(id),
            // The laptop screen too, where the headset put it (while it shows).
            |id| id == ScreenId::BUILTIN && self.mirror().is_some(),
        )
    }

    /// Arranges the desktop's outputs in the rows the headset shows them in.
    pub fn set_layout(&mut self, rows: &[LayoutRow]) {
        // Offsets are desktop pixels; anything this far out is nonsense.
        let far = |v: Option<i32>| v.map(|v| v.clamp(-desk::FAR, desk::FAR));
        let mut rows: Vec<Vec<desk::Entry<ScreenId>>> = rows
            .iter()
            .map(|r| {
                r.entries
                    .iter()
                    .map(|e| desk::Entry {
                        id: ScreenId::from_wire(e.id),
                        center: far(e.center),
                        middle: far(e.middle),
                    })
                    .collect()
            })
            .collect();
        // The laptop screen's stream is cut for a moment whenever KDE rearranges its
        // outputs, and a headset that sends a layout meanwhile leaves it out. Its spot
        // is kept, or it would come back elsewhere, the headset would move it back, and
        // that would cut it again, round and round.
        let has_mirror = |rows: &[Vec<desk::Entry<ScreenId>>]| {
            rows.iter().flatten().any(|e| e.id == ScreenId::BUILTIN)
        };
        if self.settings.mirror && self.mirror().is_none() && !has_mirror(&rows) {
            for (i, old) in self.rows.iter().enumerate() {
                let Some(spot) = old.iter().find(|e| e.id == ScreenId::BUILTIN) else {
                    continue;
                };
                let at = if i < rows.len() {
                    i
                } else {
                    rows.push(Vec::new());
                    rows.len() - 1
                };
                rows[at].push(*spot);
                break;
            }
        }
        self.rows = rows;
        self.order = self
            .effective_rows()
            .iter()
            .flatten()
            .map(|e| e.id)
            .filter(|id| *id != ScreenId::BUILTIN)
            .collect();
        self.apply_layout();
        self.send_order();
    }

    pub fn set_order(&mut self, ids: &[ScreenId]) -> Result<()> {
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        let current: Vec<ScreenId> = self.monitors().map(|s| s.id).collect();
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
        self.rows = vec![ids.iter().map(|&id| desk::Entry::new(id)).collect()];
        self.apply_layout();
        self.send_order();
        Ok(())
    }

    /// Without a chosen primary, the middle monitor gets the Plasma panel.
    fn ensure_primary(&mut self) {
        if self.primary.is_some_and(|p| self.is_monitor(p)) {
            return;
        }
        let ids = self.order.clone();
        if let Some(&id) = ids.get(ids.len().saturating_sub(1) / 2) {
            self.set_primary(id);
        } else {
            self.apply_layout();
        }
    }

    pub fn primary(&self) -> Option<ScreenId> {
        self.primary
    }

    /// Gives every screen the wallpaper of the primary desktop (Plasma configures
    /// wallpapers per screen, so new monitors would otherwise show the default one).
    pub fn sync_wallpapers(&self) {
        self.desktop.sync_wallpapers();
    }

    fn encoders(&self) -> impl Iterator<Item = (ScreenId, &Encoder)> {
        self.screens
            .values()
            .filter_map(|s| Some((s.id, s.stream.as_ref()?)))
    }

    /// Puts the PC's active window on a panel of its own: an output its size, which
    /// the headset shows as a floating panel. Returns the panel's id.
    pub fn detach_window(&mut self) -> Result<ScreenId> {
        let Some(id) = ScreenId::panels().find(|id| !self.screens.contains_key(id)) else {
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
        let mut output = self.desktop.create(id.number() + 1, &spec)?;
        let encoder = self.start_stream(id, &mut output)?;
        let name = output.output_name().to_owned();
        let kind = ScreenKind::Window {
            origin: window.clone(),
            spec,
        };
        self.screens
            .insert(id, Screen::running(id, kind, output, encoder));
        self.apply_layout();
        if let Err(e) = windows::move_to(&window, &name, true) {
            self.close_window_panel(id);
            return Err(e);
        }
        if !self.streaming
            && let Some(e) = self.screens.get(&id).and_then(|s| s.stream.as_ref())
        {
            e.set_paused(true);
        }
        self.windows_checked = Instant::now() + Duration::from_secs(1);
        // Hyprland lists a new output a moment later: placed again at the next check.
        self.windows_unplaced = true;
        info!("window panel {id} on {name} ({width}x{height})");
        Ok(id)
    }

    /// Removes a window panel; its window goes back where it was detached from (or,
    /// when that monitor is gone, to the others).
    pub fn close_window_panel(&mut self, id: ScreenId) {
        self.close_panel(id, true);
    }

    fn close_panel(&mut self, id: ScreenId, put_back: bool) {
        if !self.screens.get(&id).is_some_and(Screen::is_window) {
            return;
        }
        let Some(screen) = self.screens.remove(&id) else {
            return;
        };
        if let Some(name) = screen.output_name() {
            windows::forget_panel(name);
        }
        if put_back
            && let ScreenKind::Window { origin, .. } = &screen.kind
            && !origin.output.is_empty()
            && let Err(e) = windows::put_back(origin, &origin.output)
        {
            warn!("cannot put the window back on {}: {e:#}", origin.output);
        }
        self.router.remove_stream(id);
        drop(screen);
        self.apply_layout();
    }

    fn close_window_panels(&mut self) {
        let ids: Vec<ScreenId> = self.windows().map(|s| s.id).collect();
        for id in ids {
            self.close_window_panel(id);
        }
    }

    /// A panel whose window was closed (or moved away) goes too.
    fn check_window_panels(&mut self) {
        if self.windows().next().is_none() || self.windows_checked > Instant::now() {
            return;
        }
        self.windows_checked = Instant::now() + Duration::from_secs(1);
        if std::mem::take(&mut self.windows_unplaced) {
            self.apply_layout();
        }
        let names: Vec<String> = self
            .windows()
            .filter_map(|s| s.output_name().map(str::to_owned))
            .collect();
        for name in windows::vacated(&names) {
            if let Some(id) = self.id_of_output(&name) {
                info!("the window on panel {id} is gone: closing the panel");
                self.close_panel(id, false);
            }
        }
    }

    /// Moves the PC's active window to monitor `id` (the one the user looks at).
    pub fn window_here(&self, id: ScreenId) -> Result<()> {
        let output = self
            .screens
            .get(&id)
            .filter(|s| s.is_virtual() || s.is_builtin())
            .and_then(Screen::output_name)
            .map(str::to_owned)
            .context(t("Немає такого монітора", "No such monitor"))?;
        let window = windows::active().context(t(
            "Немає активного вікна на ПК",
            "No active window on the PC",
        ))?;
        windows::move_to(&window, &output, false)
    }

    /// Saves a picture of monitor `id` (all of them where only that is possible).
    pub fn screenshot(&self, id: ScreenId) -> Result<std::path::PathBuf> {
        let screen = self
            .screens
            .get(&id)
            .filter(|s| s.is_virtual() || s.is_window());
        // Where the desktop's screenshot tool cannot take one output by name, the
        // monitor's own stream can. A fresh keyframe, decoded into a PNG; the whole
        // desktop if not.
        if !self.desktop.screenshots_one_output()
            && let Some(encoder) = screen.and_then(|s| s.stream.as_ref())
            && self.streaming
        {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            *self.router.snapshot.lock().unwrap() = Some((id, tx));
            encoder.force_keyframe();
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
        windows::screenshot(screen.and_then(Screen::output_name))
    }

    pub fn force_keyframe(&self, id: ScreenId) {
        if let Some(e) = self.screens.get(&id).and_then(|s| s.stream.as_ref()) {
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
        self.monitors().next().is_some()
            && !self.router.failed.load(Ordering::Relaxed)
            && !self
                .screens
                .values()
                .filter(|s| !s.is_builtin())
                .any(|s| s.output.as_ref().is_none_or(|o| o.closed()))
    }

    /// The set and its rows as they are now, to keep; none without monitors (none
    /// could stream again), so the set kept before stays for the next start.
    pub fn saved_screens(&self) -> Option<SavedScreens> {
        let specs: BTreeMap<ScreenId, MonitorSpec> = self
            .monitors()
            .filter_map(|s| Some((s.id, s.kind.spec()?.clone())))
            .collect();
        if specs.is_empty() {
            return None;
        }
        Some(SavedScreens {
            specs,
            rows: self.effective_rows(),
            primary: self.primary,
        })
    }
}

/// `rows` kept in step with the outputs: only monitors (`monitor`) and other outputs
/// that show (`shown`) stay, each in the first place it is listed in, and monitors of
/// `order` not placed yet go to the end of the bottom row. So every monitor is listed
/// exactly once.
fn rows_in_step<K: Copy + PartialEq>(
    rows: &[Vec<desk::Entry<K>>],
    order: &[K],
    monitor: impl Fn(K) -> bool,
    shown: impl Fn(K) -> bool,
) -> Vec<Vec<desk::Entry<K>>> {
    let mut placed: Vec<K> = Vec::new();
    let mut kept: Vec<Vec<desk::Entry<K>>> = rows
        .iter()
        .map(|r| {
            r.iter()
                .filter(|e| {
                    let new = (monitor(e.id) || shown(e.id)) && !placed.contains(&e.id);
                    if new {
                        placed.push(e.id);
                    }
                    new
                })
                .copied()
                .collect::<Vec<_>>()
        })
        .filter(|r| !r.is_empty())
        .collect();
    let mut missing = Vec::new();
    for &id in order {
        if monitor(id) && !placed.contains(&id) {
            placed.push(id);
            missing.push(desk::Entry::new(id));
        }
    }
    if !missing.is_empty() {
        match kept.last_mut() {
            Some(last) => last.extend(missing),
            None => kept.push(missing),
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

    fn keyframe(router: &Router, id: ScreenId) {
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
        let (s0, s1) = (ScreenId::from_wire(0), ScreenId::from_wire(1));
        router.add_stream(s0, Codec::H265);
        keyframe(&router, s0);
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
        router.add_stream(s0, Codec::H264);
        keyframe(&router, s0);
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
        router.add_stream(s0, Codec::H265);
        router.remove_stream(s0);
        assert!(matches!(sent(&outbox)[..], [HostMsg::StreamEnd { id: 0 }]));
        // Never shown: nothing to end.
        router.add_stream(s1, Codec::H265);
        router.remove_stream(s1);
        assert!(sent(&outbox).is_empty());
    }

    /// Every monitor is in the rows once, in the first place listed: an order made
    /// from them never repeats one (or grows past what a message holds).
    #[test]
    fn rows_list_each_monitor_once() {
        const MIRROR: u8 = 100;
        let at = |id, center| desk::Entry {
            id,
            center: Some(center),
            middle: None,
        };
        let rows = vec![
            vec![at(0, -100), at(0, 0), at(MIRROR, 50)],
            vec![at(1, 10), at(0, 20), at(9, 30)],
        ];
        let monitor = |id| id < 3;
        let kept = rows_in_step(&rows, &[2, 0, 1, 2], monitor, |id| id == MIRROR);
        let ids: Vec<Vec<u8>> = kept
            .iter()
            .map(|r| r.iter().map(|e| e.id).collect())
            .collect();
        assert_eq!(ids, vec![vec![0, MIRROR], vec![1, 2]]);
        assert_eq!(kept[0][0], at(0, -100), "the first place counts");
        let many = vec![vec![at(0, 0); 16]];
        let kept = rows_in_step(&many, &[0, 1], monitor, |_| false);
        assert_eq!(kept[0].iter().map(|e| e.id).collect::<Vec<_>>(), vec![0, 1]);
    }
}
