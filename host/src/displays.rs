//! The set of virtual monitors and their encoders (delivery to the headsets is the
//! [`Router`]'s).

use crate::i18n::t;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{error, info, warn};
use prostir_layout::desk;
use prostir_proto::state;
use prostir_proto::{Codec, HostMsg};

use crate::encoder::{Encoder, EncoderConfig};
use crate::errors::{Counted, UserError, code_of};
use crate::events::EventSink;
use crate::layout::panel_size;
use crate::platform::audio::{self, AudioOut, MicIn};
use crate::platform::dialogs;
use crate::platform::sound::{MicAvailability, SoundStatus};
use crate::platform::{
    ActiveWindow, DeskPlan, Desktop, Geometry, MonitorSpec, Output, OutputHealth, OutputKey,
    PhysicalDisplay, VirtualSpec, WindowList,
};
use crate::router::Router;
use crate::screen::{Screen, ScreenId, ScreenKind, ScreenState, SizeAction};
use crate::store::{LEGACY_LAPTOP, Physical, PhysicalKey, SavedScreens, Settings};

/// Monitors of versions before the counter were numbered from 0 (and their outputs
/// `Prostir-(n + 1)`), below this.
const LEGACY_IDS: u32 = 100;
/// Virtual monitors a headset gets: more streams than this overwhelm its hardware
/// decoders.
pub const MONITORS_MAX: usize = 6;
/// Virtual monitors with "No limits", for a headset that decodes more: the most the PC
/// ever makes (the limit of the headset asking is checked before, see `requests`).
pub const MONITORS_NO_LIMITS: usize = 10;
/// Window panels at once: the headset has only so many video decoders.
pub const WINDOWS_MAX: usize = 3;
/// Displays of the PC kept while not connected: beyond these, the least recently seen
/// are forgotten (a laptop that met many projectors would gather them for ever).
pub const ABSENT_KEPT: usize = 8;

/// While a display of the PC is connected, when it was seen is kept this often (in
/// seconds), and when it goes and the app quits.
const SEEN_EVERY: u64 = 3600;

/// Now, in seconds since 1970, for when a display of the PC was last seen.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

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

/// A headset that waited this long for a fresh picture of a stream that sent nothing
/// meanwhile gets it from the stream starting over.
const STARVED: Duration = Duration::from_secs(3);
/// Between sessions, how often a layout waiting for the desktop to list new outputs
/// looks again.
const LISTING_LOOK: Duration = Duration::from_millis(500);

pub struct Displays {
    // Field order matters: streams close before the desktop connection.
    /// Every screen: virtual monitors, window panels, and every display of the PC
    /// itself seen so far, connected now or not.
    screens: BTreeMap<ScreenId, Screen>,
    /// When the window panels are next checked for a closed window.
    windows_checked: Instant,
    /// How the headset arranges the monitors in rows: every monitor, once, and the
    /// laptop screen where the headset put it (see `effective_rows`). The one source of
    /// their order.
    rows: Vec<Vec<desk::Entry<ScreenId>>>,
    /// The desktop is to be laid out again (see [`Displays::place`]).
    layout_due: bool,
    /// The rows and the layout the desktop took last (see [`Displays::place_now`]).
    applied: (Vec<Vec<Out>>, Vec<desk::Spot<Out>>),
    /// Where each output is on the desktop as laid out last, by name: Prostir's and the
    /// PC's own displays, shown or not.
    on_desk: BTreeMap<OutputKey, state::DeskRect>,
    /// Laid out without an output the desktop did not list yet: again once it does.
    layout_waits: bool,
    /// After the desktop refused a layout: when to try again, and how many times it did.
    layout_retry: (Instant, u32),
    /// When the PC's own displays are looked at next (see `sync_physical`).
    physical_due: Instant,
    /// When each display of the PC was last connected, in seconds since 1970 (0: not
    /// known), to forget the least recently seen of those not connected.
    physical_seen: BTreeMap<ScreenId, u64>,
    /// Displays larger than the headset decodes (see [`Displays::stop_too_big`]): the
    /// size and codec they did not fit in, until a headset or codec they fit.
    too_big: BTreeMap<ScreenId, ((u32, u32), Codec)>,
    /// When a headset last asked for a keyframe of each screen.
    keyframes_asked: BTreeMap<ScreenId, Instant>,
    /// The settings as last applied from the store (see [`Displays::apply`]).
    settings: Settings,
    /// PC sound to the headset, while one is connected.
    audio: Option<AudioOut>,
    mic: Option<MicIn>,
    /// After a failed start, sound and microphone wait longer and longer before the
    /// next attempt (starting them blocks the session for up to 2 s).
    audio_retry: Backoff,
    mic_retry: Backoff,
    /// The headset's microphone switch is on: the PC's end of it is there.
    mic_wanted: bool,
    /// Why the headset microphone could not be made on the PC, while it cannot.
    mic_failed: Option<MicAvailability>,
    /// A program on the PC records from the headset microphone, as the sound system
    /// last said (see [`Displays::set_recording`]).
    recording: bool,
    /// Follows the sound system while sound or the microphone is there; its changes
    /// go to `sound_to` (the control loop).
    sound_events: Option<audio::Watcher>,
    sound_to: Option<EventSink>,
    /// The sound as last written in the log.
    sound_logged: Option<SoundStatus>,
    /// What the PC can encode with (see `encoder::available`), found once.
    choices: Vec<(Codec, bool)>,
    /// A headset with a session is worn (encoders running).
    streaming: bool,
    /// The streams go in H.264 because a headset did not decode the codec chosen:
    /// they go back to it once every headset with a session does.
    fitted_down: bool,
    /// When KDE's primary output was last compared with ours.
    primary_checked: Instant,
    desktop: Box<dyn Desktop>,
    pub router: Arc<Router>,
    cfg: EncoderConfig,
    primary: Option<ScreenId>,
    /// Whether to pick KDE's primary monitor ourselves.
    manage_primary: bool,
}

impl Drop for Displays {
    /// Window panels close first, while the desktop is still there: each window goes
    /// back where it was, out of full screen, and the desktop stops watching it (on KDE,
    /// its KWin script is unloaded). The outputs and the desktop go after.
    fn drop(&mut self) {
        self.close_window_panels();
    }
}

impl Displays {
    /// The screens of `set`, each with its own number, streaming with `cfg`; sound,
    /// microphone and the laptop screen as `settings` say. A display of the PC seen for
    /// the first time gets its number from `new_id`.
    pub fn create(
        set: &SavedScreens,
        settings: &Settings,
        cfg: EncoderConfig,
        manage_primary: bool,
        new_id: &mut dyn FnMut() -> ScreenId,
    ) -> Result<Self> {
        // One device per graphics card for every capture and encoder (step 6.5).
        let desktop = crate::platform::connect(&crate::platform::GpuDevices::default())?;
        // Only some desktops (KDE) have a primary output.
        let manage_primary = manage_primary && desktop.caps().has_primary;
        let mut displays = Self {
            screens: BTreeMap::new(),
            windows_checked: Instant::now(),
            rows: set.rows.clone(),
            layout_due: true,
            applied: (Vec::new(), Vec::new()),
            on_desk: BTreeMap::new(),
            layout_waits: false,
            layout_retry: (Instant::now(), 0),
            physical_due: Instant::now(),
            physical_seen: set.physical.iter().map(|(id, p)| (*id, p.seen)).collect(),
            too_big: BTreeMap::new(),
            keyframes_asked: BTreeMap::new(),
            settings: settings.clone(),
            audio: None,
            mic: None,
            audio_retry: Backoff::default(),
            mic_retry: Backoff::default(),
            mic_wanted: false,
            mic_failed: None,
            recording: false,
            sound_events: None,
            sound_to: None,
            sound_logged: None,
            choices: crate::encoder::available(),
            streaming: true,
            fitted_down: false,
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
        // The PC's own displays as they were known, absent until seen.
        for (id, p) in &set.physical {
            let kind = ScreenKind::Physical {
                key: p.key.clone(),
                label: p.key.connector.clone(),
                builtin: p.builtin,
                shown: p.shown,
            };
            let mut screen = Screen::starting(*id, 0, kind);
            screen.state = ScreenState::Absent;
            displays.screens.insert(*id, screen);
        }
        // Monitors keep the output names they had: monitor 0 is `Prostir-1` as
        // before, others the lowest free number.
        let ids: Vec<ScreenId> = ids.into_iter().take(MONITORS_NO_LIMITS).collect();
        let mut numbers: BTreeMap<ScreenId, u32> = BTreeMap::new();
        for id in &ids {
            let n = id.number() + 1;
            if id.number() < LEGACY_IDS && !numbers.values().any(|v| *v == n) {
                numbers.insert(*id, n);
            }
        }
        for id in &ids {
            if !numbers.contains_key(id) {
                let n = (1..)
                    .find(|n| !numbers.values().any(|v| v == n))
                    .unwrap_or(1);
                numbers.insert(*id, n);
            }
        }
        // One that cannot start stays in the set, Failed; without any running, there
        // is nothing to show.
        let mut first_error = None;
        for id in ids {
            if let Err(e) = displays.add_as(id, numbers[&id], set.specs[&id].clone()) {
                first_error.get_or_insert(e);
            }
        }
        if !displays.monitors().any(Screen::running)
            && let Some(e) = first_error
        {
            return Err(e);
        }
        match set.primary {
            Some(id) if displays.is_monitor(id) || displays.is_physical(id) => {
                let _ = displays.set_primary(id);
            }
            _ => displays.ensure_primary(),
        }
        displays.tick(new_id);
        displays.place();
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

    /// The PC's own displays, connected or not.
    fn physical(&self) -> impl Iterator<Item = &Screen> {
        self.screens.values().filter(|s| s.is_physical())
    }

    /// The lowest output number no screen has now.
    fn free_number(&self) -> u32 {
        (1..)
            .find(|n| {
                !self
                    .screens
                    .values()
                    .any(|s| s.number == *n && (!s.is_physical() || s.output.is_some()))
            })
            .unwrap_or(1)
    }

    /// Whether `id` is one of the virtual monitors.
    pub fn is_monitor(&self, id: ScreenId) -> bool {
        self.screens.get(&id).is_some_and(Screen::is_virtual)
    }

    /// Adds monitor `id` (a number never given before), at most `max` of them; its
    /// output takes the lowest free `Prostir-N`, so KScreen keeps the position and
    /// scale it remembers for that name. One that cannot start stays, `Failed`, and
    /// keeps its place (it is tried again at the next session).
    pub fn add(&mut self, id: ScreenId, spec: MonitorSpec, max: usize) -> Result<()> {
        if self.monitor_count() >= max {
            bail!(UserError::Limit {
                max,
                of: Counted::Monitors
            });
        }
        let number = self.free_number();
        if let Err(e) = self.add_as(id, number, spec) {
            warn!("{id} stays, failed: {e:#}");
        }
        Ok(())
    }

    /// Adds monitor `id` (output `number`) to the set, at the end of the bottom row
    /// unless the rows have it, and starts it; one that cannot start stays, Failed
    /// (and says why).
    fn add_as(&mut self, id: ScreenId, number: u32, spec: MonitorSpec) -> Result<()> {
        self.screens.insert(
            id,
            Screen::starting(id, number, ScreenKind::Virtual { spec }),
        );
        if !self.rows.iter().flatten().any(|e| e.id == id) {
            match self.rows.last_mut() {
                Some(row) => row.push(desk::Entry::new(id)),
                None => self.rows.push(vec![desk::Entry::new(id)]),
            }
        }
        self.start(id)
    }

    /// Makes monitor `id`'s output if it has none and starts its stream: Running, or
    /// Failed with why.
    fn start(&mut self, id: ScreenId) -> Result<()> {
        let Some(screen) = self.screens.get_mut(&id) else {
            return Ok(());
        };
        let Some(spec) = screen.kind.spec().cloned() else {
            return Ok(());
        };
        let output = screen.output.take();
        let number = screen.number;
        let started = output
            .map_or_else(|| self.make(id, number, &spec), Ok)
            .and_then(|mut output| match self.start_stream(id, output.as_mut()) {
                Ok(encoder) => Ok((output, encoder)),
                // The output stays for the next try.
                Err(e) => {
                    if let Some(s) = self.screens.get_mut(&id) {
                        s.output = Some(output);
                    }
                    Err(e)
                }
            });
        let Some(screen) = self.screens.get_mut(&id) else {
            return Ok(());
        };
        let result = match started {
            Ok((output, encoder)) => {
                // Made while the headset is off: paused like the others.
                if !self.streaming {
                    encoder.set_paused(true);
                }
                screen.output = Some(output);
                screen.stream = Some(encoder);
                screen.state = ScreenState::Running;
                Ok(())
            }
            Err(e) => {
                error!("{} cannot start: {e:#}", screen.name());
                screen.state = ScreenState::Failed {
                    error: code_of(&e),
                    why: format!("{e:#}"),
                };
                Err(e)
            }
        };
        self.layout_changed();
        result
    }

    /// Displays too big for the last headset or codec are tried again where they fit
    /// the headset and codec now; the others stay failed, without a start that would
    /// only fail again.
    fn retry_too_big(&mut self) {
        let codec = self.cfg.codec;
        let fit: Vec<ScreenId> = self
            .too_big
            .iter()
            .filter(|(_, (size, _))| self.router.fits(codec, *size))
            .map(|(id, _)| *id)
            .collect();
        for id in fit {
            self.too_big.remove(&id);
            let physical = self.is_physical(id);
            if physical {
                if let Some(s) = self.screens.get_mut(&id) {
                    s.state = ScreenState::Starting;
                }
                self.physical_due = Instant::now();
            } else {
                let _ = self.start(id);
            }
        }
    }

    /// Tries again the monitors that could not start; those that still cannot stay
    /// `Failed` in the list the headset gets.
    fn retry_failed(&mut self) {
        let failed: Vec<ScreenId> = self
            .monitors()
            .filter(|s| matches!(s.state, ScreenState::Failed { .. }))
            .filter(|s| !self.too_big.contains_key(&s.id))
            .map(|s| s.id)
            .collect();
        for id in failed {
            let _ = self.start(id);
        }
    }

    /// A new virtual monitor (or window panel) for screen `id`, as output `number`.
    fn make(&mut self, id: ScreenId, number: u32, spec: &MonitorSpec) -> Result<Box<dyn Output>> {
        let caps = self.desktop.caps();
        if let Err(no) = &caps.virtual_monitors {
            return Err(anyhow::anyhow!("{}", no.why).context(no.error.clone()));
        }
        let spec = VirtualSpec {
            id,
            number,
            w: spec.width,
            h: spec.height,
            scale: spec.percent(),
            refresh: caps.refresh_for_new(),
        };
        self.desktop.make(&spec).context(UserError::Desktop)
    }

    /// Starts stream `id`: its slot in the router and an encoder fed by `output`. When
    /// that fails, nothing of the stream stays registered.
    fn start_stream(&self, id: ScreenId, output: &mut dyn Output) -> Result<Encoder> {
        self.start_encoder(id, output, &self.cfg)
            .context(UserError::Encoder)
            .inspect_err(|_| self.router.remove_stream(id))
    }

    /// A new encoder with `cfg` for stream `id`, fed by `output`. The stream's slot
    /// starts over with it, and stays when this fails.
    fn start_encoder(
        &self,
        id: ScreenId,
        output: &mut dyn Output,
        cfg: &EncoderConfig,
    ) -> Result<Encoder> {
        let (name, physical) = self
            .screens
            .get(&id)
            .map_or((id.to_string(), false), |s| (s.name(), s.is_physical()));
        self.router.add_stream(id, cfg.codec, name, physical);
        let router = self.router.clone();
        let encoder = Encoder::start(&output.source().input, cfg, move |ev| {
            router.on_event(id, ev)
        })?;
        output.attach(&encoder)?;
        Ok(encoder)
    }

    /// Housekeeping, about twice a second: logs what the streams sent now and then,
    /// and follows the PC's own displays (a laptop's panel goes when its lid closes and
    /// comes back when it opens); one seen for the first time gets its number from
    /// `new_id`.
    pub fn tick(&mut self, new_id: &mut dyn FnMut() -> ScreenId) {
        self.router.report();
        self.sync_audio();
        // A way in for a recording that broke is made anew (nothing is asked).
        if self.recording
            && let Some(mic) = &self.mic
        {
            mic.set_recording(true);
        }
        self.check_sizes();
        self.check_primary();
        self.check_window_panels();
        self.stop_too_big();
        // A fresh picture asked of a stream that then stood still never came.
        for id in self.router.starved(STARVED) {
            if self.streaming
                && let Some(e) = self.screens.get(&id).and_then(|s| s.stream.as_ref())
            {
                info!("{id}: no fresh picture came for a headset: starting its stream over");
                e.set_paused(false);
            }
        }
        self.sync_physical(new_id);
    }

    /// Stops the streams larger than every headset decodes (see [`Router::attach`]):
    /// a monitor or a display of the PC stands `Failed` with `BAD_SIZE` and why, and
    /// is tried again with the next headset or codec; a window panel closes, its
    /// window back where it was, and the headsets hear why. One that some headset
    /// decodes goes on for it, failed only for the others (see `displays_part`).
    pub fn stop_too_big(&mut self) {
        for (id, (w, h), codec) in self.router.take_too_big() {
            let Some(s) = self.screens.get(&id) else {
                continue;
            };
            if !self.router.still_too_big(id) {
                continue;
            }
            let why = too_big_why(&s.name(), (w, h), codec);
            warn!("{why}");
            if s.is_window() {
                self.close_panel(id, true);
                self.router.send_all(HostMsg::Problem {
                    error: state::ErrorCode::BAD_SIZE,
                    detail: state::clip(&why, state::MAX_DETAIL),
                    display: None,
                });
                continue;
            }
            if s.is_physical() {
                self.stop_physical(id);
            } else if let Some(s) = self.screens.get_mut(&id) {
                drop(s.stream.take());
                self.router.remove_stream(id);
            }
            if let Some(s) = self.screens.get_mut(&id) {
                s.state = ScreenState::Failed {
                    error: state::ErrorCode::BAD_SIZE,
                    why,
                };
            }
            self.too_big.insert(id, ((w, h), codec));
            self.layout_changed();
        }
    }

    /// Whether display `id` waits for a headset or codec it fits.
    fn is_too_big(&self, id: ScreenId) -> bool {
        self.too_big.contains_key(&id)
    }

    /// The first display streaming now that `headset` would not decode in `codec`:
    /// its size.
    pub fn too_big_for(&self, codec: Codec, headset: &state::HeadsetCaps) -> Option<(u32, u32)> {
        self.screens
            .values()
            .filter(|s| s.stream.is_some())
            .filter_map(|s| {
                self.router
                    .stream_size(s.id)
                    .or_else(|| s.kind.spec().map(|m| (m.width, m.height)))
            })
            .find(|&(w, h)| !headset.fits(codec, w, h))
    }

    /// Puts back the size of monitors another program changed (on KDE, sharing one in
    /// Discord or OBS can shrink it to what that program asks for). Each monitor's
    /// [`crate::screen::SizeGuard`] decides; the desktop is laid out again once the
    /// size is back, at the next [`Displays::place`]. Window panels are left alone: made
    /// anew, one would lose its window.
    fn check_sizes(&mut self) {
        // Paused encoders connect again on resuming, which puts the size back too.
        if !self.streaming {
            return;
        }
        let now = Instant::now();
        let ids: Vec<ScreenId> = self
            .monitors()
            .filter(|s| s.running())
            .map(|s| s.id)
            .collect();
        for id in ids {
            let Some(screen) = self.screens.get_mut(&id) else {
                continue;
            };
            let Some(output) = &screen.output else {
                continue;
            };
            let resized = resized(output.as_ref());
            let action = screen.size_guard.check(resized, now);
            if action == SizeAction::Wait {
                continue;
            }
            let name = output.key().to_string();
            let number = screen.number;
            let Some(spec) = screen.kind.spec().cloned() else {
                continue;
            };
            let (w, h) = resized.unwrap_or_default();
            match action {
                SizeAction::Wait => {}
                SizeAction::Restored => {
                    info!("{name} is {spec} again");
                    self.layout_changed();
                }
                // Laid out once the size is back: the desktop still lists the wrong one.
                SizeAction::Restart => {
                    warn!(
                        "{name} was resized to {w}x{h} by another program (screen sharing?): restoring {spec}"
                    );
                    if let Err(e) = self.restart_monitor(id, None) {
                        error!("{id}: {e:#}");
                    }
                    show_sharing_tip(number);
                }
                SizeAction::Recreate => {
                    warn!("{name} is still {w}x{h}: making it again at {spec}");
                    self.recreate(id);
                }
                SizeAction::GiveUp => {
                    warn!("{name} keeps being resized by another program: leaving it at {w}x{h}");
                    let body = format!(
                        "{} {}. {}",
                        t(
                            "Інша програма раз у раз змінює розмір монітора",
                            "Another program keeps changing the size of monitor"
                        ),
                        number,
                        t(
                            "Покажіть у ній вікно програми замість монітора, а тоді зніміть і знову вдягніть шолом.",
                            "Share an app window in it instead of the monitor, then take the headset off and put it on again."
                        )
                    );
                    dialogs::notify(t("Розмір монітора", "Monitor size"), &body);
                }
            }
        }
    }

    /// Makes monitor `id`'s output anew with the same number and spec, when nothing
    /// else gives it back its size. It keeps its place in the set and the rows; its
    /// windows move to the other screens. One that cannot start again stays, Failed.
    fn recreate(&mut self, id: ScreenId) {
        let Some(screen) = self.screens.get_mut(&id) else {
            return;
        };
        // The encoder stops before the output goes away.
        drop(screen.stream.take());
        drop(screen.output.take());
        screen.state = ScreenState::Starting;
        if self.start(id).is_err() {
            return;
        }
        if self.manage_primary
            && self.primary == Some(id)
            && let Some(output) = self.screens.get(&id).and_then(|s| s.output.as_ref())
        {
            output.make_primary();
        }
        self.sync_wallpapers();
    }

    /// Follows the PC's own displays: each connected one is matched to one known (see
    /// [`match_physical`]); one seen for the first time gets a number from `new_id` and
    /// is shown if it is the one the desktop shows by default and the laptop screen is
    /// wanted. Shown ones that are connected stream (and start again when their
    /// stream stopped); one not connected is `Absent`, keeping its number and place.
    fn sync_physical(&mut self, new_id: &mut dyn FnMut() -> ScreenId) {
        let mut stopped = self.router.take_broken();
        stopped.extend(
            self.physical()
                .filter(|s| s.output.as_ref().is_some_and(|o| closed(o.as_ref())))
                .map(|s| s.id),
        );
        for id in &stopped {
            info!("the stream of {id} stopped, starting it again shortly");
            self.stop_physical(*id);
        }
        if !stopped.is_empty() {
            self.physical_due = Instant::now() + Duration::from_secs(2);
        }
        if Instant::now() < self.physical_due {
            return;
        }
        self.physical_due = Instant::now() + Duration::from_secs(2);
        let seen = self.desktop.physical();
        let known: Vec<(ScreenId, PhysicalKey)> = self
            .physical()
            .filter_map(|s| match &s.kind {
                ScreenKind::Physical { key, .. } => Some((s.id, key.clone())),
                _ => None,
            })
            .collect();
        let matched = match_physical(&known, &seen);
        let default = seen.iter().find(|d| d.default).map(|d| d.key.clone());
        let mut present = Vec::new();
        for (display, id) in seen.iter().zip(matched) {
            let id = match id {
                Some(id) => id,
                None => self.found_physical(display, &seen, default.as_ref(), new_id),
            };
            present.push(id);
            if let Some(s) = self.screens.get_mut(&id)
                && let ScreenKind::Physical { key, label, .. } = &mut s.kind
            {
                label.clone_from(&display.label);
                if key.connector != display.key.as_str() {
                    info!("{id} is on {} now", display.key);
                    key.connector = display.key.to_string();
                }
                // Known by its connector until now: by its EDID from now on.
                if key.edid.is_none()
                    && let Some(edid) = unique_edid(display, &seen)
                {
                    info!("{id} says it is {edid}");
                    key.edid = Some(edid.to_owned());
                }
            }
        }
        let ids: Vec<ScreenId> = self.physical().map(|s| s.id).collect();
        let mut changed = false;
        for id in ids {
            let shown = self.screens.get(&id).is_some_and(Screen::is_shown_physical);
            let here = present.contains(&id);
            let Some(s) = self.screens.get_mut(&id) else {
                continue;
            };
            let streaming = s.output.is_some();
            // Seen now: when it comes or goes, and every hour while connected (each
            // change rewrites screens.txt).
            let seen = self.physical_seen.get(&id).copied().unwrap_or(0);
            if here == matches!(s.state, ScreenState::Absent)
                || (here && now_secs().saturating_sub(seen) >= SEEN_EVERY)
            {
                self.physical_seen.insert(id, now_secs());
            }
            if !here {
                if !matches!(s.state, ScreenState::Absent) {
                    info!("{} is not connected", s.name());
                    changed = true;
                }
                self.stop_physical(id);
                if let Some(s) = self.screens.get_mut(&id) {
                    s.state = ScreenState::Absent;
                }
            } else if !shown {
                if streaming {
                    self.stop_physical(id);
                    changed = true;
                }
                if let Some(s) = self.screens.get_mut(&id)
                    && !matches!(s.state, ScreenState::Running)
                {
                    s.state = ScreenState::Running;
                    changed = true;
                }
            } else if !streaming && !self.is_too_big(id) {
                // One too big for this headset waits for another headset or codec.
                self.start_physical(id);
                changed = true;
            }
        }
        changed |= self.forget_long_absent();
        if seen.is_empty() && self.physical().any(Screen::is_shown_physical) {
            self.desktop.wake_physical();
        }
        if changed {
            self.layout_changed();
        }
    }

    /// The displays of the PC connected now were seen now (the app quits: what is
    /// saved says when each was last seen).
    pub fn note_connected(&mut self) {
        let now = now_secs();
        let connected: Vec<ScreenId> = self
            .physical()
            .filter(|s| !matches!(s.state, ScreenState::Absent))
            .map(|s| s.id)
            .collect();
        for id in connected {
            self.physical_seen.insert(id, now);
        }
    }

    /// Forgets the displays of the PC not connected beyond [`ABSENT_KEPT`], the least
    /// recently seen first: they leave the list, the rows and the saved set. Whether
    /// any was forgotten.
    fn forget_long_absent(&mut self) -> bool {
        let absent: Vec<(ScreenId, u64)> = self
            .physical()
            .filter(|s| matches!(s.state, ScreenState::Absent))
            .map(|s| (s.id, self.physical_seen.get(&s.id).copied().unwrap_or(0)))
            .collect();
        let gone = least_recently_seen(absent, ABSENT_KEPT);
        for id in &gone {
            info!("forgetting {id}, not connected for the longest");
            self.screens.remove(id);
            self.physical_seen.remove(id);
            self.forget(*id);
        }
        !gone.is_empty()
    }

    /// A display of the PC seen for the first time: a new number (the laptop screen of
    /// earlier versions keeps 100 and its place), shown if it is the one the desktop
    /// shows by default and the laptop screen is wanted. Known by its EDID where no
    /// other connected display says the same, else by its connector.
    fn found_physical(
        &mut self,
        display: &PhysicalDisplay,
        seen: &[PhysicalDisplay],
        default: Option<&OutputKey>,
        new_id: &mut dyn FnMut() -> ScreenId,
    ) -> ScreenId {
        let is_default = default == Some(&display.key);
        let legacy = ScreenId::saved(LEGACY_LAPTOP);
        let id = if is_default
            && !self.screens.contains_key(&legacy)
            && self.rows.iter().flatten().any(|e| e.id == legacy)
        {
            legacy
        } else {
            new_id()
        };
        let unique = |edid: &String| {
            seen.iter()
                .filter(|d| d.edid.as_ref() == Some(edid))
                .count()
                == 1
        };
        let key = PhysicalKey {
            edid: display.edid.clone().filter(unique),
            connector: display.key.to_string(),
        };
        let shown = is_default && self.settings.mirror;
        self.physical_seen.insert(id, now_secs());
        info!(
            "{} is {id} ({}), {}",
            display.key,
            key.edid.as_deref().unwrap_or("no EDID"),
            if shown { "shown" } else { "not shown" }
        );
        let kind = ScreenKind::Physical {
            key,
            label: display.label.clone(),
            builtin: display.builtin,
            shown,
        };
        self.screens.insert(id, Screen::starting(id, 0, kind));
        self.layout_changed();
        id
    }

    /// Streams display `id` of the PC: `Running`, or `Failed` with why (tried again).
    fn start_physical(&mut self, id: ScreenId) {
        let number = self.free_number();
        let Some(s) = self.screens.get_mut(&id) else {
            return;
        };
        s.number = number;
        let name = s.name();
        let ScreenKind::Physical { key, .. } = &s.kind else {
            return;
        };
        let key = OutputKey::new(key.connector.as_str());
        let started = self.desktop.show(&key).and_then(|mut output| {
            let encoder = self.start_stream(id, output.as_mut())?;
            Ok((output, encoder))
        });
        let Some(s) = self.screens.get_mut(&id) else {
            return;
        };
        match started {
            Ok((output, encoder)) => {
                if !self.streaming {
                    encoder.set_paused(true);
                }
                encoder.force_keyframe();
                s.output = Some(output);
                s.stream = Some(encoder);
                s.state = ScreenState::Running;
            }
            Err(e) => {
                warn!("cannot show {name}: {e:#}");
                let e = e.context(UserError::Capture);
                s.state = ScreenState::Failed {
                    error: code_of(&e),
                    why: format!("{e:#}"),
                };
            }
        }
    }

    /// Stops the stream of display `id` of the PC, if it streams.
    fn stop_physical(&mut self, id: ScreenId) {
        let Some(s) = self.screens.get_mut(&id) else {
            return;
        };
        // The encoder stops before the output goes away.
        let had = s.stream.take().is_some();
        drop(s.output.take());
        if had {
            self.router.remove_stream(id);
            self.layout_changed();
        }
    }

    /// Shows or hides display `id` of the PC in the headset, with at most `max` of them
    /// shown.
    pub fn show_physical(&mut self, id: ScreenId, show: bool, max: usize) -> Result<()> {
        let others = self
            .physical()
            .filter(|s| s.id != id && s.is_shown_physical())
            .count();
        let Some(ScreenKind::Physical { shown, .. }) =
            self.screens.get_mut(&id).map(|s| &mut s.kind)
        else {
            bail!(UserError::UnknownDisplay);
        };
        if show && others >= max {
            bail!(UserError::Limit {
                max,
                of: Counted::Physical
            });
        }
        *shown = show;
        // Streamed or stopped at the next look, right away.
        self.physical_due = Instant::now();
        Ok(())
    }

    /// The displays of the PC itself, connected or not.
    pub fn physical_ids(&self) -> Vec<ScreenId> {
        self.physical().map(|s| s.id).collect()
    }

    /// Whether `id` is a display of the PC itself that the headset shows.
    pub fn is_shown_physical(&self, id: ScreenId) -> bool {
        self.screens.get(&id).is_some_and(Screen::is_shown_physical)
    }

    /// Whether `id` is a display of the PC itself.
    pub fn is_physical(&self, id: ScreenId) -> bool {
        self.screens.get(&id).is_some_and(Screen::is_physical)
    }

    /// The displays of the PC shown in the headset now.
    pub fn physical_shown(&self) -> usize {
        self.physical().filter(|s| s.is_shown_physical()).count()
    }

    /// The window panels.
    pub fn window_count(&self) -> usize {
        self.windows().count()
    }

    /// The streams there are, or are to be: every monitor and window panel, and every
    /// display of the PC that is shown and connected.
    pub fn stream_count(&self) -> usize {
        self.screens
            .values()
            .filter(|s| {
                !s.is_physical()
                    || (s.is_shown_physical() && !matches!(s.state, ScreenState::Absent))
            })
            .count()
    }

    /// Makes sure the desktop's primary output (where the Plasma panel lives) is ours.
    /// Right after outputs come and go (a profile switch) KDE can end up with another one.
    fn check_primary(&mut self) {
        if !self.manage_primary || self.primary_checked.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.primary_checked = Instant::now();
        let Some(key) = self
            .primary
            .and_then(|id| self.screens.get(&id))
            .and_then(Screen::output_key)
            .cloned()
        else {
            return;
        };
        self.desktop.keep_primary(&key);
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
        }
        self.sync_audio();
    }

    /// The headset's microphone switch (its `UpState`): the PC's end of the microphone
    /// is there while it is on.
    pub fn set_mic(&mut self, on: bool) {
        if on != self.mic_wanted {
            self.mic_wanted = on;
            // Switched on by hand: tried right away.
            self.mic_retry.succeeded();
            self.sync_audio();
        }
    }

    /// A program on the PC records from the headset's microphone, as the sound system
    /// last said.
    pub fn recording(&self) -> bool {
        self.recording && self.mic.is_some()
    }

    /// The sound system said a program began (`on`) or stopped recording from the
    /// headset's microphone: a recording gets its own way in for the headset's sound.
    pub fn set_recording(&mut self, on: bool) {
        let Some(mic) = &self.mic else {
            return;
        };
        if on != self.recording {
            info!(
                "{}",
                if on {
                    "a program on the PC records from the headset microphone"
                } else {
                    "no program on the PC records from the headset microphone"
                }
            );
        }
        self.recording = on;
        mic.set_recording(on);
    }

    /// Where the sound system's changes go (the control loop), once sound or the
    /// microphone is there.
    pub fn send_sound_to(&mut self, events: EventSink) {
        self.sound_to = Some(events);
    }

    /// Where the PC's sound goes, and the headset microphone on the PC.
    pub fn sound_status(&self) -> SoundStatus {
        let mic = match (&self.mic, &self.mic_failed) {
            _ if !(self.streaming && self.mic_wanted) => None,
            (Some(_), _) => Some(MicAvailability::Ready),
            (None, Some(why)) => Some(why.clone()),
            (None, None) => Some(MicAvailability::Failed(
                t("Мікрофон ще не готовий", "The microphone is not ready yet").into(),
            )),
        };
        SoundStatus {
            route: self.audio.as_ref().map(AudioOut::route),
            output: self.audio.as_ref().map(AudioOut::device),
            mic,
        }
    }

    /// The codec the streams are in now.
    pub fn codec(&self) -> Codec {
        self.cfg.codec
    }

    /// What the encoders make now: the codec, and whether on the CPU.
    fn encoding_now(&self) -> (Codec, bool) {
        (
            self.cfg.codec,
            self.cfg.backend == crate::encoder::Backend::X264,
        )
    }

    /// The streams go in a codec every headset with a session (`headsets`) decodes:
    /// one in a codec some headset does not decode (a choice saved with another
    /// headset) starts over in H.264, which every headset decodes. The choice stays
    /// saved, and the streams go back to it once every headset decodes it again.
    pub fn fit_codec(&mut self, headsets: &[state::HeadsetCaps]) {
        if headsets.is_empty() {
            return;
        }
        // The one chosen where it was put aside for a headset, else what encodes now.
        let base = if self.fitted_down {
            (self.settings.codec, self.settings.software)
        } else {
            self.encoding_now()
        };
        if let Err(e) = self.encode_for(base.0, base.1, headsets) {
            warn!("{e:#}");
        }
    }

    /// The codec the streams go in when `codec` is chosen and the headsets with a
    /// session are `headsets`: it, where every one of them decodes it, else H.264.
    pub fn codec_for(codec: Codec, headsets: &[state::HeadsetCaps]) -> Codec {
        if codec == Codec::H264 || headsets.iter().all(|h| h.decodes(codec)) {
            codec
        } else {
            Codec::H264
        }
    }

    /// Encodes `codec` (on the CPU with `software`) as far as `headsets` decode it
    /// (see [`Displays::codec_for`]), with one switch at most; the choice stays
    /// `codec`, and the streams go back to it once every headset decodes it.
    pub fn encode_for(
        &mut self,
        codec: Codec,
        software: bool,
        headsets: &[state::HeadsetCaps],
    ) -> Result<()> {
        let fits = Self::codec_for(codec, headsets);
        let now = self.encoding_now().0;
        if fits != codec && now != fits {
            info!("a headset does not decode {codec:?}: streaming H.264");
        } else if self.fitted_down && fits == codec && now != fits {
            info!("every headset decodes {codec:?}: streaming it again");
        }
        self.fitted_down = fits != codec;
        self.encode_with(fits, software)
    }

    /// Every screen as the headset that can do what `headset` says is told about it
    /// (`Displays`): in the order of the rows, then those the rows leave out; window
    /// panels and the PC's own displays only for a headset that shows them; one
    /// streaming larger than it decodes, or beyond the streams it takes at once,
    /// failed for it. Texts are cut to fit.
    pub fn displays_part(&self, headset: &state::HeadsetCaps) -> state::Displays {
        use prostir_proto::state::{
            DeskRect, Display, DisplayKind, DisplayState, Mode, caps, clip,
        };
        let mut order: Vec<ScreenId> = self
            .effective_rows()
            .iter()
            .flatten()
            .map(|e| e.id)
            .collect();
        let rest: Vec<ScreenId> = self
            .screens
            .keys()
            .filter(|id| !order.contains(id))
            .copied()
            .collect();
        order.extend(rest);
        let shown = |s: &Screen| match s.kind {
            ScreenKind::Virtual { .. } => true,
            ScreenKind::Window { .. } => headset.has(caps::headset::WINDOWS),
            ScreenKind::Physical { .. } => headset.has(caps::headset::PHYSICAL),
        };
        // Streams this headset takes at once: those after it, in the order of the
        // list, stand failed for it alone.
        let streams_max = streams_max(headset);
        let mut streams = 0;
        let several = self.router.receivers() > 1;
        let displays = order
            .into_iter()
            .filter_map(|id| self.screens.get(&id))
            .filter(|s| shown(s))
            .map(|s| {
                let kind = match &s.kind {
                    ScreenKind::Virtual { .. } => DisplayKind::Virtual,
                    ScreenKind::Window { .. } => DisplayKind::Window {
                        title: String::new(),
                    },
                    ScreenKind::Physical {
                        label,
                        builtin,
                        shown,
                        ..
                    } => DisplayKind::Physical {
                        builtin: *builtin,
                        connector: clip(label, state::MAX_CONNECTOR),
                        shown: *shown,
                    },
                };
                let (w, h) = self
                    .router
                    .stream_size(s.id)
                    .or_else(|| s.kind.spec().map(|m| (m.width, m.height)))
                    .unwrap_or_default();
                let scale = s.kind.spec().map_or(100, MonitorSpec::percent);
                // A display of the PC is on the desktop under its connector, shown or not.
                let output = match &s.kind {
                    ScreenKind::Physical { key, .. } => Some(key.connector.as_str()),
                    _ => s.output_key().map(OutputKey::as_str),
                };
                let desk: Option<DeskRect> = output
                    .filter(|_| !matches!(s.state, ScreenState::Absent))
                    .and_then(|name| self.on_desk.get(name).copied());
                // Streaming for other headsets, larger than this one decodes.
                let too_big = self
                    .router
                    .stream_format(s.id)
                    .filter(|((w, h), codec)| !headset.fits(*codec, *w, *h));
                // Its size not known yet: starting still for a headset it might not fit
                // (as made, larger than it decodes; a display of the PC, as large as
                // it is, while several headsets watch).
                let pending = self.router.stream_format(s.id).is_none()
                    && s.kind.spec().map_or(several, |m| {
                        !headset.fits(self.cfg.codec, m.width, m.height)
                    });
                let state = match (&s.state, too_big) {
                    (ScreenState::Running, Some((size, codec))) => DisplayState::Failed {
                        error: state::ErrorCode::BAD_SIZE,
                        detail: clip(&too_big_why(&s.name(), size, codec), state::MAX_DETAIL),
                    },
                    (ScreenState::Starting, _) => DisplayState::Starting,
                    (ScreenState::Running, None) if pending => DisplayState::Starting,
                    (ScreenState::Running, None) => DisplayState::Running,
                    (ScreenState::Failed { error, why }, _) => DisplayState::Failed {
                        error: *error,
                        detail: clip(why, state::MAX_DETAIL),
                    },
                    (ScreenState::Absent, _) => DisplayState::Absent,
                };
                let streams_here = matches!(state, DisplayState::Running | DisplayState::Starting)
                    && (!s.is_physical() || s.is_shown_physical());
                let state = if streams_here && streams >= streams_max {
                    let why = format!(
                        "{}: {} ({streams_max})",
                        s.name(),
                        t(
                            "більше потоків, ніж шолом бере",
                            "more streams than the headset takes"
                        )
                    );
                    DisplayState::Failed {
                        error: state::ErrorCode::LIMIT,
                        detail: clip(&why, state::MAX_DETAIL),
                    }
                } else {
                    streams += usize::from(streams_here);
                    state
                };
                Display {
                    id: s.id.wire(),
                    kind,
                    name: clip(&s.name(), state::MAX_DISPLAY_NAME),
                    mode: Mode {
                        w,
                        h,
                        scale: scale.max(1),
                    },
                    desk,
                    state,
                }
            })
            .collect::<Vec<_>>();
        let displays = within_limit(displays);
        let primary = self
            .primary
            .map(ScreenId::wire)
            .filter(|p| displays.iter().any(|d| d.id == *p));
        state::Displays { displays, primary }
    }

    /// How the PC encodes, as the headset is told (`Encoding`): only codecs it decodes.
    pub fn encoding_part(
        &self,
        wanted: &Settings,
        headset: &state::HeadsetCaps,
    ) -> state::Encoding {
        let decodes = |c: Codec| c == Codec::H264 || headset.decodes(c);
        let (codec, software) = self.encoding_now();
        state::Encoding {
            wanted: state::Wanted {
                bitrate: u16::try_from(wanted.bitrate).unwrap_or(u16::MAX),
                codec: Some(wanted.codec).filter(|c| decodes(*c)),
                software: wanted.software,
                sharp: wanted.sharp,
            },
            using: Some((codec, !software)).filter(|(c, _)| decodes(*c)),
            choices: self
                .choices
                .iter()
                .copied()
                .filter(|(c, _)| decodes(*c))
                .collect(),
        }
    }

    /// Where the PC's sound goes, and its end of the headset microphone (`Sound`), as
    /// the headset is told whose microphone it takes (`mic`) or not (it is off then).
    pub fn sound_part(&self, mic: bool) -> state::Sound {
        let mic = match (&self.mic, &self.mic_failed) {
            _ if !mic => state::MicDevice::Off,
            (Some(_), _) => state::MicDevice::Ready,
            (None, Some(why)) if self.mic_wanted => why.device(),
            (None, _) => state::MicDevice::Off,
        };
        state::Sound {
            to_headset: self.settings.audio,
            mic,
        }
    }

    /// Whether the desktop can save a picture of one output by itself.
    pub fn screenshots_one_output(&self) -> bool {
        self.desktop.caps().screenshot_one_output
    }

    /// The scales the desktop gives an output of `w`x`h` pixels.
    pub fn scales_for(&self, w: u32, h: u32) -> crate::platform::Scales {
        self.desktop.scales_for(w, h)
    }

    /// Whether the desktop lets the mouse be moved from here.
    pub fn moves_mouse(&self) -> bool {
        self.desktop.caps().moves_pointer
    }

    /// The desktop's name, for the headset.
    pub fn desktop_name(&self) -> &'static str {
        self.desktop.caps().name
    }

    /// The screen the user works on, as the desktop says.
    pub fn active_output(&mut self) -> Option<ScreenId> {
        let key = self.desktop.windows()?.active_output()?;
        self.id_of_output(&key)
    }

    /// The desktop's list of windows, for a thread of its own.
    pub fn window_list(&mut self) -> Option<Arc<dyn WindowList>> {
        Some(self.desktop.windows()?.list())
    }

    /// Our outputs (monitors, window panels, the mirror) with their desktop geometry.
    fn output_geometries(&self) -> Vec<(ScreenId, Geometry)> {
        let geometries = self.desktop.outputs();
        self.screens
            .values()
            .filter_map(|s| Some((s.id, *geometries.get(s.output_key()?)?)))
            .collect()
    }

    /// Where the mouse is: the screen and the point across its image (0..=65535).
    pub fn find_mouse(&mut self) -> Option<(ScreenId, u16, u16)> {
        let (x, y) = self.desktop.pointer()?.position()?;
        let frac = |v: i32, start: i32, len: i32| {
            ((f64::from(v - start) / f64::from(len.max(1))).clamp(0.0, 1.0) * 65535.0) as u16
        };
        self.output_geometries()
            .into_iter()
            .find(|(_, g)| x >= g.x && x < g.x + g.w && y >= g.y && y < g.y + g.h)
            .map(|(id, g)| (id, frac(x, g.x, g.w), frac(y, g.y, g.h)))
    }

    /// Puts the mouse on screen `id` at `x`/`y` across its image (0..=65535).
    pub fn mouse_to(&mut self, id: ScreenId, x: u16, y: u16) {
        let Some((_, g)) = self.output_geometries().into_iter().find(|(i, _)| *i == id) else {
            warn!("mouse: no {id} on the desktop");
            return;
        };
        let at =
            |start: i32, len: i32, f: u16| start + (f64::from(len) * f64::from(f) / 65535.0) as i32;
        let (cx, cy) = (at(g.x, g.w - 1, x), at(g.y, g.h - 1, y));
        if self.desktop.pointer().is_some_and(|p| p.move_to(cx, cy)) {
            info!("mouse moved to {id} at {cx},{cy}");
        } else {
            warn!("could not move the mouse to {cx},{cy}");
        }
    }

    /// Another codec or the CPU encoder, asked for in the headset menu: every stream
    /// starts over with what the PC can make of it (the monitors and their windows
    /// stay). Nothing changes when that is what encodes now.
    pub fn encode_with(&mut self, codec: Codec, software: bool) -> Result<()> {
        if (codec, software) == self.encoding_now() {
            return Ok(());
        }
        let (backend, codec) = crate::encoder::pick(codec, software).context(UserError::Encoder)?;
        info!("encoding with {backend:?} {codec:?} (from the headset menu)");
        self.switch_encoding(backend, codec)
            .context(UserError::Encoder)
    }

    /// Encodes with `backend` and `codec` from now on: every monitor's stream starts
    /// over with them. When one cannot, all go back to how they encoded before.
    fn switch_encoding(&mut self, backend: crate::encoder::Backend, codec: Codec) -> Result<()> {
        let before = self.cfg.clone();
        self.cfg.backend = backend;
        self.cfg.codec = codec;
        // Monitors and window panels (their windows stay on them).
        let ids: Vec<ScreenId> = self
            .screens
            .values()
            .filter(|s| s.is_virtual() || s.is_window())
            .map(|s| s.id)
            .collect();
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
        // The PC's displays start again by themselves, right away.
        let physical: Vec<ScreenId> = self.physical().map(|s| s.id).collect();
        for id in physical {
            self.stop_physical(id);
        }
        self.physical_due = Instant::now();
        // Displays too big for the codec before may fit this one.
        self.retry_too_big();
        Ok(())
    }

    /// A new encoder for monitor `id` with the current settings, or else with
    /// `fallback`; the old one stops before, so the headset never gets frames of two
    /// codecs for one stream. Returns why the current settings did not work. A monitor
    /// whose stream cannot start at all stays in the set, Failed.
    fn restart_monitor(&mut self, id: ScreenId, fallback: Option<&EncoderConfig>) -> Result<()> {
        // One too big for the headset starts again only where it fits now (it is
        // tried again with the next headset otherwise).
        let size = self
            .screens
            .get(&id)
            .and_then(|s| s.kind.spec())
            .map(|m| (m.width, m.height));
        if self.too_big.contains_key(&id)
            && size.is_some_and(|size| !self.router.fits(self.cfg.codec, size))
        {
            return Ok(());
        }
        let Some(screen) = self.screens.get_mut(&id) else {
            return Ok(());
        };
        let Some(mut output) = screen.output.take() else {
            return Ok(());
        };
        output.detach();
        drop(screen.stream.take());
        let (encoder, result) = match self.start_encoder(id, output.as_mut(), &self.cfg) {
            Ok(encoder) => (Some(encoder), Ok(())),
            Err(e) => {
                let again = fallback.and_then(|cfg| {
                    self.start_encoder(id, output.as_mut(), cfg)
                        .inspect_err(|e| error!("{id}: {e:#}"))
                        .ok()
                });
                (again, Err(e))
            }
        };
        match encoder {
            Some(encoder) => {
                encoder.force_keyframe();
                // Streams again (one too big for the headset is found again by the
                // router once its encoder says its size).
                self.too_big.remove(&id);
                if let Some(screen) = self.screens.get_mut(&id) {
                    screen.stream = Some(encoder);
                    screen.output = Some(output);
                    screen.state = ScreenState::Running;
                }
                self.layout_changed();
            }
            // A window panel closes, its window back where it was.
            None if self.screens.get(&id).is_some_and(Screen::is_window) => {
                error!("{id}: no stream; the window panel closes");
                self.router.remove_stream(id);
                if let Some(screen) = self.screens.get_mut(&id) {
                    screen.output = Some(output);
                }
                self.close_panel(id, true);
            }
            // It keeps its place, and is tried again at the next session.
            None => {
                error!("{id}: no stream; trying again at the next session");
                self.router.remove_stream(id);
                if let Some(screen) = self.screens.get_mut(&id) {
                    screen.output = Some(output);
                    screen.state = ScreenState::Failed {
                        error: state::ErrorCode::ENCODER,
                        why: result
                            .as_ref()
                            .err()
                            .map_or_else(String::new, |e| format!("{e:#}")),
                    };
                }
                self.layout_changed();
            }
        }
        result
    }

    pub fn monitor_count(&self) -> usize {
        self.monitors().count()
    }

    /// Whether a display of the PC streams to the headset.
    pub fn mirror_active(&self) -> bool {
        self.physical().any(|s| s.output.is_some())
    }

    /// Streams while a headset with a session is worn; otherwise the encoders pause
    /// and sound and microphone go back to the PC (the monitors and windows stay).
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

    /// A headset just connected (the router is attached): every stream starts over
    /// once, so each monitor sends its picture now, monitors kept from the last
    /// session too (the other headsets get a keyframe of it). The encoders run from
    /// here; [`Displays::worn`] pauses them again if no headset is worn.
    pub fn headset_connected(&mut self) {
        self.retry_too_big();
        self.retry_failed();
        // Paused ones (kept from before) start over as `set_streaming` resumes them;
        // running ones (just made, or streaming to other headsets) get a fresh
        // picture here.
        if self.streaming {
            let ids: Vec<ScreenId> = self.encoders().map(|(id, _)| id).collect();
            for id in ids {
                self.fresh_picture(id);
            }
        }
        self.set_streaming(true);
    }

    /// Whether a headset with a session is worn (`any`): the encoders run while one
    /// is. `fresh` are the streams a headset put on again needs a fresh picture of,
    /// which resuming the encoders gives anyway.
    pub fn worn(&mut self, any: bool, fresh: &[ScreenId]) {
        let resumes = any && !self.streaming;
        self.set_streaming(any);
        if !resumes {
            for id in fresh {
                self.fresh_picture(*id);
            }
        }
    }

    pub fn streaming(&self) -> bool {
        self.streaming
    }

    /// Starts or stops PC sound and microphone to match the wishes and the state, and
    /// the watcher of the sound system with them (it starts first, so it sees them
    /// come).
    fn sync_audio(&mut self) {
        let audio = self.streaming && self.settings.audio;
        let mic = self.streaming && self.mic_wanted;
        if (audio || mic)
            && self.sound_events.is_none()
            && let Some(events) = &self.sound_to
        {
            self.sound_events = audio::watch(events.clone());
        }
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
        if mic && self.mic.is_none() && self.mic_retry.due() {
            match MicIn::start() {
                Ok(m) => {
                    self.mic = Some(m);
                    self.mic_failed = None;
                    self.mic_retry.succeeded();
                }
                Err(e) => {
                    warn!("no headset microphone: {e:#}");
                    self.mic_failed = Some(MicAvailability::of_failure(&e));
                    self.mic_retry.failed();
                }
            }
        } else if !mic {
            self.mic = None;
        }
        if self.mic.is_none() {
            self.recording = false;
        }
        if self.audio.is_none() && self.mic.is_none() {
            self.sound_events = None;
        }
        let status = self.sound_status();
        if self.sound_logged.as_ref() != Some(&status) {
            info!("sound: {status}");
            self.sound_logged = Some(status);
        }
    }

    pub fn push_mic(&self, pcm: Vec<u8>) {
        if let Some(m) = &self.mic {
            m.push(pcm);
        }
    }

    /// The screen whose output is `key`.
    pub fn id_of_output(&self, key: &OutputKey) -> Option<ScreenId> {
        self.screens
            .values()
            .find(|s| s.output_key() == Some(key))
            .map(|s| s.id)
    }

    /// Removes monitor `id`, or closes window panel `id`.
    pub fn remove(&mut self, id: ScreenId) -> Result<()> {
        if self.screens.get(&id).is_some_and(Screen::is_window) {
            self.close_window_panel(id);
            return Ok(());
        }
        if !self.is_monitor(id) {
            bail!(UserError::UnknownDisplay);
        }
        if self.monitor_count() == 1 {
            bail!(UserError::LastDisplay);
        }
        self.screens.remove(&id);
        self.forget(id);
        Ok(())
    }

    /// Monitor `id`, no longer among the screens, leaves the router, the order and the
    /// desktop's arrangement.
    fn forget(&mut self, id: ScreenId) {
        self.router.remove_stream(id);
        for row in &mut self.rows {
            row.retain(|e| e.id != id);
        }
        self.rows.retain(|r| !r.is_empty());
        if self.primary == Some(id) {
            self.primary = None;
        }
        self.ensure_primary();
        // Close the gap the monitor leaves.
        self.layout_changed();
    }

    /// The monitors left to right, row by row from the top: as the rows have them.
    fn order(&self) -> Vec<ScreenId> {
        self.effective_rows()
            .iter()
            .flatten()
            .map(|e| e.id)
            .filter(|id| self.is_monitor(*id))
            .collect()
    }

    /// Makes monitor or display of the PC `id` the desktop's primary one (where the
    /// panel lives).
    pub fn set_primary(&mut self, id: ScreenId) -> Result<()> {
        let Some(s) = self
            .screens
            .get(&id)
            .filter(|s| s.is_virtual() || s.is_physical())
        else {
            bail!(UserError::UnknownDisplay);
        };
        if self.manage_primary
            && let Some(o) = &s.output
        {
            o.make_primary();
        }
        self.primary = Some(id);
        self.layout_changed();
        Ok(())
    }

    /// Lays the outputs out on the desktop like they stand in the headset (see
    /// `prostir_layout::desk`): the monitors in the headset's rows, the laptop screen
    /// under the primary monitor unless the headset placed it, other real displays
    /// next to it and window panels right of everything. The mouse then crosses
    /// screens where the eyes expect it to.
    /// Lays the desktop out again, once, at [`Displays::place`].
    fn layout_changed(&mut self) {
        self.layout_due = true;
    }

    /// When [`Displays::place`] has something to do without anything else changing: a
    /// layout the desktop refused, at its retry; one waiting for the desktop to list
    /// new outputs, in a while.
    pub fn place_due(&self) -> Option<Instant> {
        if !self.layout_due {
            return None;
        }
        if self.layout_waits {
            return Some(self.layout_retry.0.max(Instant::now() + LISTING_LOOK));
        }
        Some(self.layout_retry.0)
    }

    /// Lays the outputs out on the desktop if anything changed since the last time,
    /// once a pass of the main loop however much changed. Only outputs the desktop
    /// lists already are placed; the others once it lists them. When the desktop
    /// refuses (KScreen stuck, say), the layout stays due and is tried again after
    /// 1, 2, 4... up to 30 s.
    pub fn place(&mut self) {
        if !self.layout_due || Instant::now() < self.layout_retry.0 {
            return;
        }
        match self.place_now() {
            Ok(()) => {
                self.layout_due = self.layout_waits;
                self.layout_retry = (Instant::now(), 0);
            }
            Err(e) => {
                let failures = self.layout_retry.1 + 1;
                let wait = Duration::from_secs((1u64 << failures.min(5)).min(30));
                warn!("{e:#}; trying again in {} s", wait.as_secs());
                // Once for each time it starts failing: the headset's map is not the
                // desktop's meanwhile.
                if failures == 1 {
                    self.router.send_all(HostMsg::Problem {
                        error: state::ErrorCode::DESKTOP,
                        detail: state::clip(&format!("{e:#}"), state::MAX_DETAIL),
                        display: None,
                    });
                }
                self.layout_retry = (Instant::now() + wait, failures);
            }
        }
    }

    fn place_now(&mut self) -> Result<()> {
        let geometries = self.desktop.outputs();
        let mut names: BTreeMap<Out, OutputKey> = self
            .screens
            .values()
            .filter_map(|s| Some((Out::Screen(s.id), s.output_key()?.clone())))
            .collect();
        // Other real displays (the laptop panel next to a mirrored external monitor,
        // say) go on the bottom row too, so nothing overlaps the Prostir monitors.
        let mut others: Vec<&OutputKey> = geometries
            .iter()
            .filter(|(name, g)| {
                g.enabled && g.w > 0 && !names.values().any(|n| n == *name) && !g.is_virtual
            })
            .map(|(name, _)| name)
            .collect();
        others.sort();
        for (i, name) in others.into_iter().enumerate() {
            names.insert(Out::Other(i), name.clone());
        }
        // Only outputs the desktop lists: the others once it does.
        let size = |out: Out| {
            let g = geometries.get(names.get(&out)?)?;
            Some(desk::Size { w: g.w, h: g.h })
        };
        self.layout_waits = self
            .screens
            .values()
            .any(|s| s.output_key().is_some_and(|n| !geometries.contains_key(n)));
        let rows = self.effective_rows();
        // Displays of the PC shown here: the laptop's panel first, under the primary
        // monitor, unless the headset placed them.
        let mut streaming_physical: Vec<(bool, Out)> = self
            .physical()
            .filter(|s| s.output.is_some())
            .map(|s| {
                (
                    !matches!(s.kind, ScreenKind::Physical { builtin: true, .. }),
                    Out::Screen(s.id),
                )
            })
            .collect();
        streaming_physical.sort();
        let streaming_physical: Vec<Out> = streaming_physical.into_iter().map(|(_, o)| o).collect();
        let row_ids: Vec<Vec<Out>> = rows
            .iter()
            .map(|r| r.iter().map(|e| Out::Screen(e.id)).collect())
            .collect();
        // Where the headset has each monitor: the boxes of its own plan, in the pixels
        // of the streams (`SetLayout`, see `PROTOCOL.md`).
        let boxes = desk::Boxes::new(
            rows.iter()
                .flatten()
                .filter_map(|e| {
                    let out = Out::Screen(e.id);
                    let on_desk = size(out)?;
                    let (w, h) = self.stream_size_of(e.id).unwrap_or((on_desk.w, on_desk.h));
                    let seen = desk::SeenBox::from_wire(e.center?, e.middle?, desk::Size { w, h });
                    Some((out, seen, on_desk))
                })
                .collect(),
        );
        let fresh = desk::plan(
            &desk::Desk {
                rows: row_ids.clone(),
                main: self
                    .primary
                    .or_else(|| self.order().first().copied())
                    .map(Out::Screen),
                under_main: streaming_physical.first().copied(),
                bottom: streaming_physical
                    .iter()
                    .skip(1)
                    .copied()
                    .chain(names.keys().filter(|o| matches!(o, Out::Other(_))).copied())
                    .collect(),
                column: self.windows().map(|s| Out::Screen(s.id)).collect(),
            },
            size,
            &boxes,
        );
        // The headset's own plan comes back as it is (planning a plan gives the plan)
        // and is taken, so the desktop is what its map shows. Anything worked out here
        // (outputs scaled, monitors the headset has not placed) replaces the layout the
        // desktop has, in the same rows, only when better by more than a hand's tremor.
        let plan = if is_the_headsets(&fresh, &boxes) || self.applied.0 != row_ids {
            fresh
        } else {
            desk::settled(&self.applied.1, &fresh, &boxes)
        };
        // Every applied change makes KWin reconfigure its outputs, which also cuts the
        // laptop mirror's stream: `place` only sends what actually differs.
        let mut positions: Vec<(OutputKey, i32, i32)> = plan
            .iter()
            .map(|p| (names[&p.id].clone(), p.x, p.y))
            .collect();
        // A display of the PC is the primary one under its connector, shown or not.
        let primary = self
            .primary
            .and_then(|p| self.screens.get(&p))
            .filter(|_| self.manage_primary)
            .and_then(|s| match &s.kind {
                ScreenKind::Physical { key, .. } => Some(OutputKey::new(key.connector.as_str())),
                _ => s.output_key().cloned(),
            });
        // A desktop whose primary output is always at (0, 0) moves with it: the whole
        // plan shifts so that it is, the same for the desktop and the headset's map.
        let (dx, dy) = if self.desktop.caps().origin_is_primary {
            let main = primary.clone().or_else(|| {
                geometries
                    .iter()
                    .find(|(_, g)| g.priority == Some(1))
                    .map(|(key, _)| key.clone())
            });
            origin_at(&positions, main.as_ref())
        } else {
            (0, 0)
        };
        for (_, x, y) in &mut positions {
            (*x, *y) = (*x - dx, *y - dy);
        }
        self.desktop
            .place(&DeskPlan { positions, primary }, &geometries)?;
        self.on_desk = plan
            .iter()
            .map(|p| {
                let rect = state::DeskRect {
                    x: p.x - dx,
                    y: p.y - dy,
                    w: p.w.max(0) as u32,
                    h: p.h.max(0) as u32,
                };
                (names[&p.id].clone(), rect)
            })
            .collect();
        self.applied = (row_ids, plan);
        Ok(())
    }

    /// The size of the stream of screen `id`: as its encoder says, else as the
    /// monitor was made.
    fn stream_size_of(&self, id: ScreenId) -> Option<(i32, i32)> {
        let (w, h) = self.router.stream_size(id).or_else(|| {
            let spec = self.screens.get(&id)?.kind.spec()?;
            Some((spec.width, spec.height))
        })?;
        Some((i32::try_from(w).ok()?, i32::try_from(h).ok()?))
    }

    /// Makes the monitors match `specs` left to right (a headset profile), and gives
    /// their numbers in that order. Monitors of a matching size are kept, so their
    /// windows stay where they are.
    pub fn set_monitors(
        &mut self,
        specs: &[MonitorSpec],
        primary: Option<usize>,
        new_id: &mut dyn FnMut() -> ScreenId,
    ) -> Result<Vec<ScreenId>> {
        if specs.is_empty() || specs.len() > MONITORS_NO_LIMITS {
            bail!("1 to {MONITORS_NO_LIMITS} monitors");
        }
        for s in specs {
            s.validate()?;
        }
        let mut unused: Vec<ScreenId> = self.order();
        let mut plan: Vec<Option<ScreenId>> = specs
            .iter()
            .map(|s| {
                let i = unused.iter().position(|id| {
                    self.screens
                        .get(id)
                        .and_then(|m| m.kind.spec())
                        .is_some_and(|m| m == s)
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
        for (slot, spec) in plan.iter_mut().zip(specs) {
            if slot.is_none() {
                let id = new_id();
                self.add(id, spec.clone(), usize::MAX)?;
                *slot = Some(id);
            }
        }
        if let Some(id) = last {
            self.remove(id)?;
        }
        let ids: Vec<ScreenId> = plan.into_iter().flatten().collect();
        self.set_order(&ids)?;
        if let Some(&p) = primary.and_then(|i| ids.get(i)) {
            self.set_primary(p)?;
        }
        Ok(ids)
    }

    /// The headset's rows, kept in step with the monitors (see [`rows_in_step`]), with
    /// the displays of the PC where the headset put them, while they stream.
    fn effective_rows(&self) -> Vec<Vec<desk::Entry<ScreenId>>> {
        self.rows_with(|s| s.is_physical() && s.output.is_some())
    }

    /// The rows with every monitor, and the other screens `kept` says.
    fn rows_with(&self, kept: impl Fn(&Screen) -> bool) -> Vec<Vec<desk::Entry<ScreenId>>> {
        let monitors: Vec<ScreenId> = self.monitors().map(|s| s.id).collect();
        rows_in_step(
            &self.rows,
            &monitors,
            |id| self.is_monitor(id),
            |id| self.screens.get(&id).is_some_and(&kept),
        )
    }

    /// Arranges the desktop's outputs in the rows the headset shows them in.
    pub fn set_layout(&mut self, rows: &[state::Row]) {
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
        // A laptop screen's stream is cut for a moment whenever KDE rearranges its
        // outputs, and a headset that sends a layout meanwhile leaves it out; a display
        // of the PC that is not connected is not in the headset's rows either. Their
        // spots are kept, or they would come back elsewhere, the headset would move
        // them back, and that would cut them again, round and round.
        let resting: Vec<ScreenId> = self
            .physical()
            .filter(|s| s.output.is_none())
            .map(|s| s.id)
            .collect();
        for id in resting {
            if rows.iter().flatten().any(|e| e.id == id) {
                continue;
            }
            for (i, old) in self.rows.iter().enumerate() {
                let Some(spot) = old.iter().find(|e| e.id == id) else {
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
        // Monitors the headset left out stay, at the end of the bottom row, in the order
        // they had.
        let missing: Vec<desk::Entry<ScreenId>> = self
            .order()
            .into_iter()
            .filter(|id| !rows.iter().flatten().any(|e| e.id == *id))
            .map(desk::Entry::new)
            .collect();
        if !missing.is_empty() {
            match rows.last_mut() {
                Some(row) => row.extend(missing),
                None => rows.push(missing),
            }
        }
        self.rows = rows;
        self.layout_changed();
    }

    fn set_order(&mut self, ids: &[ScreenId]) -> Result<()> {
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
        // One row, in this order.
        self.rows = vec![ids.iter().map(|&id| desk::Entry::new(id)).collect()];
        self.layout_changed();
        Ok(())
    }

    /// Without a chosen primary, the middle monitor gets the Plasma panel.
    fn ensure_primary(&mut self) {
        if self
            .primary
            .is_some_and(|p| self.is_monitor(p) || self.is_physical(p))
        {
            return;
        }
        let ids = self.order();
        if let Some(&id) = ids.get(ids.len().saturating_sub(1) / 2) {
            let _ = self.set_primary(id);
        } else {
            self.layout_changed();
        }
    }

    pub fn primary(&self) -> Option<ScreenId> {
        self.primary
    }

    /// Gives every screen the wallpaper of the primary desktop (Plasma configures
    /// wallpapers per screen, so new monitors would otherwise show the default one).
    pub fn sync_wallpapers(&mut self) {
        self.desktop.sync_wallpapers();
    }

    fn encoders(&self) -> impl Iterator<Item = (ScreenId, &Encoder)> {
        self.screens
            .values()
            .filter_map(|s| Some((s.id, s.stream.as_ref()?)))
    }

    /// Puts the PC's active window on a panel of its own, numbered `id` (a number never
    /// given before): an output its size, which the headset shows as a floating panel.
    pub fn detach_window(&mut self, id: ScreenId) -> Result<()> {
        if self.windows().count() >= WINDOWS_MAX {
            bail!(UserError::Limit {
                max: WINDOWS_MAX,
                of: Counted::Windows
            });
        }
        let Some(window) = self.desktop.windows().and_then(|w| w.active()) else {
            bail!(UserError::NoWindow);
        };
        // Sized and planned with the scale its output gets, in whole percent.
        let scale = crate::platform::whole_percent(window.scale);
        let (width, height) = panel_size(&ActiveWindow {
            scale,
            ..window.clone()
        });
        let spec = MonitorSpec {
            width,
            height,
            scale,
        };
        let number = self.free_number();
        let kind = ScreenKind::Window {
            origin: window.clone(),
            spec: spec.clone(),
        };
        // Listed before its stream starts, so the stream knows its name.
        self.screens.insert(id, Screen::starting(id, number, kind));
        let started = self.make(id, number, &spec).and_then(|mut output| {
            let encoder = self.start_stream(id, output.as_mut())?;
            Ok((output, encoder))
        });
        let (output, encoder) = match started {
            Ok(started) => started,
            Err(e) => {
                self.screens.remove(&id);
                return Err(e);
            }
        };
        let name = output.key().clone();
        if let Some(s) = self.screens.get_mut(&id) {
            s.output = Some(output);
            s.stream = Some(encoder);
            s.state = ScreenState::Running;
        }
        self.layout_changed();
        let moved = match self.desktop.windows() {
            Some(w) => w.move_to(&window, &name, true),
            None => Err(anyhow::anyhow!("the desktop has no windows")),
        };
        if let Err(e) = moved {
            self.close_window_panel(id);
            return Err(e.context(UserError::Desktop));
        }
        if !self.streaming
            && let Some(e) = self.screens.get(&id).and_then(|s| s.stream.as_ref())
        {
            e.set_paused(true);
        }
        self.windows_checked = Instant::now() + Duration::from_secs(1);
        info!("window panel {id} on {name} ({width}x{height})");
        Ok(())
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
        if let Some(windows) = self.desktop.windows() {
            if let Some(key) = screen.output_key() {
                windows.forget_panel(key);
            }
            if put_back
                && let ScreenKind::Window { origin, .. } = &screen.kind
                && let Some(on) = &origin.output
                && let Err(e) = windows.put_back(origin, on)
            {
                warn!("cannot put the window back on {on}: {e:#}");
            }
        }
        self.router.remove_stream(id);
        drop(screen);
        self.layout_changed();
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
        let names: Vec<OutputKey> = self
            .windows()
            .filter_map(|s| s.output_key().cloned())
            .collect();
        let vacated = match self.desktop.windows() {
            Some(w) => w.vacated(&names),
            None => Vec::new(),
        };
        for name in vacated {
            if let Some(id) = self.id_of_output(&name) {
                info!("the window on panel {id} is gone: closing the panel");
                self.close_panel(id, false);
            }
        }
    }

    /// Moves the PC's active window to monitor `id` (the one the user looks at).
    pub fn window_here(&mut self, id: ScreenId) -> Result<()> {
        let Some(output) = self
            .screens
            .get(&id)
            .filter(|s| s.is_virtual() || (s.is_physical() && s.output.is_some()))
            .and_then(Screen::output_key)
            .cloned()
        else {
            bail!(UserError::UnknownDisplay);
        };
        let Some(windows) = self.desktop.windows() else {
            bail!(UserError::NoWindow);
        };
        let Some(window) = windows.active() else {
            bail!(UserError::NoWindow);
        };
        windows
            .move_to(&window, &output, false)
            .context(UserError::Desktop)
    }

    /// Saves a picture of monitor `id` (all of them where only that is possible).
    pub fn screenshot(&mut self, id: ScreenId) -> Result<std::path::PathBuf> {
        let screen = self
            .screens
            .get(&id)
            .filter(|s| s.is_virtual() || s.is_window());
        if screen.is_none() {
            bail!(UserError::UnknownDisplay);
        }
        // Where the desktop's screenshot tool cannot take one output by name, the
        // monitor's own stream can. A fresh keyframe, decoded into a PNG; the whole
        // desktop if not.
        if !self.desktop.caps().screenshot_one_output
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
        let key = screen.and_then(Screen::output_key).cloned();
        let name = self.desktop.caps().name;
        let Some(windows) = self.desktop.windows() else {
            return Err(anyhow::anyhow!("the {name} desktop takes no screenshots"))
                .context(UserError::Capture);
        };
        windows.screenshot(key.as_ref()).context(UserError::Capture)
    }

    /// A fresh picture of screen `id` as a keyframe, though nothing on it moves: the
    /// headset missed what it sent before.
    ///
    /// A stream whose picture moves and that goes to another headset already sends a
    /// keyframe with its next frame, so that headset sees no hiccup; any other starts
    /// over to send its picture (and should a moving one stop, `tick` starts it over all the same, see
    /// [`Router::starved`]).
    pub fn fresh_picture(&self, id: ScreenId) {
        if !self.streaming {
            return;
        }
        let Some(e) = self.screens.get(&id).and_then(|s| s.stream.as_ref()) else {
            return;
        };
        if self.router.live(id) {
            e.force_keyframe();
        } else {
            e.set_paused(false);
        }
    }

    /// A headset asks for a keyframe of screen `id`: at most every 300 ms for each
    /// screen, whoever asks, or keyframes would follow one another.
    pub fn keyframe_asked(&mut self, id: ScreenId) {
        let now = Instant::now();
        if self
            .keyframes_asked
            .get(&id)
            .is_some_and(|t| now.duration_since(*t) < Duration::from_millis(300))
        {
            return;
        }
        self.keyframes_asked.insert(id, now);
        self.force_keyframe(id);
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

    /// Every running monitor streams, and one at least does. Otherwise the screens are
    /// made anew for the next session.
    pub fn healthy(&self) -> bool {
        // Monitors too big for this headset are not broken: they wait for another.
        self.monitors()
            .any(|s| s.running() || self.too_big.contains_key(&s.id))
            && !self.router.failed.load(Ordering::Relaxed)
            && !self
                .screens
                .values()
                .filter(|s| s.running() && !s.is_physical())
                .any(|s| s.output.as_ref().is_none_or(|o| closed(o.as_ref())))
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
        let physical = self
            .physical()
            .filter_map(|s| match &s.kind {
                ScreenKind::Physical {
                    key,
                    builtin,
                    shown,
                    ..
                } => Some((
                    s.id,
                    Physical {
                        key: key.clone(),
                        builtin: *builtin,
                        shown: *shown,
                        seen: self.physical_seen.get(&s.id).copied().unwrap_or(0),
                    },
                )),
                _ => None,
            })
            .collect();
        Some(SavedScreens {
            specs,
            physical,
            // The PC's displays keep their places while not connected or not shown.
            rows: self.rows_with(Screen::is_physical),
            primary: self.primary,
        })
    }
}

/// Streams at once at most: a lane each (`2..32`).
pub const STREAMS_MAX: u8 = 30;

/// How many streams a headset that can do what `headset` says takes at once.
pub fn streams_max(headset: &state::HeadsetCaps) -> usize {
    usize::from(match headset.streams_max {
        0 => STREAMS_MAX,
        n => n.min(STREAMS_MAX),
    })
}

/// Why display `name` does not stream to a headset: its picture of `w`x`h` is larger
/// than the headset decodes in `codec`.
fn too_big_why(name: &str, (w, h): (u32, u32), codec: Codec) -> String {
    format!(
        "{name}: {w}x{h} {} {codec:?}",
        t(
            "більше, ніж шолом декодує в",
            "is larger than the headset decodes in"
        )
    )
}

/// The first time in a run that a screen sharing program resizes a monitor (output
/// `Prostir-number`, as the desktop names it): says the size is back, and how to
/// share without the jumps.
fn show_sharing_tip(number: u32) {
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if SHOWN.swap(true, Ordering::Relaxed) {
        return;
    }
    let body = format!(
        "{} {}. {}",
        t(
            "Програма, що показує екран, змінила розмір монітора",
            "A screen sharing program changed the size of monitor"
        ),
        number,
        t(
            "Розмір повернено, але показ може стрибати. Щоб цього не було, показуйте вікно програми, а не монітор.",
            "Its size is back, but the share may jump. To avoid that, share an app window instead of the monitor."
        )
    );
    dialogs::notify(t("Показ екрана", "Screen sharing"), &body);
}

/// How far a plan moves so that output `main` is at (0, 0): where `positions` put it
/// (nothing, without it).
fn origin_at(positions: &[(OutputKey, i32, i32)], main: Option<&OutputKey>) -> (i32, i32) {
    main.and_then(|m| positions.iter().find(|(key, _, _)| key == m))
        .map_or((0, 0), |(_, x, y)| (*x, *y))
}

/// Whether the desktop closed `output`. One paused for a while is not closed: it comes
/// back by itself.
fn closed(output: &dyn Output) -> bool {
    output.health() == OutputHealth::Closed
}

/// The pixel size another program gave `output`, when that is not the size it was made
/// with.
fn resized(output: &dyn Output) -> Option<(u32, u32)> {
    match output.health() {
        OutputHealth::Resized { w, h } => Some((w, h)),
        _ => None,
    }
}

/// Which known display of the PC (`known`: number and how it is known) each connected
/// one (`seen`) is: by its EDID where it says one that no other connected display says
/// too; else by its connector, among those known by their connector. Another display
/// in the same connector is another display. `None`: seen for the first time.
fn match_physical(
    known: &[(ScreenId, PhysicalKey)],
    seen: &[PhysicalDisplay],
) -> Vec<Option<ScreenId>> {
    let mut taken: Vec<ScreenId> = Vec::new();
    seen.iter()
        .map(|d| {
            let unique = unique_edid(d, seen);
            let free = |id: &ScreenId| !taken.contains(id);
            // By its EDID; else by its connector, one known without an EDID (which
            // may say its EDID only now).
            let found = known
                .iter()
                .find(|(id, key)| free(id) && unique.is_some() && key.edid.as_deref() == unique)
                .or_else(|| {
                    known.iter().find(|(id, key)| {
                        free(id) && key.edid.is_none() && key.connector == d.key.as_str()
                    })
                });
            let id = found.map(|(id, _)| *id);
            taken.extend(id);
            id
        })
        .collect()
}

/// The EDID display `d` says, where no other connected display says the same.
fn unique_edid<'a>(d: &'a PhysicalDisplay, seen: &[PhysicalDisplay]) -> Option<&'a str> {
    d.edid.as_deref().filter(|e| {
        seen.iter()
            .filter(|o| o.edid.as_deref() == Some(*e))
            .count()
            == 1
    })
}

/// Of the displays not connected (`absent`, each with when it was last seen), those
/// beyond the `keep` most recently seen.
fn least_recently_seen(mut absent: Vec<(ScreenId, u64)>, keep: usize) -> Vec<ScreenId> {
    // The most recent first; of the same moment, the newer number.
    absent.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    absent.into_iter().skip(keep).map(|(id, _)| id).collect()
}

/// `Displays` holds at most `MAX_DISPLAYS`: beyond that, displays of the PC that are
/// not connected go first (from the end), then the last ones, with a warning. Never
/// more than that, so the part always goes out.
fn within_limit(mut displays: Vec<state::Display>) -> Vec<state::Display> {
    let max = state::MAX_DISPLAYS;
    if displays.len() <= max {
        return displays;
    }
    warn!(
        "{} displays, the headset is told about {max}: absent ones of the PC left out first",
        displays.len()
    );
    while displays.len() > max {
        match displays
            .iter()
            .rposition(|d| matches!(d.state, state::DisplayState::Absent))
        {
            Some(i) => {
                displays.remove(i);
            }
            None => displays.truncate(max),
        }
    }
    displays
}

/// An output on the desktop: a screen, or another real display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Out {
    Screen(ScreenId),
    Other(usize),
}

/// Whether `plan` is the headset's plan as `boxes` hold it: every output with a box
/// where the box is, give or take one shift of the whole.
fn is_the_headsets<K: Copy + Ord>(plan: &[desk::Spot<K>], boxes: &desk::Boxes<K>) -> bool {
    use desk::View;
    let mut shift = None;
    let mut any = false;
    for s in plan {
        if !boxes.boxes.iter().any(|b| b.0 == s.id) {
            continue;
        }
        let Some((x, y)) = boxes.exact(s.id, desk::Size { w: s.w, h: s.h }) else {
            return false;
        };
        if *shift.get_or_insert((s.x - x, s.y - y)) != (s.x - x, s.y - y) {
            return false;
        }
        any = true;
    }
    any
}

/// `rows` kept in step with the outputs: only monitors (`monitor`) and other outputs
/// that show (`shown`) stay, each in the first place it is listed in, and any of
/// `monitors` left out go to the end of the bottom row. So every monitor is listed
/// exactly once.
fn rows_in_step<K: Copy + PartialEq>(
    rows: &[Vec<desk::Entry<K>>],
    monitors: &[K],
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
    for &id in monitors {
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

    /// The streams go in the codec chosen only where every headset decodes it, and in
    /// H.264 otherwise; without a headset, as chosen.
    #[test]
    fn the_codec_is_one_every_headset_decodes() {
        let decoder = |codec| state::Decoder {
            codec,
            max_w: 4096,
            max_h: 4096,
        };
        let both = state::HeadsetCaps {
            decoders: vec![decoder(0), decoder(1)],
            ..state::HeadsetCaps::default()
        };
        let h264 = state::HeadsetCaps {
            decoders: vec![decoder(0)],
            ..state::HeadsetCaps::default()
        };
        assert_eq!(Displays::codec_for(Codec::H265, &[]), Codec::H265);
        assert_eq!(
            Displays::codec_for(Codec::H265, std::slice::from_ref(&both)),
            Codec::H265
        );
        assert_eq!(
            Displays::codec_for(Codec::H265, &[both.clone(), h264.clone()]),
            Codec::H264
        );
        assert_eq!(Displays::codec_for(Codec::H264, &[both, h264]), Codec::H264);
    }

    /// On a desktop whose primary output is always at (0, 0), the plan moves with it.
    #[test]
    fn a_plan_moves_with_a_primary_at_the_origin() {
        let key = OutputKey::new;
        let positions = [
            (key("a"), -1920, 0),
            (key("b"), 0, 0),
            (key("c"), 640, 1080),
        ];
        assert_eq!(origin_at(&positions, Some(&key("c"))), (640, 1080));
        assert_eq!(origin_at(&positions, Some(&key("x"))), (0, 0));
        assert_eq!(origin_at(&positions, None), (0, 0));
    }

    /// A display of the PC is known by its EDID wherever it is plugged in; one that says
    /// nothing, or the same as another, by its connector; another display in the same
    /// connector is another display.
    #[test]
    fn displays_of_the_pc_are_known_by_edid_else_by_connector() {
        let id = ScreenId::saved;
        let display = |connector: &str, edid: Option<&str>| PhysicalDisplay {
            key: OutputKey::new(connector),
            label: connector.into(),
            edid: edid.map(Into::into),
            builtin: false,
            size: None,
            default: false,
        };
        let key = |edid: Option<&str>, connector: &str| PhysicalKey {
            edid: edid.map(Into::into),
            connector: connector.into(),
        };
        let known = [
            (id(100), key(Some("BOE 1"), "eDP-1")),
            (id(101), key(Some("DEL 7"), "HDMI-A-1")),
            (id(102), key(None, "DP-2")),
        ];
        // The Dell moved to DP-1; a display without EDID on DP-2; another one in
        // HDMI-A-1.
        let seen = [
            display("eDP-1", Some("BOE 1")),
            display("DP-1", Some("DEL 7")),
            display("DP-2", None),
            display("HDMI-A-1", Some("LG 3")),
        ];
        assert_eq!(
            match_physical(&known, &seen),
            [Some(id(100)), Some(id(101)), Some(id(102)), None]
        );
        // Two of the same model and serial: by connector, and none of them is the one
        // known by that EDID.
        let twins = [
            display("DP-1", Some("DEL 7")),
            display("DP-2", Some("DEL 7")),
        ];
        assert_eq!(match_physical(&known, &twins), [None, Some(id(102))]);
        // Each known display matches once.
        let two_on_dp2 = [display("DP-2", None), display("DP-2", None)];
        assert_eq!(match_physical(&known, &two_on_dp2), [Some(id(102)), None]);
        // The one known by its connector says its EDID now: still that one (and
        // `sync_physical` keeps the EDID from now on).
        let says = [display("DP-2", Some("SAM 9"))];
        assert_eq!(match_physical(&known, &says), [Some(id(102))]);
        assert_eq!(unique_edid(&says[0], &says), Some("SAM 9"));
    }

    /// Of the displays of the PC not connected, the 8 seen last stay; the others
    /// (the oldest, and those never dated) are forgotten.
    #[test]
    fn displays_of_the_pc_long_absent_are_forgotten() {
        let id = ScreenId::saved;
        let absent: Vec<(ScreenId, u64)> =
            (0..12).map(|n| (id(100 + n), 1000 + n as u64)).collect();
        let mut gone = least_recently_seen(absent.clone(), ABSENT_KEPT);
        gone.sort();
        assert_eq!(gone, [id(100), id(101), id(102), id(103)]);
        assert!(least_recently_seen(absent[..8].to_vec(), ABSENT_KEPT).is_empty());
        let undated = vec![(id(1), 0), (id(2), 5)];
        assert_eq!(least_recently_seen(undated, 1), [id(1)]);
    }

    /// `Displays` never holds more than it may: absent displays of the PC are left out
    /// first, so the part always goes out (it failed to encode before, ending every
    /// session).
    #[test]
    fn the_list_of_displays_always_fits() {
        use prostir_proto::state::{Display, DisplayKind, DisplayState, Mode};
        let display = |n: u32, state: DisplayState| Display {
            id: state::DisplayId(n),
            kind: DisplayKind::Virtual,
            name: format!("Prostir-{n}"),
            mode: Mode::new(640, 360),
            desk: None,
            state,
        };
        let mut list: Vec<Display> = (0..10).map(|n| display(n, DisplayState::Running)).collect();
        list.extend((10..40).map(|n| display(n, DisplayState::Absent)));
        let kept = within_limit(list);
        assert_eq!(kept.len(), state::MAX_DISPLAYS);
        assert_eq!(
            kept.iter()
                .filter(|d| d.state == DisplayState::Running)
                .count(),
            10,
            "every running one stays"
        );
        let part = state::Displays {
            displays: kept,
            primary: None,
        };
        assert!(prostir_proto::wire::encode(&part).is_ok());
        let many: Vec<Display> = (0..40).map(|n| display(n, DisplayState::Running)).collect();
        assert_eq!(within_limit(many).len(), state::MAX_DISPLAYS);
    }

    /// The headset's plan read back from `SetLayout` is recognised as its plan,
    /// shifted as a whole too; a plan that moved one monitor is not.
    #[test]
    fn the_headsets_plan_is_recognised() {
        let spot = |id, x, y| desk::Spot {
            id,
            x,
            y,
            w: 640,
            h: 360,
        };
        let headset = [spot(0u8, 0, 360), spot(1, 640, 300)];
        let boxes = desk::Boxes::new(
            headset
                .iter()
                .map(|s| {
                    let (c, m) = s.wire();
                    let size = desk::Size { w: s.w, h: s.h };
                    (s.id, desk::SeenBox::from_wire(c, m, size), size)
                })
                .collect(),
        );
        assert!(is_the_headsets(&headset, &boxes));
        let shifted = [spot(0, 0, 60), spot(1, 640, 0)];
        assert!(is_the_headsets(&shifted, &boxes));
        let moved = [spot(0, 0, 360), spot(1, 640, 0)];
        assert!(!is_the_headsets(&moved, &boxes));
        assert!(!is_the_headsets(&moved, &desk::Boxes::new(Vec::new())));
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
