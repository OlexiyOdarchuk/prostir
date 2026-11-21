//! The selftest on the headset: started by the intent extra `selftest`
//! (`am start ... --es selftest "seconds=30"`), it runs the app against a fake PC
//! (`prostir_testkit::pc::FakePc`) in this process, on `127.0.0.1` at a port of its
//! own, with its own data (`files/selftest/`), and writes its report to the app's
//! external files (`selftest.txt`) and to the log (tag `prostir-selftest`).
//!
//! The app does what it always does; the selftest only watches (events, frame
//! times) and, at set moments, asks for what a person would do in the menu
//! ([`Act`]), while the fake PC breaks things on purpose.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use jni::objects::{JString, JValue};
use log::{info, warn};
use openxr as xr;
use prostir_proto::state::{DisplayId, Gen, HeadsetCaps};
use prostir_testkit::pc::{Clip, Event, FakePc, Happened, PcConfig, trust_each_other};

use super::{Exit, Measured, Options, Probe, Report, Samples, check, is_app_thread};
use crate::net::NetEvent;

/// How many times `android_main` started in this process (the first is 1).
static RUNS: AtomicU32 = AtomicU32::new(0);

/// Counts a start of the app in this process; which one it is.
pub fn count_run() -> u32 {
    RUNS.fetch_add(1, Ordering::Relaxed) + 1
}

/// The OpenXR session must be focused (worn, the app in front) this soon.
const FOCUS_WITHIN: Duration = Duration::from_secs(10);
/// Every stream must show its first picture this soon after the session opened.
const PICTURES_WITHIN: Duration = Duration::from_secs(15);
/// After it starts streaming, before measuring.
const SETTLE: Duration = Duration::from_secs(2);
/// How long a monitor's move, the menu and each breakage get.
const MOVE_FOR: Duration = Duration::from_secs(2);
const MENU_FOR: Duration = Duration::from_millis(1500);
const RECOVER_WITHIN: Duration = Duration::from_secs(5);
const RECONNECT_WITHIN: Duration = Duration::from_secs(10);
/// Beyond the calm part, the scenario takes about 20 s; past this it timed out.
const EXTRA: Duration = Duration::from_secs(60);
/// Frames kept for percentiles: two minutes at 120 Hz (a longer run keeps its first
/// two minutes of frames; the counts go on).
const FRAMES_KEPT: usize = 120 * 120;

/// What the selftest asks the app to do, as a person would in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Nothing,
    /// Swap the first monitor with the next one (the menu's move right), which saves
    /// the arrangement.
    MoveMonitor,
    OpenMenu,
    /// Another tab of the menu (saved in the settings, and redrawn).
    SwitchTab,
    CloseMenu,
    /// Done: leave the main loop.
    Finish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Until the session is focused and every stream shows a picture.
    Waiting,
    Settle,
    /// Calm: frame times, allocations, missed frames.
    Steady,
    Move,
    Menu,
    /// A frame its decoder cannot take, then waiting for the picture to come back.
    Broken,
    /// A stream started over in a new generation.
    Restart,
    /// The fake PC hangs up; waiting for the headset to come back.
    Dropped,
    Done,
}

/// One stream as the fake PC started it.
struct Seen {
    generation: Gen,
    started: Instant,
    shown: Option<Instant>,
    /// The fake PC's session it belongs to.
    session: u32,
}

/// Where one frame's measuring began (see [`Selftest::frame_start`]).
pub struct FrameStart {
    cpu: Duration,
    wall: Instant,
    allocs: Option<u64>,
}

pub struct Selftest {
    options: Options,
    report: Report,
    run: u32,
    begun: Instant,
    /// Set when it has to end before its scenario is over.
    exit: Option<Exit>,
    fails: Vec<String>,
    pc: Option<FakePc>,
    probe: Option<Arc<Mutex<Probe>>>,
    phase: Phase,
    phase_since: Instant,
    focused_at: Option<Instant>,
    connected_at: Option<Instant>,
    displays_at: Option<Instant>,
    /// The fake PC's sessions so far, and the monitors of the last snapshot.
    sessions: u32,
    monitors: Vec<DisplayId>,
    streams: BTreeMap<DisplayId, Seen>,
    first_frames: BTreeMap<DisplayId, f32>,
    broken: Option<(DisplayId, Gen, u32, Instant)>,
    keyframe_asked_ms: Option<f32>,
    recovered_ms: Option<f32>,
    restart: Option<(DisplayId, Instant)>,
    restart_ms: Option<f32>,
    dropped_at: Option<Instant>,
    reconnect_ms: Option<f32>,
    reconnect_picture_ms: Option<f32>,
    period: Option<Duration>,
    last_display_ns: Option<i64>,
    frame_cpu: Samples,
    frame_wall: Samples,
    menu_ms: Samples,
    allocs: Samples,
    persist_ms: Samples,
    missed_steady: u32,
    missed_moving: u32,
    frames: u64,
    layers_max: usize,
    /// All threads' allocations and when, at the start of the calm part.
    allocs_all: Option<(u64, Instant)>,
    allocs_all_per_s: Option<f32>,
    /// The menu's tab was switched (once, half way through its time).
    tab_switched: bool,
}

/// The `selftest` extra of the intent that started the activity, if there is one.
fn intent_extra() -> Option<String> {
    crate::mic::with_activity(|env, activity| {
        let intent = env
            .call_method(activity, "getIntent", "()Landroid/content/Intent;", &[])?
            .l()?;
        if intent.is_null() {
            return Ok(None);
        }
        let key = env.new_string("selftest")?;
        let value = env
            .call_method(
                &intent,
                "getStringExtra",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&key)],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let value = JString::from(value);
        let text: String = env.get_string(&value)?.into();
        Ok(Some(text))
    })
    .flatten()
}

/// `AudioManager.getStreamMaxVolume(STREAM_MUSIC)`: steps above silence.
fn volume_steps() -> Option<i32> {
    crate::mic::with_activity(|env, activity| {
        let name = env.new_string("audio")?;
        let manager = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&name)],
            )?
            .l()?;
        if manager.is_null() {
            return Ok(None);
        }
        let steps = env
            .call_method(&manager, "getStreamMaxVolume", "(I)I", &[JValue::Int(3)])?
            .i()?;
        Ok(Some(steps))
    })
    .flatten()
}

/// A system property (`ro.product.model`), empty when there is none.
fn property(name: &CStr) -> String {
    let mut value = [0u8; 92]; // PROP_VALUE_MAX
    // SAFETY: a nul-terminated name, and a buffer of PROP_VALUE_MAX bytes, which is
    // what the call writes at most (with its nul).
    let n = unsafe { libc::__system_property_get(name.as_ptr(), value.as_mut_ptr().cast()) };
    let n = usize::try_from(n).unwrap_or(0).min(value.len());
    String::from_utf8_lossy(&value[..n]).into_owned()
}

/// A line to the log with the tag `prostir-selftest`.
fn log_line(text: &str) {
    let (Ok(tag), Ok(text)) = (CString::new("prostir-selftest"), CString::new(text)) else {
        return;
    };
    // SAFETY: both strings are nul-terminated and live through the call.
    unsafe {
        ndk_sys::__android_log_write(
            ndk_sys::android_LogPriority::ANDROID_LOG_INFO.0 as i32,
            tag.as_ptr(),
            text.as_ptr(),
        );
    }
}

/// The calling thread's CPU time.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Threads of this app still alive from a start before this one.
fn threads_left() -> usize {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    tasks
        .flatten()
        .filter_map(|t| std::fs::read_to_string(t.path().join("comm")).ok())
        .filter(|name| is_app_thread(name.trim()))
        .count()
}

/// The clips in the APK's `assets/selftest/` (development builds have them).
fn clips(app: &AndroidApp) -> Vec<Clip> {
    let assets = app.asset_manager();
    let Some(dir) = assets.open_dir(c"selftest") else {
        return Vec::new();
    };
    let mut clips = Vec::new();
    for name in dir {
        let Ok(text) = name.to_str() else { continue };
        let Ok(path) = CString::new(format!("selftest/{text}")) else {
            continue;
        };
        let Some(mut asset) = assets.open(&path) else {
            continue;
        };
        let data = match asset.buffer() {
            Ok(data) => data.to_vec(),
            Err(e) => {
                warn!("selftest clip {text}: {e}");
                continue;
            }
        };
        match Clip::named(text, data) {
            Ok(clip) => clips.push(clip),
            Err(e) => warn!("selftest clip {text}: {e:#}"),
        }
    }
    // The biggest first: the fake PC picks the first ones the headset decodes.
    clips.sort_by_key(|c| {
        std::cmp::Reverse((c.width * c.height, c.codec == prostir_proto::Codec::H265))
    });
    clips
}

impl Selftest {
    /// A selftest, if the intent that started the app asks for one; `run` is which
    /// start in this process this is.
    pub fn requested(run: u32) -> Option<Self> {
        let extra = intent_extra()?;
        let now = Instant::now();
        let mut test = Self {
            options: Options::default(),
            report: Report::default(),
            run,
            begun: now,
            exit: None,
            fails: Vec::new(),
            pc: None,
            probe: None,
            phase: Phase::Waiting,
            phase_since: now,
            focused_at: None,
            connected_at: None,
            displays_at: None,
            sessions: 0,
            monitors: Vec::new(),
            streams: BTreeMap::new(),
            first_frames: BTreeMap::new(),
            broken: None,
            keyframe_asked_ms: None,
            recovered_ms: None,
            restart: None,
            restart_ms: None,
            dropped_at: None,
            reconnect_ms: None,
            reconnect_picture_ms: None,
            period: None,
            last_display_ns: None,
            frame_cpu: Samples::with_capacity(FRAMES_KEPT),
            frame_wall: Samples::with_capacity(FRAMES_KEPT),
            menu_ms: Samples::with_capacity(1024),
            allocs: Samples::with_capacity(FRAMES_KEPT),
            persist_ms: Samples::with_capacity(64),
            missed_steady: 0,
            missed_moving: 0,
            frames: 0,
            layers_max: 0,
            allocs_all: None,
            allocs_all_per_s: None,
            tab_switched: false,
        };
        test.lay_out_report();
        match Options::parse(&extra) {
            Ok(options) => test.options = options,
            Err(e) => test.end_early(Exit::NoStart, &e),
        }
        test.report.set("seconds", test.options.seconds);
        info!("selftest: {extra:?}, start {run} in this process");
        Some(test)
    }

    /// Every key in its place from the start, so reports read alike even when one
    /// ends early.
    fn lay_out_report(&mut self) {
        let r = &mut self.report;
        r.set("selftest", 1);
        r.set("app", prostir_proto::app_version(env!("CARGO_PKG_VERSION")));
        r.set("pid", std::process::id());
        r.set("run", self.run);
        r.set("manufacturer", property(c"ro.product.manufacturer"));
        r.set("model", property(c"ro.product.model"));
        for key in [
            "runtime",
            "extensions",
            "caps",
            "decoders",
            "streams_max",
            "layers_max",
            "volume_steps",
            "refresh_rates",
            "refresh_hz",
            "period_ms",
        ] {
            r.set(key, "none");
        }
        r.set("perf", "default");
        r.set("connect_ms", "none");
        r.set("snapshot_ms", "none");
        r.set("threads_left", threads_left());
        r.set(
            "battery.start",
            crate::mic::battery().map_or_else(|| "none".into(), |(p, _)| p.to_string()),
        );
    }

    fn end_early(&mut self, exit: Exit, why: &str) {
        warn!("selftest: {why}");
        self.fails.push(why.to_owned());
        self.exit = Some(self.exit.map_or(exit, |e| e.max(exit)));
        self.phase = Phase::Done;
    }

    /// Whether the scenario is over (the app then leaves its main loop).
    pub fn over(&self) -> bool {
        self.phase == Phase::Done
    }

    /// The selftest's own data directory in `internal` (`selftest/`), emptied: what
    /// the app keeps there (keys, settings, the arrangement) never touches the
    /// person's own. Settings start with the first-run tips read, as after any first
    /// run, so the menu opens only when the scenario opens it.
    pub fn data_dir(&mut self, internal: &Path) -> PathBuf {
        let dir = internal.join("selftest");
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.end_early(
                Exit::NoStart,
                &format!("cannot make {}: {e}", dir.display()),
            );
        }
        let mut settings = crate::settings::Settings {
            intro_seen: true,
            ..crate::settings::Settings::default()
        };
        settings.last_tab = 0;
        settings.save(&dir.join("settings.txt"));
        dir
    }

    /// Starts the fake PC with the clips of the APK, trusted by the headset's keys in
    /// `dir`: its address, which the app connects to and nothing else.
    pub fn start_pc(&mut self, app: &AndroidApp, dir: &Path) -> Option<std::net::SocketAddr> {
        if self.over() {
            return None;
        }
        let clips = clips(app);
        if clips.is_empty() {
            self.end_early(
                Exit::NoStart,
                "no clips in assets/selftest (a store build has none)",
            );
            return None;
        }
        let pc_key = dir.join("fake-pc-identity");
        let headset = match trust_each_other(dir, &pc_key, "Prostir selftest") {
            Ok((headset, _)) => headset,
            Err(e) => {
                self.end_early(Exit::NoStart, &format!("keys: {e:#}"));
                return None;
            }
        };
        let pc = FakePc::start(PcConfig {
            identity: pc_key,
            trusted: vec![headset],
            clips,
            monitors: 2,
            name: "Prostir selftest".into(),
        });
        match pc {
            Ok(pc) => {
                let addr = pc.addr();
                info!("selftest: fake PC on {addr}");
                self.pc = Some(pc);
                self.probe = Some(super::probe_start());
                self.begun = Instant::now();
                Some(addr)
            }
            Err(e) => {
                self.end_early(Exit::NoStart, &format!("fake PC: {e:#}"));
                None
            }
        }
    }

    /// What the OpenXR runtime is and offers (every extension it lists, not only the
    /// ones the app turns on).
    pub fn runtime(
        &mut self,
        instance: &xr::Instance,
        system: xr::SystemId,
        available: &xr::ExtensionSet,
    ) {
        if let Ok(p) = instance.properties() {
            self.report.set(
                "runtime",
                format!("{} {}", p.runtime_name, p.runtime_version),
            );
        }
        let names: Vec<String> = available
            .names()
            .iter()
            .map(|n| String::from_utf8_lossy(n.strip_suffix(&[0]).unwrap_or(n)).into_owned())
            .collect();
        self.report.set("extensions", names.join(","));
        // The app turns on every optional extension the runtime has.
        self.report.set("caps", "full");
        if let Ok(p) = instance.system_properties(system) {
            self.report
                .set("layers_max", p.graphics_properties.max_layer_count);
        }
        self.report.set(
            "volume_steps",
            volume_steps().map_or_else(|| "none".into(), |s| s.to_string()),
        );
    }

    /// What the app says it decodes (fixed in this build).
    pub fn headset_caps(&mut self, caps: &HeadsetCaps) {
        let decoders: Vec<String> = caps
            .decoders
            .iter()
            .map(|d| {
                let codec = match d.codec {
                    0 => "h264",
                    1 => "h265",
                    _ => "other",
                };
                format!("{codec}:{}x{}", d.max_w, d.max_h)
            })
            .collect();
        self.report.set("decoders", decoders.join(","));
        self.report.set("streams_max", caps.streams_max);
    }

    /// The display's refresh rates, once the session runs.
    pub fn refresh(&mut self, offered: Option<Vec<f32>>, now: Option<f32>) {
        if let Some(rates) = offered {
            let rates: Vec<String> = rates.iter().map(|r| format!("{r:.0}")).collect();
            self.report.set("refresh_rates", rates.join(","));
        }
        if let Some(hz) = now {
            self.report.set("refresh_hz", format!("{hz:.0}"));
        }
    }

    pub fn session_state(&mut self, state: xr::SessionState) {
        if state == xr::SessionState::FOCUSED && self.focused_at.is_none() {
            self.focused_at = Some(Instant::now());
        }
    }

    /// An event of the network thread, before the app handles it.
    pub fn net_event(&mut self, event: &NetEvent) {
        let now = Instant::now();
        match event {
            NetEvent::Connected { .. } if self.connected_at.is_none() => {
                self.connected_at = Some(now);
                self.report.set_ms("connect_ms", Some(ms(now - self.begun)));
            }
            NetEvent::Part(prostir_proto::HostMsg::Displays { .. })
                if self.displays_at.is_none() =>
            {
                self.displays_at = Some(now);
                if let Some(c) = self.connected_at {
                    self.report.set_ms("snapshot_ms", Some(ms(now - c)));
                }
            }
            _ => {}
        }
    }

    /// Right after `xrWaitFrame` returned.
    pub fn frame_start(&self) -> FrameStart {
        FrameStart {
            cpu: thread_cpu(),
            wall: Instant::now(),
            allocs: crate::alloc_count::main_thread(),
        }
    }

    /// Right after `xrEndFrame` returned: the frame's work (`menu`: how long the menu
    /// took to draw, if it was drawn), when it is shown, its period and its layers.
    pub fn frame_end(
        &mut self,
        start: &FrameStart,
        display_ns: i64,
        period: Duration,
        menu: Option<Duration>,
        layers: usize,
    ) {
        let allocs = crate::alloc_count::main_thread()
            .zip(start.allocs)
            .map(|(b, a)| b.saturating_sub(a));
        let cpu = thread_cpu().saturating_sub(start.cpu);
        let wall = start.wall.elapsed();
        if self.period.is_none() && !period.is_zero() {
            self.report.set_ms("period_ms", Some(ms(period)));
        }
        self.period = Some(period).filter(|p| !p.is_zero()).or(self.period);
        self.frames += 1;
        self.layers_max = self.layers_max.max(layers);
        if let Some(menu) = menu {
            self.menu_ms.add(ms(menu));
        }
        let missed = self.last_display_ns.is_some_and(|last| {
            (display_ns - last) as f64 > 1.5 * period.as_nanos() as f64 && !period.is_zero()
        });
        self.last_display_ns = Some(display_ns);
        match self.phase {
            Phase::Steady => {
                self.missed_steady += u32::from(missed);
                if menu.is_none() {
                    self.frame_cpu.add(ms(cpu));
                    self.frame_wall.add(ms(wall));
                    if let Some(n) = allocs {
                        self.allocs.add(n as f32);
                    }
                }
            }
            Phase::Move => self.missed_moving += u32::from(missed),
            _ => {}
        }
    }

    /// An act that saves (the arrangement, the settings) took this long, in the frame.
    pub fn saved(&mut self, took: Duration) {
        self.persist_ms.add(ms(took));
    }

    /// Once a pass of the main loop, frame or not: what the app should do now.
    pub fn step(&mut self) -> Act {
        self.take_pc_events();
        let now = Instant::now();
        let in_phase = now - self.phase_since;
        if self.phase != Phase::Done
            && now - self.begun > Duration::from_secs(u64::from(self.options.seconds)) + EXTRA
        {
            self.end_early(
                Exit::Timeout,
                &format!("still {:?} at the deadline", self.phase),
            );
            return Act::Finish;
        }
        match self.phase {
            Phase::Waiting => {
                if self.focused_at.is_none() && now - self.begun > FOCUS_WITHIN {
                    self.end_early(
                        Exit::NoStart,
                        "the OpenXR session was not focused within 10 s (is the headset worn?)",
                    );
                    return Act::Finish;
                }
                if self.all_shown() {
                    self.next(Phase::Settle, now);
                } else if now - self.begun > PICTURES_WITHIN {
                    self.end_early(Exit::Fail, "no picture on every monitor within 15 s");
                    return Act::Finish;
                }
                Act::Nothing
            }
            Phase::Settle if in_phase >= SETTLE => {
                self.allocs_all = crate::alloc_count::all().map(|n| (n, now));
                self.next(Phase::Steady, now);
                Act::Nothing
            }
            Phase::Steady if in_phase >= Duration::from_secs(u64::from(self.options.seconds)) => {
                if let (Some((before, at)), Some(after)) =
                    (self.allocs_all, crate::alloc_count::all())
                {
                    self.allocs_all_per_s =
                        Some(after.saturating_sub(before) as f32 / (now - at).as_secs_f32());
                }
                self.next(Phase::Move, now);
                Act::MoveMonitor
            }
            Phase::Move if in_phase >= MOVE_FOR => {
                self.next(Phase::Menu, now);
                Act::OpenMenu
            }
            Phase::Menu if in_phase >= MENU_FOR * 2 => {
                if let Some(id) = self.monitors.first().copied()
                    && let Some(pc) = &self.pc
                    && pc.send_broken_frame(id)
                {
                    self.next(Phase::Broken, now);
                } else {
                    self.end_early(Exit::Fail, "the fake PC has no session to break a frame in");
                }
                Act::CloseMenu
            }
            Phase::Menu if in_phase >= MENU_FOR && !self.tab_switched => {
                self.tab_switched = true;
                Act::SwitchTab
            }
            Phase::Broken if self.recovered_ms.is_some() || in_phase >= RECOVER_WITHIN => {
                if let Some(id) = self
                    .monitors
                    .get(1)
                    .or_else(|| self.monitors.first())
                    .copied()
                    && let Some(pc) = &self.pc
                    && pc.restart_stream(id)
                {
                    self.restart = Some((id, now));
                }
                self.next(Phase::Restart, now);
                Act::Nothing
            }
            Phase::Restart if self.restart_ms.is_some() || in_phase >= RECOVER_WITHIN => {
                if let Some(pc) = &self.pc {
                    pc.drop_connection();
                }
                self.dropped_at = Some(now);
                self.next(Phase::Dropped, now);
                Act::Nothing
            }
            Phase::Dropped
                if self.reconnect_picture_ms.is_some() || in_phase >= RECONNECT_WITHIN =>
            {
                self.next(Phase::Done, now);
                Act::Finish
            }
            Phase::Done => Act::Finish,
            _ => Act::Nothing,
        }
    }

    fn next(&mut self, phase: Phase, now: Instant) {
        info!("selftest: {phase:?}");
        self.phase = phase;
        self.phase_since = now;
    }

    /// Every monitor of the fake PC's latest session shows a picture.
    fn all_shown(&self) -> bool {
        !self.monitors.is_empty()
            && self.monitors.iter().all(|id| {
                self.streams
                    .get(id)
                    .is_some_and(|s| s.session == self.sessions && s.shown.is_some())
            })
    }

    fn take_pc_events(&mut self) {
        // Taken out meanwhile, so the events need no list of their own.
        let Some(pc) = self.pc.take() else { return };
        while let Ok(Event { at, what }) = pc.events().try_recv() {
            self.pc_event(at, what);
        }
        self.pc = Some(pc);
    }

    fn pc_event(&mut self, at: Instant, what: Happened) {
        match what {
            Happened::Opened { .. } => {
                self.sessions += 1;
                if let Some(dropped) = self.dropped_at
                    && self.sessions > 1
                    && self.reconnect_ms.is_none()
                {
                    self.reconnect_ms = Some(ms(at - dropped));
                }
            }
            Happened::Snapshot { monitors } => self.monitors = monitors,
            Happened::StreamStarted { id, generation, .. } => {
                self.streams.insert(
                    id,
                    Seen {
                        generation,
                        started: at,
                        shown: None,
                        session: self.sessions,
                    },
                );
            }
            Happened::Shown {
                id,
                generation,
                seq,
            } => {
                let session = self.sessions;
                let Some(s) = self
                    .streams
                    .get_mut(&id)
                    .filter(|s| s.generation == generation && s.session == session)
                else {
                    return;
                };
                if s.shown.is_none() {
                    s.shown = Some(at);
                    let first = ms(at - s.started);
                    if session == 1 {
                        self.first_frames.entry(id).or_insert(first);
                    }
                    if let Some((r, _)) = self.restart
                        && r == id
                        && self.restart_ms.is_none()
                        && session == 1
                    {
                        self.restart_ms = Some(first);
                    }
                    if let Some(dropped) = self.dropped_at
                        && session > 1
                        && self.reconnect_picture_ms.is_none()
                    {
                        self.reconnect_picture_ms = Some(ms(at - dropped));
                    }
                }
                if let Some((b, g, bad, since)) = self.broken
                    && b == id
                    && g == generation
                    && seq > bad
                    && self.recovered_ms.is_none()
                {
                    self.recovered_ms = Some(ms(at - since));
                }
            }
            Happened::BrokenSent {
                id,
                generation,
                seq,
            } => self.broken = Some((id, generation, seq, at)),
            Happened::KeyframeAsked { id, generation } => {
                if let Some((b, g, _, since)) = self.broken
                    && (b, g) == (id, generation)
                    && self.keyframe_asked_ms.is_none()
                {
                    self.keyframe_asked_ms = Some(ms(at - since));
                }
            }
            _ => {}
        }
    }

    /// Ends the selftest: the report, to the log and to `external`'s `selftest.txt`.
    /// `error`: the app could not go on. Whether the process should stay (the next
    /// start runs in it), and the exit code otherwise.
    pub fn finish(mut self, external: Option<&Path>, error: Option<&anyhow::Error>) -> (bool, i32) {
        self.take_pc_events();
        if let Some(e) = error {
            let exit = if self.focused_at.is_some() {
                Exit::Fail
            } else {
                Exit::NoStart
            };
            self.end_early(exit, &format!("{e:#}"));
        }
        if let Some(pc) = self.pc.take() {
            for (k, id) in self.monitors.clone().iter().enumerate() {
                self.report
                    .set(&format!("stream.{}.sent", k + 1), pc.frames_sent(*id));
            }
            pc.stop();
        }
        super::probe_stop();
        self.fill_report();
        let exit = match self.exit {
            Some(exit) => exit,
            None => {
                let fails = check(&self.measured());
                self.fails.extend(fails);
                if self.fails.is_empty() {
                    Exit::Pass
                } else {
                    Exit::Fail
                }
            }
        };
        let r = &mut self.report;
        r.set(
            "battery.end",
            crate::mic::battery().map_or_else(|| "none".into(), |(p, _)| p.to_string()),
        );
        if !self.fails.is_empty() {
            r.set("fail", self.fails.join("; "));
        }
        r.set("result", if exit == Exit::Pass { "pass" } else { "fail" });
        r.set("exit", exit.code());
        let text = r.text();
        for line in text.lines() {
            log_line(line);
        }
        if let Some(dir) = external {
            let path = dir.join("selftest.txt");
            let tmp = dir.join("selftest.txt.part");
            if let Err(e) = std::fs::write(&tmp, &text).and_then(|()| std::fs::rename(&tmp, &path))
            {
                warn!("selftest: cannot write {}: {e}", path.display());
            }
        }
        (self.options.relaunch && exit != Exit::NoStart, exit.code())
    }

    fn measured(&self) -> Measured {
        let share = |value: Option<f32>| {
            let period = self.period.map(ms)?;
            value.map(|v| v / period)
        };
        Measured {
            first_frames_ms: self
                .monitors
                .iter()
                .map(|id| self.first_frames.get(id).copied())
                .collect(),
            decoder_recovered_ms: self.recovered_ms,
            reconnect_ms: self.reconnect_ms,
            frame_share_p95: share(self.frame_cpu.percentile(0.95)),
            frame_share_p99: share(self.frame_cpu.percentile(0.99)),
            menu_share_max: share(self.menu_ms.max()),
        }
    }

    fn fill_report(&mut self) {
        let m = self.measured();
        let probe = self.probe.take().map(|p| {
            std::mem::take(&mut *p.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
        });
        let r = &mut self.report;
        for (k, id) in self.monitors.iter().enumerate() {
            let n = k + 1;
            r.set_ms(
                &format!("stream.{n}.first_frame_ms"),
                self.first_frames.get(id).copied(),
            );
            let decoded = probe
                .as_ref()
                .and_then(|p| p.decoded.get(&id.0).copied())
                .unwrap_or(0);
            r.set(&format!("stream.{n}.frames"), decoded);
        }
        if let Some(p) = &probe {
            for (key, h) in [("decode_ms", &p.decode), ("net_to_decoder_ms", &p.wait)] {
                for (name, q) in [("p50", 0.5), ("p95", 0.95), ("p99", 0.99)] {
                    r.set_ms(&format!("{key}.{name}"), Some(ms(h.percentile(q))));
                }
                r.set_ms(&format!("{key}.max"), Some(ms(h.max())));
            }
        }
        r.set_ms("keyframe_asked_ms", self.keyframe_asked_ms);
        r.set_ms("decoder_recovered_ms", self.recovered_ms);
        // The decoder's own fault hooks come with its rework (step 7.8b).
        r.set("surface_recreated_ms", "none");
        r.set_ms("stream_restart_ms", self.restart_ms);
        r.set_ms("reconnect_ms", self.reconnect_ms);
        r.set_ms("reconnect_picture_ms", self.reconnect_picture_ms);
        r.set_spread("frame_cpu_ms", &self.frame_cpu);
        r.set_spread("frame_wall_ms", &self.frame_wall);
        let share = |v: Option<f32>| v.map(|v| format!("{v:.3}"));
        r.set(
            "frame_cpu_share.p95",
            share(m.frame_share_p95).unwrap_or_else(|| "none".into()),
        );
        r.set(
            "frame_cpu_share.p99",
            share(m.frame_share_p99).unwrap_or_else(|| "none".into()),
        );
        r.set_ms("menu_redraw_ms.max", self.menu_ms.max());
        r.set(
            "menu_redraw_share.max",
            share(m.menu_share_max).unwrap_or_else(|| "none".into()),
        );
        r.set("menu_redraws", self.menu_ms.len());
        r.set("frames", self.frames);
        r.set("frames_missed", self.missed_steady);
        r.set("frames_missed_moving", self.missed_moving);
        r.set("layers.max", self.layers_max);
        r.set_ms("persist_ms.max", self.persist_ms.max());
        match self.allocs.mean() {
            Some(mean) => r.set("alloc_per_frame", format!("{mean:.1}")),
            None => r.set("alloc_per_frame", "none"),
        }
        r.set(
            "alloc_per_frame.max",
            self.allocs
                .max()
                .map_or_else(|| "none".into(), |m| format!("{m:.0}")),
        );
        r.set(
            "alloc_all_threads_per_s",
            self.allocs_all_per_s
                .map_or_else(|| "none".into(), |a| format!("{a:.0}")),
        );
    }
}

fn ms(d: Duration) -> f32 {
    d.as_secs_f32() * 1000.0
}
