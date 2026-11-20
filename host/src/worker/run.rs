//! The worker's thread: it owns the desktop, the outputs and their encoders, and
//! brings them to the wanted state one step at a time (see `mod.rs`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use log::{error, info, warn};
use prostir_layout::desk;
use prostir_proto::state::{self, ErrorCode};
use prostir_proto::{Codec, HostMsg};

use super::inbox::Inbox;
use super::watch::Steps;
use super::{
    Connected, Done, Encoding, Gone, Job, JobId, JobOk, Primary, Start, Wanted, What, timed,
};
use crate::encoder::{
    Backend, Choice, Encoder, EncoderConfig, GpuDevices, Kind, Next, NotOpened, Wanted as Asked,
};
use crate::errors::{UserError, code_of};
use crate::events::{Event, EventSink};
use crate::i18n::t;
use crate::layout::panel_size;
use crate::platform::{
    ActiveWindow, DeskPlan, Desktop, Geometry, MonitorSpec, Output, OutputHealth, OutputKey,
    VirtualSpec, dialogs,
};
use crate::router::Router;
use crate::screen::{ScreenId, SizeAction, SizeGuard};

/// How long an encoder may take to start, and to stop, before the worker goes on
/// without it.
const ENCODER_START: Duration = Duration::from_secs(10);
const ENCODER_STOP: Duration = Duration::from_secs(5);
/// How long a display of the PC whose stream stopped (or did not start) rests before
/// it is shown again.
const PHYSICAL_REST: Duration = Duration::from_secs(2);
/// How often the PC's own displays are looked at, while live.
const PHYSICAL_EVERY: Duration = Duration::from_secs(2);
/// How often the desktop's primary output is compared with ours.
const PRIMARY_EVERY: Duration = Duration::from_secs(2);
/// How often window panels are checked for a window that left.
const PANELS_EVERY: Duration = Duration::from_secs(1);
/// How often a layout waiting for the desktop to list new outputs looks again.
const LISTING_LOOK: Duration = Duration::from_millis(500);

/// What the control loop and the worker share.
pub struct Shared {
    pub inbox: Arc<Inbox<Wanted, Job>>,
    pub steps: Arc<Steps>,
    pub router: Arc<Router>,
    pub events: EventSink,
    /// Which worker this is (see [`super::Desk::generation`]).
    pub generation: u64,
}

/// Starts the worker's thread: once the worker `before` is gone, it connects and works
/// until asked to end; then `gone` is set.
pub fn spawn(shared: Shared, start: Start, before: Option<Gone>, gone: Gone) {
    let mark_gone = {
        let gone = gone.clone();
        move || {
            let (lock, ring) = &*gone;
            *lock.lock().unwrap() = true;
            ring.notify_all();
        }
    };
    let spawned = std::thread::Builder::new()
        .name("desk".into())
        .spawn(move || {
            let (events, generation) = (shared.events.clone(), shared.generation);
            if let Some(before) = before {
                // A step like any other: a worker before that hangs shows here too.
                shared
                    .steps
                    .begin("waiting for the last desktop worker to finish");
                let (lock, ring) = &*before;
                let _done = ring.wait_while(lock.lock().unwrap(), |done| !*done);
                shared.steps.end();
            }
            if let Some(worker) = Worker::connect(shared, &start) {
                worker.run();
            }
            mark_gone();
            events.send(Event::Desk(generation, Done::Gone));
        });
    if let Err(e) = spawned {
        error!("no thread for the desktop: {e}");
        let (lock, ring) = &*gone;
        *lock.lock().unwrap() = true;
        ring.notify_all();
    }
}

/// What an output was made as.
#[derive(Debug, Clone)]
enum Made {
    Monitor {
        spec: MonitorSpec,
        number: u32,
    },
    Physical {
        key: OutputKey,
        number: u32,
    },
    Panel {
        origin: ActiveWindow,
        spec: MonitorSpec,
        number: u32,
    },
}

impl Made {
    /// Whether it is what `what` asks for.
    fn is(&self, what: &What) -> bool {
        match (self, what) {
            (Made::Monitor { spec, number }, What::Monitor { spec: s, number: n }) => {
                spec == s && number == n
            }
            (Made::Physical { key, number }, What::Physical { key: k, number: n }) => {
                key == k && number == n
            }
            (Made::Panel { number, .. }, What::Panel { number: n, .. }) => number == n,
            _ => false,
        }
    }

    fn spec(&self) -> Option<&MonitorSpec> {
        match self {
            Made::Monitor { spec, .. } | Made::Panel { spec, .. } => Some(spec),
            Made::Physical { .. } => None,
        }
    }
}

/// An encoder of the worker's, and the gate its frames and events go through: shut as
/// it is let go, so one that takes long to stop (or to start, and is left behind)
/// sends nothing more, and a headset never gets frames of two encoders for one stream.
struct Running {
    encoder: Encoder,
    retired: Arc<AtomicBool>,
}

impl std::ops::Deref for Running {
    type Target = Encoder;
    fn deref(&self) -> &Encoder {
        &self.encoder
    }
}

/// A screen's output and encoder.
struct Slot {
    made: Made,
    name: String,
    // Field order matters: the encoder stops before the output goes away.
    encoder: Option<Running>,
    output: Option<Box<dyn Output>>,
    /// The encoder and codec wanted when its encoder started.
    started_for: Option<(Backend, Codec)>,
    /// The attempt it failed at (tried again only at another).
    failed: Option<u32>,
    /// A display of the PC is shown again from then.
    rest_until: Option<Instant>,
    size_guard: SizeGuard,
    /// The desktop closing it was said.
    closed_told: bool,
}

impl Slot {
    fn new(made: Made, name: String) -> Self {
        Self {
            made,
            name,
            encoder: None,
            output: None,
            started_for: None,
            failed: None,
            rest_until: None,
            size_guard: SizeGuard::default(),
            closed_told: false,
        }
    }

    fn key(&self) -> Option<&OutputKey> {
        self.output.as_ref().map(|o| o.key())
    }

    fn is_physical(&self) -> bool {
        matches!(self.made, Made::Physical { .. })
    }

    fn is_panel(&self) -> bool {
        matches!(self.made, Made::Panel { .. })
    }
}

/// One step towards the wanted state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The screen is not wanted, or as something else: its output goes.
    Remove(ScreenId),
    /// It streams no more (too big for every headset): the encoder stops.
    StopStream(ScreenId),
    Make(ScreenId),
    Start(ScreenId),
    /// Another encoder or codec is wanted.
    Restart(ScreenId),
    Pause(ScreenId, bool),
    /// The active window onto a new panel, for job `JobId`.
    Detach(ScreenId, JobId),
}

/// An output on the desktop: a screen, or another real display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Out {
    Screen(ScreenId),
    Other(usize),
}

pub struct Worker {
    // Field order matters: the outputs go before the desktop connection.
    slots: BTreeMap<ScreenId, Slot>,
    desktop: Box<dyn Desktop>,
    /// One graphics device per card for every encoder (and capture) of the desktop.
    gpu: GpuDevices,
    shared: Shared,
    /// The wanted state worked on; none until the first comes.
    wanted: Option<Wanted>,
    /// The bitrate and the text preset the encoders have.
    tuning: Option<(u32, bool)>,
    /// The primary output as last made primary.
    primary_made: Option<OutputKey>,
    /// A monitor or panel was made: the others' wallpaper goes to it.
    wallpapers_due: bool,
    /// How many things handed over the worker took so far.
    taken: u64,
    /// Panels it made (or tried to) for these jobs: each is made once.
    detached: std::collections::BTreeSet<JobId>,
    /// The desktop is to be laid out again.
    layout_due: bool,
    /// The rows and the layout the desktop took last.
    applied: (Vec<Vec<Out>>, Vec<desk::Spot<Out>>),
    /// Laid out without an output the desktop did not list yet: again once it does.
    layout_waits: bool,
    /// After the desktop refused a layout: when to try again, and how many times it did.
    layout_retry: (Instant, u32),
    primary_checked: Instant,
    panels_checked: Instant,
    physical_due: Instant,
}

impl Worker {
    /// Connects to the desktop and picks how to encode; says what came of it.
    fn connect(shared: Shared, start: &Start) -> Option<Self> {
        shared.steps.begin("connecting to the desktop");
        let connected = (|| -> Result<_> {
            let gpu = GpuDevices::default();
            let mut desktop = crate::platform::connect(&gpu)?;
            let (backend, codec) = crate::encoder::pick(start.codec, start.software)?;
            info!("encoding with {backend:?} {codec:?}");
            let caps = desktop.caps().clone();
            let scales = start
                .sizes
                .iter()
                .map(|&(w, h)| ((w, h), desktop.scales_for(w, h)))
                .collect();
            let windows = desktop.windows();
            let has_windows = windows.is_some();
            let window_list = windows.map(|w| Arc::downgrade(&w.list()));
            let connected = Connected {
                caps,
                scales,
                window_list,
                has_windows,
                backend,
                codec,
                choices: crate::encoder::available(),
                physical: desktop.physical(),
            };
            Ok((desktop, gpu, connected))
        })();
        shared.steps.end();
        let (desktop, gpu, connected) = match connected {
            Ok(c) => c,
            Err(e) => {
                shared
                    .events
                    .send(Event::Desk(shared.generation, Done::NoDesktop(e)));
                return None;
            }
        };
        shared.events.send(Event::Desk(
            shared.generation,
            Done::Connected(Box::new(connected)),
        ));
        Some(Self::with(shared, desktop, gpu))
    }

    /// A worker of `desktop`.
    fn with(shared: Shared, desktop: Box<dyn Desktop>, gpu: GpuDevices) -> Self {
        let now = Instant::now();
        Self {
            slots: BTreeMap::new(),
            desktop,
            gpu,
            shared,
            wanted: None,
            tuning: None,
            primary_made: None,
            wallpapers_due: false,
            taken: 0,
            detached: std::collections::BTreeSet::new(),
            layout_due: false,
            applied: (Vec::new(), Vec::new()),
            layout_waits: false,
            layout_retry: (now, 0),
            primary_checked: now,
            panels_checked: now,
            physical_due: now,
        }
    }

    /// Works until asked to end, then puts everything back.
    fn run(mut self) {
        loop {
            let mail = self.shared.inbox.take(self.own_time());
            if mail.quit {
                break;
            }
            self.taken = self.taken.max(mail.upto);
            if let Some(w) = mail.wanted {
                self.take(w);
            }
            for job in mail.jobs {
                self.job(job);
            }
            if !self.work() {
                break;
            }
            self.done(Done::Settled(self.taken));
        }
        self.close();
    }

    fn done(&self, done: Done) {
        self.shared
            .events
            .send(Event::Desk(self.shared.generation, done));
    }

    /// When the worker has something to do by itself: a layout the desktop refused
    /// or did not list yet. None: it sleeps until given something.
    fn own_time(&self) -> Option<Instant> {
        if !self.layout_due || self.wanted.is_none() {
            return None;
        }
        if self.layout_waits {
            return Some(self.layout_retry.0.max(Instant::now() + LISTING_LOOK));
        }
        Some(self.layout_retry.0)
    }

    /// Takes the newest wanted state.
    fn take(&mut self, wanted: Wanted) {
        let was = self.wanted.as_ref();
        if was.is_none_or(|w| w.layout != wanted.layout) {
            self.layout_due = true;
        }
        let tuning = (wanted.encoding.bitrate_kbps, wanted.encoding.sharp);
        if self.tuning.is_some_and(|t| t != tuning) {
            for e in self.slots.values().filter_map(|s| s.encoder.as_ref()) {
                if tuning.0 != self.tuning.unwrap_or_default().0 {
                    e.set_bitrate(tuning.0);
                }
                if tuning.1 != self.tuning.unwrap_or_default().1 {
                    e.set_target_usage(wanted.encoding.target_usage());
                }
            }
        }
        self.tuning = Some(tuning);
        self.wanted = Some(wanted);
    }

    /// Steps towards the wanted state until there is none to take, taking newer
    /// states and jobs between steps; then the primary output, wallpapers and the
    /// layout. Whether to go on (not asked to end).
    fn work(&mut self) -> bool {
        loop {
            let (newer, jobs, quit, upto) = self.shared.inbox.peek_newer();
            if quit {
                return false;
            }
            self.taken = self.taken.max(upto);
            if let Some(w) = newer {
                self.take(w);
            }
            for job in jobs {
                self.job(job);
            }
            let Some(step) = self.next_step() else {
                break;
            };
            self.step(step);
        }
        self.keep_primary_ours();
        if std::mem::take(&mut self.wallpapers_due) {
            self.begin("giving new monitors the wallpaper");
            self.desktop.sync_wallpapers();
            self.end();
        }
        self.place();
        true
    }

    fn begin(&self, what: impl Into<String>) {
        self.shared.steps.begin(what);
    }

    fn end(&self) {
        self.shared.steps.end();
    }

    /// The first difference between what is there and what is wanted.
    fn next_step(&self) -> Option<Step> {
        let wanted = self.wanted.as_ref()?;
        let now = Instant::now();
        let find = |id: &ScreenId| wanted.screens.iter().find(|(i, _)| i == id);
        // Outputs not wanted, or wanted as something else, go first (to stay within
        // what the desktop and the encoders take).
        for (id, slot) in &self.slots {
            match find(id) {
                Some((_, w)) if slot.made.is(&w.what) => {}
                _ => return Some(Step::Remove(*id)),
            }
        }
        let cfg = (wanted.encoding.backend, wanted.encoding.codec);
        for (id, w) in &wanted.screens {
            let slot = self.slots.get(id);
            let resting = slot.is_some_and(|s| s.rest_until.is_some_and(|t| now < t));
            let failed = slot.is_some_and(|s| s.failed == Some(w.attempt));
            let has_output = slot.is_some_and(|s| s.output.is_some());
            let encoder = slot.and_then(|s| s.encoder.as_ref());
            match &w.what {
                What::Panel {
                    detach: Some(job), ..
                } if slot.is_none() => {
                    if !self.detached.contains(job) {
                        return Some(Step::Detach(*id, *job));
                    }
                    continue;
                }
                What::Panel { .. } if slot.is_none() => continue,
                What::Physical { .. } if !w.stream => continue,
                _ => {}
            }
            if !has_output {
                if !failed && !resting {
                    return Some(Step::Make(*id));
                }
                continue;
            }
            let Some(encoder) = encoder else {
                if w.stream && !failed && !resting {
                    return Some(Step::Start(*id));
                }
                continue;
            };
            if !w.stream {
                return Some(Step::StopStream(*id));
            }
            if slot.and_then(|s| s.started_for) != Some(cfg) {
                return Some(Step::Restart(*id));
            }
            if encoder.is_paused() == wanted.streaming {
                return Some(Step::Pause(*id, !wanted.streaming));
            }
        }
        None
    }

    fn step(&mut self, step: Step) {
        let name = |id: &ScreenId| {
            self.wanted
                .as_ref()
                .and_then(|w| w.screens.iter().find(|(i, _)| i == id))
                .map_or_else(|| id.to_string(), |(_, w)| w.name.clone())
        };
        match step {
            Step::Remove(id) => {
                self.begin(format!("removing {id}"));
                // A window panel no longer wanted closes, its window back where it was.
                let panel = self.slots.get(&id).is_some_and(Slot::is_panel);
                self.remove(id, panel);
            }
            Step::Detach(id, job) => {
                self.begin(format!("putting the active window on panel {id}"));
                self.detached.insert(job);
                let number = self.wanted.as_ref().and_then(|w| {
                    w.screens.iter().find_map(|(i, s)| match s.what {
                        What::Panel { number, .. } if *i == id => Some(number),
                        _ => None,
                    })
                });
                let result = match number {
                    Some(number) => self.detach(id, number),
                    None => Err(anyhow!("no panel {id} wanted")),
                };
                self.done(Done::Job { job, result });
            }
            Step::StopStream(id) => {
                self.begin(format!("stopping the stream of {}", name(&id)));
                if let Some(slot) = self.slots.get_mut(&id) {
                    let encoder = slot.encoder.take();
                    slot.started_for = None;
                    stop_encoder(encoder, &slot.name);
                }
                self.shared.router.remove_stream(id);
                self.layout_due = true;
            }
            Step::Make(id) => {
                self.begin(format!("making the output of {}", name(&id)));
                self.make(id);
            }
            Step::Start(id) => {
                self.begin(format!("starting the encoder of {}", name(&id)));
                self.start(id, None);
            }
            Step::Restart(id) => {
                self.begin(format!("starting the encoder of {} over", name(&id)));
                self.restart(id);
            }
            Step::Pause(id, paused) => {
                self.begin(format!(
                    "{} the encoder of {}",
                    if paused { "pausing" } else { "resuming" },
                    name(&id)
                ));
                self.set_paused(id, paused);
            }
        }
        self.end();
    }

    /// Removes screen `id`'s output (a window panel's window goes back where it was
    /// with `put_back`).
    fn remove(&mut self, id: ScreenId, put_back: bool) {
        let Some(mut slot) = self.slots.remove(&id) else {
            return;
        };
        if slot.is_panel()
            && let Some(windows) = self.desktop.windows()
        {
            if let Some(key) = slot.key() {
                windows.forget_panel(key);
            }
            if put_back
                && let Made::Panel { origin, .. } = &slot.made
                && let Some(on) = &origin.output
                && let Err(e) = windows.put_back(origin, on)
            {
                warn!("cannot put the window back on {on}: {e:#}");
            }
        }
        if slot.key().is_some() && slot.key() == self.primary_made.as_ref() {
            self.primary_made = None;
        }
        stop_encoder(slot.encoder.take(), &slot.name);
        self.shared.router.remove_stream(id);
        drop(slot);
        self.layout_due = true;
    }

    /// Makes screen `id`'s output: a virtual monitor, or a display of the PC shown.
    fn make(&mut self, id: ScreenId) {
        let Some((_, w)) = self
            .wanted
            .as_ref()
            .and_then(|w| w.screens.iter().find(|(i, _)| *i == id))
            .cloned()
        else {
            return;
        };
        let made = match &w.what {
            What::Monitor { spec, number } => {
                let made = self.make_monitor(id, *number, spec);
                (
                    Made::Monitor {
                        spec: spec.clone(),
                        number: *number,
                    },
                    made.context(UserError::Desktop),
                )
            }
            What::Physical { key, number } => (
                Made::Physical {
                    key: key.clone(),
                    number: *number,
                },
                self.desktop.show(key).context(UserError::Capture),
            ),
            What::Panel { .. } => return,
        };
        let slot = self
            .slots
            .entry(id)
            .or_insert_with(|| Slot::new(made.0.clone(), w.name.clone()));
        slot.made = made.0;
        slot.name.clone_from(&w.name);
        match made.1 {
            Ok(output) => {
                let key = output.key().clone();
                slot.output = Some(output);
                slot.failed = None;
                slot.rest_until = None;
                slot.closed_told = false;
                slot.size_guard = SizeGuard::default();
                if !slot.is_physical() {
                    self.wallpapers_due = true;
                }
                self.layout_due = true;
                self.done(Done::Made { id, key });
            }
            Err(e) => {
                error!("{} cannot start: {e:#}", w.name);
                self.failed(id, w.attempt, &e);
            }
        }
    }

    /// Screen `id` could not be made or start at `attempt`: a monitor waits for the
    /// next attempt, a display of the PC is tried again after a rest.
    fn failed(&mut self, id: ScreenId, attempt: u32, e: &anyhow::Error) {
        if let Some(slot) = self.slots.get_mut(&id) {
            if slot.is_physical() {
                stop_encoder(slot.encoder.take(), &slot.name);
                slot.output = None;
                slot.rest_until = Some(Instant::now() + PHYSICAL_REST);
            } else {
                slot.failed = Some(attempt);
            }
        }
        self.done(Done::Failed {
            id,
            error: code_of(e),
            why: format!("{e:#}"),
        });
    }

    /// A new virtual monitor (or window panel) for screen `id`, as output `number`.
    fn make_monitor(
        &mut self,
        id: ScreenId,
        number: u32,
        spec: &MonitorSpec,
    ) -> Result<Box<dyn Output>> {
        let caps = self.desktop.caps();
        if let Err(no) = &caps.virtual_monitors {
            return Err(anyhow!("{}", no.why).context(no.error.clone()));
        }
        let spec = VirtualSpec {
            id,
            number,
            w: spec.width,
            h: spec.height,
            scale: spec.percent(),
            refresh: caps.refresh_for_new(),
        };
        self.desktop.make(&spec)
    }

    /// Starts screen `id`'s encoder on its output with the wanted settings, else with
    /// `fallback`. Says what came of it.
    fn start(&mut self, id: ScreenId, fallback: Option<EncoderConfig>) {
        let Some(wanted) = self.wanted.clone() else {
            return;
        };
        let Some((_, w)) = wanted.screens.iter().find(|(i, _)| *i == id) else {
            return;
        };
        let Some(mut output) = self.slots.get_mut(&id).and_then(|s| s.output.take()) else {
            return;
        };
        let physical = matches!(w.what, What::Physical { .. });
        let mut result =
            self.start_encoder(id, &w.name, physical, output.as_mut(), &wanted.encoding);
        if let (Err(e), Some(cfg)) = (&result, &fallback) {
            warn!(
                "{}: cannot encode with {:?} {:?}: {e:#}; back to {:?} {:?}",
                w.name, wanted.encoding.backend, wanted.encoding.codec, cfg.backend, cfg.codec
            );
            result = self.start_encoder(id, &w.name, physical, output.as_mut(), cfg);
        }
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        slot.output = Some(output);
        match result {
            Ok(encoder) => {
                encoder.force_keyframe();
                if !wanted.streaming {
                    encoder.set_paused(true);
                }
                let encoding = Encoding {
                    encoder: encoder.kind().0.label(),
                    codec: encoder.codec(),
                    hardware: encoder.kind().0.hardware(),
                    copies: encoder.copies(),
                    note: encoder.note().map(str::to_owned),
                };
                self.shared.router.set_keyframer(id, encoder.keyframer());
                slot.encoder = Some(encoder);
                slot.started_for = Some((wanted.encoding.backend, wanted.encoding.codec));
                slot.failed = None;
                self.layout_due = true;
                self.done(Done::Started { id, encoding });
            }
            Err(e) => {
                error!("{}: no stream: {e:#}", w.name);
                self.shared.router.remove_stream(id);
                let e = e.context(UserError::Encoder);
                if slot.is_panel() {
                    // A window panel without a stream closes, its window back.
                    error!("{id}: no stream; the window panel closes");
                    self.remove(id, true);
                    self.done(Done::PanelGone { id });
                    return;
                }
                self.failed(id, w.attempt, &e);
            }
        }
    }

    /// Starts screen `id`'s encoder over with the wanted settings (the old one stops
    /// first, so a headset never gets frames of two codecs for one stream); if they do
    /// not work, with what it had.
    fn restart(&mut self, id: ScreenId) {
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        let before = slot
            .started_for
            .zip(self.wanted.as_ref())
            .map(|((backend, codec), w)| EncoderConfig {
                backend,
                codec,
                ..w.encoding.clone()
            });
        if let Some(output) = slot.output.as_mut() {
            output.detach();
        }
        let encoder = slot.encoder.take();
        slot.started_for = None;
        stop_encoder(encoder, &slot.name);
        self.start(id, before);
    }

    fn set_paused(&mut self, id: ScreenId, paused: bool) {
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        let Some(encoder) = slot.encoder.take() else {
            return;
        };
        let what = format!(
            "{} the encoder of {}",
            if paused { "pausing" } else { "resuming" },
            slot.name
        );
        let retired = encoder.retired.clone();
        match timed::call("encoder-pause", &what, ENCODER_STOP, move || {
            encoder.set_paused(paused);
            encoder
        }) {
            Ok(encoder) => slot.encoder = Some(encoder),
            // Left behind: started anew at the next step.
            Err(_) => {
                retired.store(true, Ordering::Relaxed);
                slot.started_for = None;
                self.shared.router.remove_stream(id);
            }
        }
    }

    /// A new encoder for stream `id` (`name`, a display of the PC with `physical`),
    /// fed by `output`: the best this PC has for where the output's frames are, in
    /// `cfg`'s codec (only x264 when `cfg` encodes on the CPU), else the next one, in
    /// H.264 if need be (see `encoder/choice.rs`). The stream's slot starts over with
    /// it, and stays when this fails.
    fn start_encoder(
        &self,
        id: ScreenId,
        name: &str,
        physical: bool,
        output: &mut dyn Output,
        cfg: &EncoderConfig,
    ) -> Result<Running> {
        let source = output.source();
        let wanted = Asked {
            codec: cfg.codec,
            software: cfg.backend == Backend::X264,
        };
        let running = self.running_encoders(&[id]);
        let held = |kind: Kind| running.get(&kind).copied().unwrap_or(0);
        let mut tried: Vec<(Choice, String)> = Vec::new();
        let mut again = false;
        let ranked = loop {
            // Ranked anew after each failure: it may have taught a limit.
            let ranked = crate::encoder::catalog().rank(source.memory, wanted, id, &held);
            let Some(choice) = ranked
                .choices
                .iter()
                .find(|c| !tried.iter().any(|(t, _)| t == *c))
                .copied()
            else {
                break ranked;
            };
            let cfg = EncoderConfig {
                backend: choice.backend,
                codec: choice.codec,
                ..cfg.clone()
            };
            self.shared
                .router
                .add_stream(id, choice.codec, name.to_owned(), physical);
            let router = self.shared.router.clone();
            let (input, gpu) = (source.input.clone(), self.gpu.clone());
            // Left behind after its time, a late encoder sends nothing; nor does one
            // let go.
            let retired = Arc::new(AtomicBool::new(false));
            let late = retired.clone();
            let started = timed::call(
                "encoder-start",
                &format!("starting the encoder of {name}"),
                ENCODER_START,
                move || {
                    Encoder::start(&input, &cfg, choice.adapter, &gpu, move |ev| {
                        !late.load(Ordering::Relaxed) && router.on_event(id, ev)
                    })
                },
            )
            .unwrap_or_else(|e| {
                retired.store(true, Ordering::Relaxed);
                Err(e.into())
            });
            let mut encoder = match started {
                Ok(encoder) => encoder,
                // As one that did not open after it started.
                Err(e) => {
                    let text = format!("{e:#}");
                    warn!("{name}: {choice} did not start: {text}");
                    let failure = NotOpened {
                        short: crate::encoder::short_of_sessions(None, &text),
                        why: text.clone(),
                        resumed: false,
                    };
                    let next = crate::encoder::catalog().did_not_open(
                        (choice.backend, choice.adapter),
                        choice.codec,
                        id,
                        &failure,
                        held((choice.backend, choice.adapter)),
                    );
                    if next == Next::Again && !again {
                        again = true;
                    } else {
                        tried.push((choice, text));
                    }
                    continue;
                }
            };
            output.attach(&encoder)?;
            let note = crate::encoder::note(&choice, wanted, &ranked.skipped, &tried);
            match &note {
                Some(note) => warn!("{name}: encoding with {note}"),
                None => info!("{name}: encoding with {choice}"),
            }
            encoder.set_choice(&choice, note);
            return Ok(Running { encoder, retired });
        };
        let mut why: Vec<String> = ranked
            .skipped
            .iter()
            .map(|((b, _), skip)| format!("{}: {}", b.label(), skip.text()))
            .chain(tried.iter().map(|(c, e)| format!("{c}: {e}")))
            .collect();
        if why.is_empty() {
            why.push(
                crate::encoder::pick(cfg.codec, wanted.software)
                    .err()
                    .map_or_else(
                        || {
                            t(
                                "немає кодувальника для цього виходу",
                                "no encoder for this output",
                            )
                            .to_owned()
                        },
                        |e| e.to_string(),
                    ),
            );
        }
        bail!("{}", why.join("; "))
    }

    /// Sessions each encoder holds now, those of the screens `except` left out: every
    /// stream that opened, or is opening (it runs, and has not said it did not open).
    fn running_encoders(&self, except: &[ScreenId]) -> BTreeMap<Kind, u32> {
        let mut running = BTreeMap::new();
        for (_, s) in self.slots.iter().filter(|(id, _)| !except.contains(id)) {
            if let Some(e) = s.encoder.as_ref().filter(|e| e.opened() || !e.is_paused()) {
                *running.entry(e.kind()).or_insert(0) += 1;
            }
        }
        running
    }

    /// One job.
    fn job(&mut self, job: Job) {
        match job {
            Job::Tick => self.upkeep(),
            Job::Focus => {
                let key = self.desktop.windows().and_then(|w| w.active_output());
                self.done(Done::Focus(key));
            }
            Job::LookPhysical => {
                self.physical_due = Instant::now();
                self.look_at_physical();
            }
            Job::Fresh(ids) => {
                for id in ids {
                    let running = self
                        .slots
                        .get(&id)
                        .and_then(|s| s.encoder.as_ref())
                        .is_some_and(|e| !e.is_paused());
                    if running {
                        self.begin(format!("starting the stream of {id} over"));
                        self.set_paused(id, false);
                        self.end();
                    }
                }
            }
            Job::Screenshot { until, .. }
            | Job::FindMouse { until, .. }
            | Job::WindowHere { until, .. }
                if Instant::now() > until =>
            {
                info!("a job of the desktop came too late for whoever asked: dropped");
            }
            Job::Screenshot { job, id, .. } => {
                self.begin(format!("a picture of {id}"));
                let result = self.screenshot(id).map(JobOk::Saved);
                self.end();
                self.done(Done::Job { job, result });
            }
            Job::FindMouse { job, .. } => {
                info!("looking where the mouse is");
                self.begin("finding the mouse");
                let result = self.find_mouse();
                self.end();
                self.done(Done::Job { job, result });
            }
            Job::MouseTo { id, x, y } => {
                self.begin("moving the mouse");
                self.mouse_to(id, x, y);
                self.end();
            }
            Job::WindowHere { job, id, .. } => {
                self.begin(format!("moving the active window to {id}"));
                let result = self.window_here(id).map(|()| JobOk::Done);
                self.end();
                self.done(Done::Job { job, result });
            }
        }
    }

    /// Housekeeping of a live PC.
    fn upkeep(&mut self) {
        self.begin("looking after the outputs");
        self.check_sizes();
        self.check_closed();
        self.retry_unopened();
        self.check_primary();
        self.check_panels();
        self.end();
        self.look_at_physical();
    }

    /// The PC's own displays, every 2 s: said to the control loop; where none is on
    /// though one is to be shown (a laptop's panel left off), one is switched on.
    fn look_at_physical(&mut self) {
        if Instant::now() < self.physical_due {
            return;
        }
        self.physical_due = Instant::now() + PHYSICAL_EVERY;
        self.begin("looking at the PC's displays");
        let seen = self.desktop.physical();
        if seen.is_empty() && self.wanted.as_ref().is_some_and(|w| w.wake_physical) {
            self.desktop.wake_physical();
        }
        self.end();
        self.done(Done::Physical(seen));
    }

    /// Puts back the size of monitors another program changed (on KDE, sharing one in
    /// Discord or OBS can shrink it to what that program asks for). Each monitor's
    /// [`SizeGuard`] decides; the desktop is laid out again once the size is back.
    /// Window panels are left alone: made anew, one would lose its window.
    fn check_sizes(&mut self) {
        // Paused encoders connect again on resuming, which puts the size back too.
        if !self.wanted.as_ref().is_some_and(|w| w.streaming) {
            return;
        }
        let now = Instant::now();
        let ids: Vec<ScreenId> = self
            .slots
            .iter()
            .filter(|(_, s)| matches!(s.made, Made::Monitor { .. }) && s.encoder.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(slot) = self.slots.get_mut(&id) else {
                continue;
            };
            let Some(output) = &slot.output else {
                continue;
            };
            let resized = match output.health() {
                OutputHealth::Resized { w, h } => Some((w, h)),
                _ => None,
            };
            let action = slot.size_guard.check(resized, now);
            let name = output.key().to_string();
            let Made::Monitor { spec, number } = slot.made.clone() else {
                continue;
            };
            let (w, h) = resized.unwrap_or_default();
            match action {
                SizeAction::Wait => {}
                SizeAction::Restored => {
                    info!("{name} is {spec} again");
                    self.layout_due = true;
                }
                // Laid out once the size is back: the desktop still lists the wrong one.
                SizeAction::Restart => {
                    warn!(
                        "{name} was resized to {w}x{h} by another program (screen sharing?): restoring {spec}"
                    );
                    self.restart_same(id);
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

    /// Starts screen `id`'s encoder over with the settings it has.
    fn restart_same(&mut self, id: ScreenId) {
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        if let Some(output) = slot.output.as_mut() {
            output.detach();
        }
        stop_encoder(slot.encoder.take(), &slot.name);
        slot.started_for = None;
        self.start(id, None);
    }

    /// Makes monitor `id`'s output anew with the same number and spec, when nothing
    /// else gives it back its size. Its windows move to the other screens meanwhile.
    fn recreate(&mut self, id: ScreenId) {
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        let guard = std::mem::take(&mut slot.size_guard);
        stop_encoder(slot.encoder.take(), &slot.name);
        slot.started_for = None;
        drop(slot.output.take());
        self.make(id);
        if let Some(slot) = self.slots.get_mut(&id) {
            slot.size_guard = guard;
        }
        if self.slots.get(&id).is_some_and(|s| s.output.is_some()) {
            self.start(id, None);
            if self.primary_made.as_ref() == self.slots.get(&id).and_then(Slot::key) {
                self.primary_made = None;
            }
        }
    }

    /// Monitors the desktop closed are said once (the control loop makes them anew for
    /// the next session); displays of the PC whose stream stopped rest and are shown
    /// again.
    fn check_closed(&mut self) {
        let mut stopped = self.shared.router.take_broken();
        for (id, slot) in &mut self.slots {
            let closed = slot
                .output
                .as_ref()
                .is_some_and(|o| o.health() == OutputHealth::Closed);
            if !closed {
                continue;
            }
            if slot.is_physical() {
                stopped.push(*id);
            } else if !std::mem::replace(&mut slot.closed_told, true) {
                warn!("the desktop closed {}", slot.name);
                self.shared.events.send(Event::Desk(
                    self.shared.generation,
                    Done::Closed { id: *id },
                ));
            }
        }
        stopped.sort_unstable();
        stopped.dedup();
        for id in stopped {
            let Some(slot) = self.slots.get_mut(&id) else {
                continue;
            };
            info!(
                "the stream of {} stopped, starting it again shortly",
                slot.name
            );
            stop_encoder(slot.encoder.take(), &slot.name);
            slot.started_for = None;
            drop(slot.output.take());
            slot.rest_until = Some(Instant::now() + PHYSICAL_REST);
            self.shared.router.remove_stream(id);
            self.layout_due = true;
            self.done(Done::Lost { id });
        }
    }

    /// Streams whose encoder did not open go on with the next encoder for them, or try
    /// the same once more when it was short of sessions the first time (see
    /// `Catalog::did_not_open`, which learns limits from these). Streams that did not
    /// open together are left out of each other's count of sessions held.
    fn retry_unopened(&mut self) {
        let unopened = self.shared.router.take_unopened();
        let ids: Vec<ScreenId> = unopened.iter().map(|(id, _)| *id).collect();
        for (id, failure) in unopened {
            let Some(encoder) = self.slots.get(&id).and_then(|s| s.encoder.as_ref()) else {
                continue;
            };
            let (kind, codec) = (encoder.kind(), encoder.codec());
            let open = self.running_encoders(&ids).get(&kind).copied().unwrap_or(0);
            crate::encoder::catalog().did_not_open(kind, codec, id, &failure, open);
            self.restart_same(id);
        }
        // Encoders seen holding more sessions than a limit says: it rises.
        let mut opened: BTreeMap<Kind, u32> = BTreeMap::new();
        for e in self.slots.values().filter_map(|s| s.encoder.as_ref()) {
            if e.opened() {
                *opened.entry(e.kind()).or_insert(0) += 1;
            }
        }
        if opened.values().any(|n| *n > 1) {
            let mut catalog = crate::encoder::catalog();
            for (kind, n) in opened {
                catalog.seen_open(kind, n);
            }
        }
    }

    /// The output that is to be primary, where it is ours to pick.
    fn primary_key(&self) -> Option<OutputKey> {
        if !self.desktop.caps().has_primary {
            return None;
        }
        match self.wanted.as_ref()?.layout.primary.as_ref()? {
            Primary::Screen(id) => self.slots.get(id).and_then(Slot::key).cloned(),
            Primary::Display(key) => Some(key.clone()),
        }
    }

    /// Makes the wanted primary output the primary one, when that changed (one of
    /// ours once it is made; a display of the PC that does not stream is made primary
    /// by the layout).
    fn keep_primary_ours(&mut self) {
        let key = self.primary_key();
        if key.is_none() || key == self.primary_made {
            return;
        }
        let ours = self
            .slots
            .values()
            .filter_map(|s| s.output.as_ref())
            .find(|o| Some(o.key()) == key.as_ref());
        match ours {
            Some(output) => {
                self.shared.steps.begin("making the primary output primary");
                output.make_primary();
                self.shared.steps.end();
                self.wallpapers_due = true;
            }
            None if matches!(
                self.wanted.as_ref().and_then(|w| w.layout.primary.as_ref()),
                Some(Primary::Screen(_))
            ) =>
            {
                return;
            }
            None => {}
        }
        self.primary_made = key;
        self.layout_due = true;
    }

    /// Makes sure the desktop's primary output (where the Plasma panel lives) is ours.
    /// Right after outputs come and go (a profile switch) KDE can end up with another.
    fn check_primary(&mut self) {
        if self.primary_checked.elapsed() < PRIMARY_EVERY {
            return;
        }
        self.primary_checked = Instant::now();
        if let Some(key) = self.primary_key() {
            self.desktop.keep_primary(&key);
        }
    }

    /// A panel whose window was closed (or moved away) goes too.
    fn check_panels(&mut self) {
        if !self.slots.values().any(Slot::is_panel) || self.panels_checked > Instant::now() {
            return;
        }
        self.panels_checked = Instant::now() + PANELS_EVERY;
        let keys: Vec<OutputKey> = self
            .slots
            .values()
            .filter(|s| s.is_panel())
            .filter_map(|s| s.key().cloned())
            .collect();
        let vacated = match self.desktop.windows() {
            Some(w) => w.vacated(&keys),
            None => Vec::new(),
        };
        for key in vacated {
            let id = self
                .slots
                .iter()
                .find(|(_, s)| s.key() == Some(&key))
                .map(|(id, _)| *id);
            if let Some(id) = id {
                info!("the window on panel {id} is gone: closing the panel");
                self.remove(id, false);
                self.done(Done::PanelGone { id });
            }
        }
    }

    /// Our outputs with their geometry on the desktop.
    fn geometries(&self) -> Vec<(OutputKey, Geometry)> {
        let geometries = self.desktop.outputs();
        self.slots
            .values()
            .filter_map(|s| {
                let key = s.key()?;
                Some((key.clone(), *geometries.get(key)?))
            })
            .collect()
    }

    /// Where the mouse is: the output and the point across its image (0..=65535).
    fn find_mouse(&mut self) -> Result<JobOk> {
        let Some((x, y)) = self.desktop.pointer().and_then(|p| p.position()) else {
            bail!(UserError::NotOnDisplays);
        };
        let frac = |v: i32, start: i32, len: i32| {
            ((f64::from(v - start) / f64::from(len.max(1))).clamp(0.0, 1.0) * 65535.0) as u16
        };
        self.geometries()
            .into_iter()
            .find(|(_, g)| x >= g.x && x < g.x + g.w && y >= g.y && y < g.y + g.h)
            .map(|(key, g)| JobOk::Pointer {
                key,
                u: frac(x, g.x, g.w),
                v: frac(y, g.y, g.h),
            })
            .ok_or_else(|| anyhow::Error::new(UserError::NotOnDisplays))
    }

    /// Puts the mouse on screen `id` at `x`/`y` across its image (0..=65535).
    fn mouse_to(&mut self, id: ScreenId, x: u16, y: u16) {
        let key = self.slots.get(&id).and_then(Slot::key).cloned();
        let Some((_, g)) = self
            .geometries()
            .into_iter()
            .find(|(k, _)| Some(k) == key.as_ref())
        else {
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

    /// Moves the PC's active window to screen `id` (a monitor, or a display of the PC
    /// that streams).
    fn window_here(&mut self, id: ScreenId) -> Result<()> {
        let Some(output) = self
            .slots
            .get(&id)
            .filter(|s| !s.is_panel())
            .and_then(Slot::key)
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
    fn screenshot(&mut self, id: ScreenId) -> Result<std::path::PathBuf> {
        let Some(slot) = self.slots.get(&id).filter(|s| !s.is_physical()) else {
            bail!(UserError::UnknownDisplay);
        };
        let streaming = self.wanted.as_ref().is_some_and(|w| w.streaming);
        // Where the desktop's screenshot tool cannot take one output by name, the
        // monitor's own stream can. A fresh keyframe, decoded into a PNG; the whole
        // desktop if not.
        if !self.desktop.caps().screenshot_one_output
            && let Some(encoder) = slot.encoder.as_ref()
            && streaming
        {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            *self.shared.router.snapshot.lock().unwrap() = Some((id, tx));
            encoder.force_keyframe();
            let frame = rx.recv_timeout(Duration::from_secs(2));
            self.shared.router.snapshot.lock().unwrap().take();
            match frame
                .map_err(anyhow::Error::from)
                .and_then(|data| crate::encoder::keyframe_to_png(&data, encoder.codec()))
            {
                Ok(path) => return Ok(path),
                Err(e) => warn!("screenshot from the stream: {e:#}; taking the whole desktop"),
            }
        }
        let key = slot.key().cloned();
        let name = self.desktop.caps().name;
        let Some(windows) = self.desktop.windows() else {
            return Err(anyhow!("the {name} desktop takes no screenshots"))
                .context(UserError::Capture);
        };
        windows.screenshot(key.as_ref()).context(UserError::Capture)
    }

    /// Puts the PC's active window on a panel of its own, screen `id`, output
    /// `number`: an output its size, which the headset shows as a floating panel.
    fn detach(&mut self, id: ScreenId, number: u32) -> Result<JobOk> {
        let Some(window) = self.desktop.windows().and_then(|w| w.active()) else {
            bail!(UserError::NoWindow);
        };
        let Some(wanted) = self.wanted.clone() else {
            bail!(UserError::Desktop);
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
        let name = format!("Prostir-{number}");
        let mut output = self
            .make_monitor(id, number, &spec)
            .context(UserError::Desktop)?;
        let encoder = self
            .start_encoder(id, &name, false, output.as_mut(), &wanted.encoding)
            .context(UserError::Encoder)
            .inspect_err(|_| self.shared.router.remove_stream(id))?;
        let key = output.key().clone();
        let encoding = Encoding {
            encoder: encoder.kind().0.label(),
            codec: encoder.codec(),
            hardware: encoder.kind().0.hardware(),
            copies: encoder.copies(),
            note: encoder.note().map(str::to_owned),
        };
        self.shared.router.set_keyframer(id, encoder.keyframer());
        let mut slot = Slot::new(
            Made::Panel {
                origin: window.clone(),
                spec: spec.clone(),
                number,
            },
            name,
        );
        slot.output = Some(output);
        slot.encoder = Some(encoder);
        slot.started_for = Some((wanted.encoding.backend, wanted.encoding.codec));
        self.slots.insert(id, slot);
        self.layout_due = true;
        self.wallpapers_due = true;
        let moved = match self.desktop.windows() {
            Some(w) => w.move_to(&window, &key, true),
            None => Err(anyhow!("the desktop has no windows")),
        };
        if let Err(e) = moved {
            self.remove(id, true);
            return Err(e.context(UserError::Desktop));
        }
        if !wanted.streaming
            && let Some(e) = self.slots.get(&id).and_then(|s| s.encoder.as_ref())
        {
            e.set_paused(true);
        }
        self.panels_checked = Instant::now() + PANELS_EVERY;
        info!("window panel {id} on {key} ({width}x{height})");
        Ok(JobOk::Panel {
            id,
            spec,
            key,
            encoding,
        })
    }

    /// Lays the outputs out if anything changed since the last time, once however much
    /// changed. Only outputs the desktop lists already are placed; the others once it
    /// lists them. When the desktop refuses (KScreen stuck, say), the layout stays due
    /// and is tried again after 1, 2, 4... up to 30 s.
    fn place(&mut self) {
        if !self.layout_due || Instant::now() < self.layout_retry.0 || self.wanted.is_none() {
            return;
        }
        self.begin("placing the outputs");
        let placed = self.place_now();
        self.end();
        match placed {
            Ok(on_desk) => {
                self.layout_due = self.layout_waits;
                self.layout_retry = (Instant::now(), 0);
                self.done(Done::Placed(on_desk));
            }
            Err(e) => {
                let failures = self.layout_retry.1 + 1;
                let wait = Duration::from_secs((1u64 << failures.min(5)).min(30));
                warn!("{e:#}; trying again in {} s", wait.as_secs());
                // Once for each time it starts failing: the headset's map is not the
                // desktop's meanwhile.
                if failures == 1 {
                    self.shared.router.send_all(HostMsg::Problem {
                        error: ErrorCode::DESKTOP,
                        detail: state::clip(&format!("{e:#}"), state::MAX_DETAIL),
                        display: None,
                    });
                }
                self.layout_retry = (Instant::now() + wait, failures);
            }
        }
    }

    /// The size of the stream of screen `id`: as its encoder says, else as the
    /// monitor was made.
    fn stream_size_of(&self, id: ScreenId) -> Option<(i32, i32)> {
        let (w, h) = self.shared.router.stream_size(id).or_else(|| {
            let spec = self.slots.get(&id)?.made.spec()?;
            Some((spec.width, spec.height))
        })?;
        Some((i32::try_from(w).ok()?, i32::try_from(h).ok()?))
    }

    /// Lays the outputs out on the desktop like they stand in the headset (see
    /// `prostir_layout::desk`): the monitors in the headset's rows, the laptop screen
    /// under the main monitor unless the headset placed it, other real displays next
    /// to it and window panels right of everything. The mouse then crosses screens
    /// where the eyes expect it to. Returns where each output went.
    fn place_now(&mut self) -> Result<BTreeMap<OutputKey, state::DeskRect>> {
        let Some(wanted) = self.wanted.clone() else {
            return Ok(BTreeMap::new());
        };
        let layout = &wanted.layout;
        let geometries = self.desktop.outputs();
        let mut names: BTreeMap<Out, OutputKey> = self
            .slots
            .iter()
            .filter_map(|(id, s)| Some((Out::Screen(*id), s.key()?.clone())))
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
            .slots
            .values()
            .any(|s| s.key().is_some_and(|n| !geometries.contains_key(n)));
        let has = |id: &ScreenId| self.slots.get(id).is_some_and(|s| s.output.is_some());
        let rows: Vec<Vec<desk::Entry<ScreenId>>> = layout
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .filter(|e| has(&e.id) || !layout.physical.contains(&e.id))
                    .copied()
                    .collect::<Vec<_>>()
            })
            .filter(|r| !r.is_empty())
            .collect();
        // Displays of the PC shown here: the laptop's panel first, under the main
        // monitor, unless the headset placed them.
        let streaming_physical: Vec<Out> = layout
            .physical
            .iter()
            .filter(|id| has(id))
            .map(|id| Out::Screen(*id))
            .collect();
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
                main: layout.main.map(Out::Screen),
                under_main: streaming_physical.first().copied(),
                bottom: streaming_physical
                    .iter()
                    .skip(1)
                    .copied()
                    .chain(names.keys().filter(|o| matches!(o, Out::Other(_))).copied())
                    .collect(),
                column: layout
                    .column
                    .iter()
                    .filter(|id| has(id))
                    .map(|id| Out::Screen(*id))
                    .collect(),
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
        let primary = self.primary_key();
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
        let on_desk = plan
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
        Ok(on_desk)
    }

    /// Puts everything back: window panels first (each window back where it was), then
    /// the outputs and their encoders, then the desktop.
    fn close(mut self) {
        self.begin("putting the desktop back");
        let panels: Vec<ScreenId> = self
            .slots
            .iter()
            .filter(|(_, s)| s.is_panel())
            .map(|(id, _)| *id)
            .collect();
        for id in panels {
            self.remove(id, true);
        }
        let ids: Vec<ScreenId> = self.slots.keys().copied().collect();
        for id in ids {
            self.remove(id, false);
        }
        let steps = self.shared.steps.clone();
        drop(self);
        crate::platform::give_memory_back();
        steps.end();
    }
}

/// Stops `encoder` (of output `name`), waiting for that a while at most; nothing of it
/// reaches the headsets from now on.
fn stop_encoder(encoder: Option<Running>, name: &str) {
    let Some(Running { encoder, retired }) = encoder else {
        return;
    };
    retired.store(true, Ordering::Relaxed);
    let _ = timed::call(
        "encoder-stop",
        &format!("stopping the encoder of {name}"),
        ENCODER_STOP,
        move || {
            #[cfg(feature = "fake-desktop")]
            crate::platform::hang("encoder-stop");
            drop(encoder);
        },
    );
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

#[cfg(test)]
mod tests {
    use super::super::WantedScreen;
    use super::*;
    use crate::encoder::Input;
    use crate::platform::{
        AddModel, Cursor, DesktopCaps, FrameSource, Memory, Origin, Outputs, PhysicalDisplay,
        Pointer, Scales, Windows,
    };
    use std::sync::{Condvar, Mutex};

    /// What the stub desktop was asked to do, in order.
    type Calls = Arc<Mutex<Vec<String>>>;

    /// A desktop whose `make` waits while its gate is shut, as KScreen thinking.
    struct Stub {
        caps: DesktopCaps,
        calls: Calls,
        gate: Arc<(Mutex<bool>, Condvar)>,
        outputs: Arc<Mutex<Outputs>>,
    }

    struct StubOutput {
        key: OutputKey,
        calls: Calls,
        outputs: Arc<Mutex<Outputs>>,
    }

    impl Drop for StubOutput {
        fn drop(&mut self) {
            self.outputs.lock().unwrap().remove(&self.key);
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove {}", self.key));
        }
    }

    impl Output for StubOutput {
        fn key(&self) -> &OutputKey {
            &self.key
        }
        fn health(&self) -> OutputHealth {
            OutputHealth::Live
        }
        fn source(&self) -> FrameSource {
            FrameSource {
                input: Input {
                    source: String::new(),
                    label: self.key.to_string(),
                },
                origin: Origin::Monitor,
                memory: Memory::System,
                cursor: Cursor::InFrame,
            }
        }
        fn attach(&mut self, _encoder: &Encoder) -> Result<()> {
            Ok(())
        }
        fn detach(&mut self) {}
        fn make_primary(&self) {}
    }

    impl Desktop for Stub {
        fn caps(&self) -> &DesktopCaps {
            &self.caps
        }
        fn outputs(&self) -> Outputs {
            self.outputs.lock().unwrap().clone()
        }
        fn physical(&self) -> Vec<PhysicalDisplay> {
            Vec::new()
        }
        fn make(&mut self, spec: &VirtualSpec) -> Result<Box<dyn Output>> {
            let key = OutputKey::new(format!("Prostir-{}", spec.number));
            self.calls.lock().unwrap().push(format!("make {key}"));
            let (shut, ring) = &*self.gate;
            let _open = ring.wait_while(shut.lock().unwrap(), |shut| *shut);
            let geometry = Geometry {
                x: 0,
                y: 0,
                w: spec.w as i32,
                h: spec.h as i32,
                priority: None,
                enabled: true,
                is_virtual: true,
            };
            self.outputs.lock().unwrap().insert(key.clone(), geometry);
            Ok(Box::new(StubOutput {
                key,
                calls: self.calls.clone(),
                outputs: self.outputs.clone(),
            }))
        }
        fn show(&mut self, key: &OutputKey) -> Result<Box<dyn Output>> {
            bail!("no display {key}")
        }
        fn place(&mut self, _plan: &DeskPlan, _current: &Outputs) -> Result<()> {
            self.calls.lock().unwrap().push("place".into());
            Ok(())
        }
        fn windows(&mut self) -> Option<&mut dyn Windows> {
            None
        }
        fn pointer(&mut self) -> Option<&mut dyn Pointer> {
            None
        }
    }

    /// Monitors `specs` (id, number, width), none streaming (no encoders here).
    fn wanted(specs: &[(u32, u32, u32)]) -> Wanted {
        let screens = specs
            .iter()
            .map(|&(id, number, w)| {
                (
                    ScreenId::saved(id),
                    WantedScreen {
                        what: What::Monitor {
                            spec: MonitorSpec {
                                width: w,
                                height: 360,
                                scale: 1.0,
                            },
                            number,
                        },
                        stream: false,
                        attempt: 0,
                        name: format!("Prostir-{number}"),
                    },
                )
            })
            .collect();
        Wanted {
            screens,
            encoding: EncoderConfig {
                codec: Codec::H264,
                bitrate_kbps: 8000,
                backend: Backend::X264,
                sharp: false,
            },
            streaming: true,
            layout: super::super::Layout {
                rows: vec![
                    specs
                        .iter()
                        .map(|&(id, _, _)| desk::Entry::new(ScreenId::saved(id)))
                        .collect(),
                ],
                ..Default::default()
            },
            wake_physical: false,
        }
    }

    /// Wanted states that came while the desktop was busy are replaced by the newest:
    /// the worker makes what the newest says and never what a state between asked
    /// for, and lays the desktop out once, at the end. The step it hangs in shows.
    #[test]
    fn the_newest_wanted_state_wins_and_the_desktop_is_placed_once() {
        let calls = Calls::default();
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let stub = Stub {
            caps: DesktopCaps {
                name: "stub",
                virtual_monitors: Ok(AddModel::OneByOne),
                has_primary: false,
                origin_is_primary: false,
                scales: Scales::Any { min: 50, max: 400 },
                refresh: vec![60],
                screenshot_one_output: true,
                moves_pointer: false,
                cleans_up_after_crash: true,
            },
            calls: calls.clone(),
            gate: gate.clone(),
            outputs: Arc::default(),
        };
        let (events, received) = crate::events::channel();
        let inbox: Arc<Inbox<Wanted, Job>> = Arc::default();
        let steps = Arc::new(Steps::default());
        let shared = Shared {
            inbox: inbox.clone(),
            steps: steps.clone(),
            router: Arc::new(Router::new()),
            events,
            generation: 1,
        };
        let worker = Worker::with(shared, Box::new(stub), GpuDevices::default());
        let thread = std::thread::spawn(move || worker.run());

        inbox.want(wanted(&[(1, 1, 640)]));
        // The worker hangs in making the first monitor: the step shows.
        let until = Instant::now() + Duration::from_secs(5);
        while !steps
            .now()
            .is_some_and(|(_, what)| what.contains("making the output"))
        {
            assert!(Instant::now() < until, "the step never showed");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Two more states while it is busy: only the last counts.
        inbox.want(wanted(&[(1, 1, 640), (2, 2, 800)]));
        let upto = inbox.want(wanted(&[(1, 1, 640), (3, 2, 320)]));
        *gate.0.lock().unwrap() = false;
        gate.1.notify_all();

        let mut made = Vec::new();
        loop {
            match received.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Desk(1, Done::Made { id, .. }) => made.push(id.number()),
                Event::Desk(1, Done::Settled(n)) if n >= upto => break,
                _ => {}
            }
        }
        assert_eq!(made, [1, 3], "never the state in between");
        assert_eq!(
            *calls.lock().unwrap(),
            ["make Prostir-1", "make Prostir-2", "place"],
            "screen 3 took the number 2; one layout"
        );
        assert!(steps.now().is_none(), "no step under way");

        inbox.quit();
        thread.join().unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 5, "{calls:?}");
        assert!(calls[3..].iter().all(|c| c.starts_with("remove")));
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
    /// A worker of a stub desktop that never waits, on a thread of its own.
    struct Harness {
        calls: Calls,
        inbox: Arc<Inbox<Wanted, Job>>,
        received: std::sync::mpsc::Receiver<Event>,
        thread: std::thread::JoinHandle<()>,
    }

    impl Harness {
        fn start() -> Self {
            let calls = Calls::default();
            let stub = Stub {
                caps: DesktopCaps {
                    name: "stub",
                    virtual_monitors: Ok(AddModel::OneByOne),
                    has_primary: false,
                    origin_is_primary: false,
                    scales: Scales::Any { min: 50, max: 400 },
                    refresh: vec![60],
                    screenshot_one_output: true,
                    moves_pointer: false,
                    cleans_up_after_crash: true,
                },
                calls: calls.clone(),
                gate: Arc::new((Mutex::new(false), Condvar::new())),
                outputs: Arc::default(),
            };
            let (events, received) = crate::events::channel();
            let inbox: Arc<Inbox<Wanted, Job>> = Arc::default();
            let shared = Shared {
                inbox: inbox.clone(),
                steps: Arc::default(),
                router: Arc::new(Router::new()),
                events,
                generation: 1,
            };
            let worker = Worker::with(shared, Box::new(stub), GpuDevices::default());
            let thread = std::thread::spawn(move || worker.run());
            Self {
                calls,
                inbox,
                received,
                thread,
            }
        }

        /// Hands `wanted` over and waits until the worker did all of it: the jobs it
        /// answered meanwhile.
        fn settle(&self, wanted: Wanted) -> Vec<(JobId, bool)> {
            let upto = self.inbox.want(wanted);
            let mut jobs = Vec::new();
            loop {
                match self.received.recv_timeout(Duration::from_secs(5)).unwrap() {
                    Event::Desk(_, Done::Job { job, result }) => jobs.push((job, result.is_ok())),
                    Event::Desk(_, Done::Settled(n)) if n >= upto => return jobs,
                    _ => {}
                }
            }
        }

        fn end(self) -> Vec<String> {
            self.inbox.quit();
            self.thread.join().unwrap();
            self.calls.lock().unwrap().clone()
        }
    }

    /// A number given again (a monitor removed, another added at once) is made only
    /// after the output that had it is gone: the difference removes first.
    #[test]
    fn a_number_is_made_again_only_after_its_output_is_gone() {
        let harness = Harness::start();
        harness.settle(wanted(&[(1, 1, 640), (2, 2, 800)]));
        harness.settle(wanted(&[(1, 1, 640), (3, 2, 320)]));
        let calls = harness.end();
        let remove = calls.iter().position(|c| c == "remove Prostir-2").unwrap();
        let again = calls.iter().rposition(|c| c == "make Prostir-2").unwrap();
        assert!(remove < again, "{calls:?}");
    }

    /// A window panel asked for is tried once, however long the state asks for it, and
    /// its answer comes once (here: the stub desktop has no windows).
    #[test]
    fn a_panel_is_tried_once() {
        let harness = Harness::start();
        let mut with_panel = wanted(&[(1, 1, 640)]);
        with_panel.screens.push((
            ScreenId::saved(9),
            WantedScreen {
                what: What::Panel {
                    number: 2,
                    detach: Some(JobId(7)),
                },
                stream: true,
                attempt: 0,
                name: "Prostir-2".into(),
            },
        ));
        assert_eq!(harness.settle(with_panel.clone()), [(JobId(7), false)]);
        with_panel.streaming = false;
        assert_eq!(harness.settle(with_panel), [], "not tried again");
        harness.end();
    }
}
