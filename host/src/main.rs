// Every unsafe block says why it is sound.
#![deny(clippy::undocumented_unsafe_blocks)]
mod api;
mod brain;
mod ctl;
mod displays;
mod encoder;
mod errors;
mod events;
mod i18n;
mod layout;
mod listen;
mod local;
mod outbox;
mod pacing;
mod pairing;
mod platform;
mod publish;
mod requests;
mod router;
mod screen;
mod session;
mod store;
mod usage;

use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use log::{error, info, warn};
use prostir_proto::request::{Asked, Command};
use prostir_proto::state::ErrorCode;
use prostir_proto::{ClientMsg, Codec, HostMsg, PcGoodbye};

use brain::{Admit, LastEnded, Phase};
use displays::Displays;
use encoder::EncoderConfig;
use events::{Accepted, Event, EventSink, SessionId};
use platform::MonitorSpec;
use platform::{TrayCommand, TrayStatus, tray};
use screen::ScreenId;
use session::{Closing, Session, Standing, TrayLine};
use store::Store;

#[derive(Parser, Debug)]
#[command(
    about = "Virtual monitors for KDE Plasma and Hyprland, streamed to a Meta Quest 3",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Sub>,
    /// Without a command: as `prostir serve`.
    #[command(flatten)]
    serve: Args,
}

#[derive(Subcommand, Debug)]
enum Sub {
    /// Run: wait for the headset and serve it (the same as no command).
    Serve(Args),
    /// Open the settings window, starting Prostir first if it is not running.
    Open,
    /// Ask the running Prostir something (`prostir ctl help` lists the commands).
    #[command(disable_help_flag = true)]
    Ctl {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
}

#[derive(clap::Args, Debug)]
struct Args {
    /// Monitor in pixels, optionally with KWin scale: 2560x1440 or 3840x2160@1.5. Repeat for
    /// several monitors. Without it, the set from the last session (changed from the headset
    /// menu) is used, or a single 2560x1440.
    #[arg(short, long = "monitor", value_parser = parse_monitor)]
    monitors: Vec<MonitorSpec>,
    /// Leave KDE's primary monitor alone (by default the middle Quest monitor becomes primary).
    #[arg(long)]
    keep_primary: bool,
    /// Video codec. Without it: the one chosen in the headset menu, else HEVC (sharpest
    /// small text). H.264 needs far less memory on Intel (~9 MB against ~70 MB of driver
    /// memory per monitor) at a slightly softer picture.
    #[arg(long, value_enum)]
    codec: Option<CodecArg>,
    /// Bitrate per monitor, Mbit/s (default: the quality chosen in the tray, or 30).
    #[arg(long)]
    bitrate: Option<u32>,
    /// Encode on the CPU with x264 even when the graphics card can (implies H.264).
    /// Without it Prostir uses VA-API (Intel, AMD), then NVENC (NVIDIA), then x264.
    #[arg(long)]
    software: bool,
    #[arg(long, default_value_t = prostir_proto::TCP_PORT)]
    port: u16,
    /// Keep the virtual monitors this many seconds after the headset disconnects.
    #[arg(long, default_value_t = 30)]
    linger: u64,
    /// Do not announce the host on the local network.
    #[arg(long)]
    no_discovery: bool,
    /// Do not run `adb reverse` for USB connections.
    #[arg(long)]
    no_adb: bool,
    /// Open the settings window once Prostir is ready (`prostir open` starts it so).
    #[arg(long)]
    open_settings: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CodecArg {
    H264,
    H265,
}

fn parse_monitor(s: &str) -> Result<MonitorSpec, String> {
    s.parse()
}

/// The tray icon, what it shows, and the commands for the headset waiting to go.
struct Control {
    tray: Option<tray::Handle>,
    shown: Option<TrayStatus>,
    /// The settings the tray shows: quality, the text preset, the counter.
    shown_settings: Option<(u32, bool, bool)>,
    /// The battery the tray shows.
    shown_battery: Option<Option<(u8, bool)>>,
    /// Commands for the headset from the tray and the local API, and who waits for
    /// what came of each.
    for_headset: VecDeque<(Asked<Command>, Option<api::Reply>)>,
}

impl Control {
    /// Commands for the headset with no headset to take them.
    fn no_headset(&mut self, answers: &api::Answers) {
        for (asked, reply) in self.for_headset.drain(..) {
            info!("{asked:?}: no headset is connected");
            if let Some(r) = reply {
                answers.push(
                    r,
                    api::CtlReply::failed(
                        api::ApiError::NotConnected,
                        i18n::t("Шолом не підключено", "No headset is connected"),
                    ),
                );
            }
        }
    }

    /// Shows the current state and settings in the tray (only what changed): of the
    /// headsets, the one the tray follows (`headset`) and how many others there are.
    fn show(
        &mut self,
        displays: Option<&Displays>,
        store: &Store,
        headset: &TrayLine,
        others: usize,
    ) {
        let status = TrayStatus {
            no_limits: headset.no_limits,
            mic_denied: headset.mic_denied,
            connected: headset.via,
            others,
            monitors: displays.map_or(0, Displays::monitor_count),
            mirror: displays.is_some_and(Displays::mirror_active),
            paused: displays.is_some_and(|d| !d.streaming()),
        };
        if self.shown.as_ref() != Some(&status) {
            tray::set_status(self.tray.as_ref(), status.clone());
            self.shown = Some(status);
        }
        if self.shown_battery != Some(headset.battery) {
            tray::set_battery(self.tray.as_ref(), headset.battery);
            self.shown_battery = Some(headset.battery);
        }
        let wanted = store.wanted();
        let settings = (wanted.bitrate, wanted.sharp, wanted.count);
        if self.shown_settings != Some(settings) {
            tray::set_settings(self.tray.as_ref(), settings.0, settings.1, settings.2);
            self.shown_settings = Some(settings);
        }
    }
}

/// Keeps the set and its rows as they are now (`chosen`: the headset changed which
/// monitors there are).
pub fn keep_screens(store: &mut Store, displays: &Displays, chosen: bool) {
    if let Some(set) = displays.saved_screens() {
        store.set_screens(set, chosen);
    }
}

/// Taken off for this long, the headset counts as gone.
const TAKEN_OFF_GRACE: Duration = Duration::from_secs(10);

/// Events handled one after the other before the next pass of the control loop, at
/// most for this long: what a pass costs never adds up behind a stream of small
/// messages (the microphone sends a hundred a second), and the clocks still run.
const TAKE_AT_ONCE: Duration = Duration::from_millis(50);
/// A pass of the control loop that takes this long is written in the log: the headset
/// waits for it.
const BUSY: Duration = Duration::from_millis(150);

/// The control loop: the one owner of the PC's state (settings, the monitors, the
/// sessions), acting on the events every other thread sends and on its own deadlines.
struct Brain {
    /// How long the monitors stay after the last headset leaves.
    linger: Duration,
    /// Make the middle monitor the desktop's primary one.
    set_primary: bool,
    store: Store,
    control: Control,
    security: Arc<pairing::Security>,
    /// For the sessions' readers.
    events: EventSink,
    displays: Option<Displays>,
    /// The sessions, by the headset's key: one each, at most `brain::max_headsets()`.
    sessions: BTreeMap<[u8; 32], Session>,
    /// Which headsets' sessions have the jobs only one can have (see `brain::roles`).
    roles: brain::Roles<[u8; 32]>,
    /// The order the headsets with a session first connected in.
    arrivals: brain::Arrivals<[u8; 32]>,
    /// Sessions that said goodbye, until their headset closes or their moment ends.
    closing: Vec<Closing>,
    phase: Phase,
    last_id: SessionId,
    /// The output that has the mouse, as last looked at.
    focus: Option<ScreenId>,
    quit: bool,
    /// The local API: where the state, events and replies go out.
    api: api::Outlet,
    /// The state as last sent out, and its revision.
    shown: Option<Arc<api::State>>,
    revision: u32,
    /// Replies of the local API that go out after this pass's state.
    answers: api::Answers,
    /// Whether each headset is worn, as last told on the local API.
    worn_told: BTreeMap<[u8; 32], bool>,
}

impl Brain {
    /// Waits for events and deadlines until asked to quit, then ends every session
    /// with a goodbye and puts everything back.
    fn run(mut self, events: &Receiver<Event>) {
        loop {
            let next = brain::earliest([
                self.phase.due(),
                self.closing.iter().map(|c| c.until).min(),
                self.sessions.values().filter_map(Session::volume_at).min(),
                // Live, every pass places; between sessions only a retry wakes it.
                self.displays
                    .as_ref()
                    .filter(|_| self.sessions.is_empty())
                    .and_then(Displays::place_due),
            ]);
            let first = match next {
                // Nothing to do until something happens: no wakeups at all.
                None => events.recv().ok(),
                Some(at) => events
                    .recv_timeout(at.saturating_duration_since(Instant::now()))
                    .ok(),
            };
            if let Some(event) = first {
                self.handle(event);
                // Every event waiting is taken before the next pass, for a short while
                // at most.
                let until = Instant::now() + TAKE_AT_ONCE;
                while !self.quit && Instant::now() < until {
                    match events.try_recv() {
                        Ok(event) => self.handle(event),
                        Err(_) => break,
                    }
                }
            }
            if self.quit {
                break;
            }
            self.pass();
        }
        self.shut_down(events);
    }

    /// One event.
    fn handle(&mut self, event: Event) {
        match event {
            Event::Accepted(accepted) => self.accepted(*accepted),
            Event::Headset(id, came, msg) => {
                let Some(key) = self.key_of(id) else {
                    // From a session that said goodbye or gave way: read and dropped.
                    return;
                };
                if let ClientMsg::Goodbye { reason } = msg {
                    info!("{}", session::headset_left(reason));
                    let why = match prostir_proto::HeadsetGoodbye::from_code(reason) {
                        prostir_proto::HeadsetGoodbye::Quit => api::Reason::Quit,
                        prostir_proto::HeadsetGoodbye::Forgotten => api::Reason::Forgotten,
                        _ => api::Reason::Other,
                    };
                    return self.end_session(key, None, why, Ok(()));
                }
                let (Some(session), Some(displays)) =
                    (self.sessions.get_mut(&key), self.displays.as_mut())
                else {
                    return;
                };
                if session.handle(msg, came, displays, &mut self.store) {
                    self.headsets_changed();
                }
            }
            Event::SessionEnded(id, e) => {
                if let Some(i) = self.closing.iter().position(|c| c.id == id) {
                    // The headset read the goodbye and closed.
                    return self.closing.swap_remove(i).done();
                }
                let Some(key) = self.key_of(id) else {
                    return;
                };
                let why = match e.kind() {
                    ErrorKind::TimedOut => api::Reason::Silence,
                    _ => api::Reason::Other,
                };
                let result = match e.kind() {
                    ErrorKind::UnexpectedEof => Ok(()),
                    ErrorKind::TimedOut => Err(anyhow::anyhow!(
                        "silence: nothing from the headset for {} s",
                        session::SILENCE.as_secs()
                    )),
                    _ => Err(e.into()),
                };
                self.end_session(key, None, why, result);
            }
            Event::Tray(command) => self.tray(command),
            Event::Ctl(request, reply) => self.ctl(request, reply),
            // The next pass shows it.
            Event::Pairing => {}
            Event::Quit => self.quit = true,
        }
    }

    /// The key of the headset whose session is `id`, while it runs.
    fn key_of(&self, id: SessionId) -> Option<[u8; 32]> {
        self.sessions
            .iter()
            .find(|(_, s)| s.id == id)
            .map(|(key, _)| *key)
    }

    /// A connection set up and trusted: its session opens, replacing the headset's
    /// earlier one, unless too many headsets have one.
    fn accepted(&mut self, accepted: Accepted) {
        let Accepted { conn, open, from } = accepted;
        let key = conn.link.peer.key;
        info!(
            "headset connected via {} from {from}",
            session::via_name(conn.link.peer.via)
        );
        // Forgotten after its connection was set up (the tray's item came first).
        if !self.security.trusted.lock().unwrap().contains(&key) {
            info!("the headset from {from} was forgotten meanwhile: turning it away");
            return turn_away(
                conn,
                i18n::t(
                    "Цей ПК більше не довіряє шолому. Сполучіть їх знову.",
                    "This PC no longer trusts this headset. Pair them again.",
                )
                .into(),
            );
        }
        match brain::admit(self.sessions.keys(), &key, brain::max_headsets()) {
            Admit::Open => {}
            Admit::Replace => {
                info!("the same headset connected again: its earlier session gives way");
                if let (Some(old), Some(displays)) =
                    (self.sessions.remove(&key), self.displays.as_mut())
                {
                    // It no longer reads (or soon will not): no goodbye. Its new
                    // session takes its place, and its roles.
                    old.finish(None, displays);
                    self.headset_left(&key, api::Reason::Other);
                }
            }
            Admit::Refuse => {
                warn!("another headset has a session: turning {from} away");
                return turn_away(
                    conn,
                    i18n::t(
                        "До ПК уже підключено інший шолом.",
                        "Another headset is already connected to this PC.",
                    )
                    .into(),
                );
            }
        }
        if self.sessions.is_empty() && self.displays.as_ref().is_some_and(|d| !d.healthy()) {
            self.displays = None;
        }
        if self.displays.is_none() {
            match create_displays(self.set_primary, &mut self.store) {
                Ok(d) => self.displays = Some(d),
                Err(e) => {
                    error!("{e:#}");
                    // Tell the headset why, instead of a snapshot.
                    turn_away(conn, format!("{e:#}"));
                    if self.sessions.is_empty() {
                        self.phase = Phase::Idle;
                    }
                    return;
                }
            }
        }
        // What the new session has of the jobs only one headset can have.
        self.arrivals.connected(key);
        let order = self.in_order(Some((key, self.store.wanted().access_of(&key))));
        let roles = brain::roles(&order, self.roles, self.mic_busy());
        let standing = Standing {
            main: roles.main == Some(key),
            mic: roles.mic == Some(key),
            lead: roles.lead == Some(key),
            no_limits: self.main_no_limits(),
        };
        let others: Vec<_> = self.sessions.values().map(|s| s.caps().clone()).collect();
        let displays = self
            .displays
            .as_mut()
            .expect("made above if it was missing");
        self.last_id += 1;
        match Session::start(
            self.last_id,
            conn,
            open,
            standing,
            &others,
            displays,
            &mut self.store,
            &self.events,
        ) {
            Ok(mut session) => {
                session.set_answers(self.answers.clone());
                let via = session.api_session().via;
                self.worn_told.insert(key, session.worn());
                self.sessions.insert(key, session);
                self.phase.session_opened(Instant::now());
                self.headsets_changed();
                self.api
                    .send(api::Note::Event(api::Event::HeadsetConnected {
                        headset: prostir_proto::secure::fingerprint(&key),
                        name: self.headset_name(&key),
                        via,
                    }));
            }
            Err(e) => {
                warn!("session ended: {e:#}");
                self.arrivals.left(key);
                // The codec it was fitted to, the encoders and the sound it started:
                // as the sessions left (if any) need them.
                self.sessions_ended(false);
            }
        }
    }

    /// The keys of the headsets with a session and the rights of their lines, in the
    /// order they first connected; with `opening`, one whose session opens now.
    fn in_order(
        &self,
        opening: Option<([u8; 32], prostir_proto::state::Access)>,
    ) -> Vec<([u8; 32], prostir_proto::state::Access)> {
        self.arrivals
            .keys()
            .filter_map(|key| match (self.sessions.get(key), opening) {
                (_, Some((k, access))) if k == *key => Some((k, access)),
                (Some(s), _) => Some((*key, s.configured)),
                (None, _) => None,
            })
            .collect()
    }

    /// The PC's end of the headset microphone is in use: a program records from it,
    /// or the headset whose microphone it is streams it.
    fn mic_busy(&self) -> bool {
        self.displays.as_ref().is_some_and(Displays::recording)
            || self
                .roles
                .mic
                .and_then(|key| self.sessions.get(&key))
                .is_some_and(Session::mic_streams)
    }

    /// "No limits" of the main headset, which the PC's limits follow.
    fn main_no_limits(&self) -> bool {
        self.roles
            .main
            .and_then(|key| self.sessions.get(&key))
            .is_some_and(Session::no_limits)
    }

    /// A session opened or ended, or a headset said something about itself: who has
    /// the jobs only one headset can have, whether the encoders run (while any headset
    /// is worn, with a fresh picture for one put on again), and the PC's end of the
    /// microphone (the switch of the headset whose microphone it takes).
    fn headsets_changed(&mut self) {
        let roles = brain::roles(&self.in_order(None), self.roles, self.mic_busy());
        if roles.mic != self.roles.mic && self.roles.mic.is_some() {
            info!("the PC's end of the headset microphone follows another headset now");
        }
        self.roles = roles;
        let no_limits = self.main_no_limits();
        let Some(displays) = self.displays.as_mut() else {
            return;
        };
        let mut fresh = Vec::new();
        for (key, s) in &mut self.sessions {
            s.set_standing(Standing {
                main: roles.main == Some(*key),
                mic: roles.mic == Some(*key),
                lead: roles.lead == Some(*key),
                no_limits,
            });
            fresh.extend(s.take_fresh());
        }
        let worn = self.sessions.values().any(Session::worn);
        displays.worn(worn, &fresh);
        let mic = roles
            .mic
            .and_then(|key| self.sessions.get(&key))
            .is_some_and(Session::mic_on);
        displays.set_mic(mic);
    }

    /// Shows the state in the tray: of the headsets, the session the tray follows
    /// (the main one, else the first), the microphone of the one the PC takes it from,
    /// and how many others there are.
    fn show(&mut self) {
        let line = |key: Option<[u8; 32]>| {
            key.and_then(|key| self.sessions.get(&key))
                .map(Session::tray_line)
                .unwrap_or_default()
        };
        // The microphone as the headset whose microphone the PC takes has it.
        let lead = TrayLine {
            mic_denied: line(self.roles.mic).mic_denied,
            ..line(self.roles.lead)
        };
        let others = self.sessions.len().saturating_sub(1);
        self.control
            .show(self.displays.as_ref(), &self.store, &lead, others);
    }

    /// The session commands for the headset go to: the main one, else the first.
    fn lead_mut(&mut self) -> Option<&mut Session> {
        let lead = self.roles.lead?;
        self.sessions.get_mut(&lead)
    }

    /// Ends the session of headset `key`: with `goodbye`, saying why. The other
    /// sessions go on.
    fn end_session(
        &mut self,
        key: [u8; 32],
        goodbye: Option<PcGoodbye>,
        why: api::Reason,
        result: Result<()>,
    ) {
        if let Some(taken_off) = self.close_session(key, goodbye, why, result) {
            self.sessions_ended(taken_off);
        }
    }

    /// Ends the sessions of `keys` at once, each with `goodbye` (and why, from
    /// `result`): what the others need is looked at once, after the last.
    fn end_sessions(
        &mut self,
        keys: Vec<[u8; 32]>,
        goodbye: PcGoodbye,
        why: api::Reason,
        result: impl Fn() -> Result<()>,
    ) {
        let mut ended = None;
        for key in keys {
            ended = self
                .close_session(key, Some(goodbye), why, result())
                .or(ended);
        }
        if let Some(taken_off) = ended {
            self.sessions_ended(taken_off);
        }
    }

    /// Closes the session of headset `key` (see [`Session::finish`]), with nothing
    /// else changed for the others yet: whether its headset was taken off, if it had
    /// a session.
    fn close_session(
        &mut self,
        key: [u8; 32],
        goodbye: Option<PcGoodbye>,
        why: api::Reason,
        result: Result<()>,
    ) -> Option<bool> {
        let session = self.sessions.remove(&key)?;
        self.headset_left(&key, why);
        let displays = self.displays.as_mut()?;
        self.arrivals.left(key);
        let taken_off = session.taken_off_for().is_some();
        if let Some(closing) = session.finish(goodbye, displays) {
            self.closing.push(closing);
        }
        match result {
            Ok(()) => info!("headset disconnected"),
            Err(e) => warn!("session ended: {e:#}"),
        }
        Some(taken_off)
    }

    /// Sessions ended (the last of them taken off with `taken_off`): the encoders and
    /// sound as the sessions left need them, in a codec they all decode; without any,
    /// what becomes of the monitors.
    fn sessions_ended(&mut self, taken_off: bool) {
        if !self.sessions.is_empty()
            && let Some(displays) = self.displays.as_mut()
        {
            // The headsets left may all decode the codec chosen again.
            let headsets = displays.router.headsets();
            displays.fit_codec(&headsets);
        }
        self.headsets_changed();
        if self.sessions.is_empty() {
            self.after_the_last_session(taken_off);
        }
    }

    /// No session is left: the monitors stay for a while, or go now when the last
    /// headset was taken off (`taken_off`: it reconnects by itself once worn again)
    /// or they broke.
    fn after_the_last_session(&mut self, taken_off: bool) {
        self.control.no_headset(&self.answers);
        self.roles = brain::Roles::NONE;
        let how = match &self.displays {
            Some(_) if taken_off => {
                info!("removing virtual monitors until the headset is worn again");
                LastEnded::Drop
            }
            Some(d) if d.healthy() => LastEnded::Keep,
            _ => LastEnded::Drop,
        };
        self.phase
            .last_session_ended(how, Instant::now(), self.linger);
        match self.phase {
            Phase::Lingering { .. } => {
                info!("keeping virtual monitors for {} s", self.linger.as_secs());
            }
            _ => self.drop_displays(),
        }
        self.show();
    }

    /// Removes the monitors and gives their memory back.
    fn drop_displays(&mut self) {
        if self.displays.take().is_some() {
            platform::give_memory_back();
        }
    }

    /// An item of the tray.
    fn tray(&mut self, command: TrayCommand) {
        match command {
            TrayCommand::SetBitrate(mbps) => {
                info!("video quality: {mbps} Mbit/s per monitor");
                self.store.set_bitrate(mbps);
            }
            TrayCommand::SetSharp(sharp) => {
                info!("clearer text: {sharp}");
                self.store.change(|s| s.sharp = sharp);
            }
            TrayCommand::SetCounting(on) => {
                info!("counting this PC among users: {on}");
                self.store.change(|s| s.count = on);
            }
            TrayCommand::Headset(asked) => {
                self.control.for_headset.push_back((asked, None));
                if self.sessions.is_empty() {
                    self.control.no_headset(&self.answers);
                }
            }
            TrayCommand::ShowSettings => {
                if let Err(e) = local::open_settings() {
                    warn!("{e}");
                }
            }
            TrayCommand::ForgetHeadsets => {
                let mut trusted = self.security.trusted.lock().unwrap();
                info!("forgetting {} paired headset(s)", trusted.len());
                if let Err(e) = trusted.clear() {
                    warn!("cannot forget headsets: {e}");
                }
                drop(trusted);
                let keys: Vec<[u8; 32]> = self.sessions.keys().copied().collect();
                self.end_sessions(
                    keys,
                    PcGoodbye::Forgotten,
                    api::Reason::Forgotten,
                    || Ok(()),
                );
            }
            TrayCommand::Quit => self.quit = true,
        }
        if let Some(d) = self.displays.as_mut() {
            d.apply(&self.store.wanted());
        }
    }

    /// What changed since the last pass goes out: to the headsets, the desktop, the
    /// tray and the files. Live, the clocks' work too.
    fn pass(&mut self) {
        let now = Instant::now();
        // Sessions that said goodbye and whose headset did not close in time.
        let (over, waiting) = std::mem::take(&mut self.closing)
            .into_iter()
            .partition(|c| c.until <= now);
        self.closing = waiting;
        over.into_iter().for_each(Closing::done);
        if self.phase.expire(now) {
            info!("the headset did not come back: removing virtual monitors");
            self.drop_displays();
        }
        if self.sessions.is_empty() {
            if let Some(d) = self.displays.as_mut() {
                d.place();
            }
        } else {
            self.live_pass(now);
        }
        self.show();
        self.store.flush();
        self.publish_api();
    }

    /// A pass with sessions: the headsets told what changed (the replies after it), the
    /// desktop laid out once (which can take it a while: the headsets know already),
    /// told again; the clocks' work.
    fn live_pass(&mut self, now: Instant) {
        let Some(displays) = self.displays.as_mut() else {
            return;
        };
        let started = Instant::now();
        for s in self.sessions.values_mut() {
            s.publish(displays, &self.store, self.focus);
        }
        displays.place();
        for s in self.sessions.values_mut() {
            s.volume_due();
            s.publish(displays, &self.store, self.focus);
        }
        self.store.flush();
        let busy = started.elapsed();
        if busy >= BUSY {
            info!(
                "the control loop was busy for {} ms telling the headset and laying out the desktop",
                busy.as_millis()
            );
        }
        // Commands for the headset go to the main one, else the first.
        let mut waiting = std::mem::take(&mut self.control.for_headset);
        if let Some(s) = self.lead_mut() {
            s.commands_due(&mut waiting);
        }
        self.control.for_headset = waiting;
        let Some(displays) = self.displays.as_mut() else {
            return;
        };
        let Phase::Live(clocks) = &mut self.phase else {
            return;
        };
        if clocks.focus(now) {
            self.focus = displays.active_output();
            for s in self.sessions.values_mut() {
                s.look_at_sound(displays);
            }
        }
        if !clocks.housekeeping(now) {
            return;
        }
        displays.tick(&mut || self.store.new_id());
        // A display of the PC seen for the first time is kept.
        keep_screens(&mut self.store, displays, false);
        let healthy = displays.healthy();
        // The microphone may be free to go to the main headset now.
        self.headsets_changed();
        self.show();
        // Each headset off the head for long enough counts as gone; the others go on.
        let taken_off: Vec<[u8; 32]> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.taken_off_for() >= Some(TAKEN_OFF_GRACE))
            .map(|(key, _)| *key)
            .collect();
        if !taken_off.is_empty() {
            info!("headset taken off");
            self.end_sessions(
                taken_off,
                PcGoodbye::TakenOff,
                api::Reason::TakenOff,
                || Ok(()),
            );
        }
        let Some(displays) = self.displays.as_mut() else {
            return;
        };
        if !healthy && !self.sessions.is_empty() {
            let why = "a virtual monitor or encoder stopped on the PC";
            warn!("{why}");
            displays.router.send_all(HostMsg::Problem {
                error: ErrorCode::ENCODER,
                detail: why.into(),
                display: None,
            });
            let keys: Vec<[u8; 32]> = self.sessions.keys().copied().collect();
            self.end_sessions(keys, PcGoodbye::Failed, api::Reason::Other, || {
                Err(anyhow::anyhow!("virtual monitor or encoder stopped"))
            });
        }
    }

    /// Asked to quit: every session ends with a goodbye, which each headset gets a
    /// moment to read; the monitors go, the files are written.
    fn shut_down(mut self, events: &Receiver<Event>) {
        let keys: Vec<[u8; 32]> = self.sessions.keys().copied().collect();
        self.end_sessions(keys, PcGoodbye::Quit, api::Reason::Quit, || Ok(()));
        info!("quitting");
        // Whoever asked to quit hears it was done.
        self.publish_api();
        while let Some(until) = self.closing.iter().map(|c| c.until).max() {
            let left = until.saturating_duration_since(Instant::now());
            match events.recv_timeout(left) {
                Ok(Event::SessionEnded(id, _)) => {
                    if let Some(i) = self.closing.iter().position(|c| c.id == id) {
                        self.closing.swap_remove(i).done();
                    }
                }
                // What else comes is too late now.
                Ok(_) => {}
                Err(_) => break,
            }
        }
        self.closing.drain(..).for_each(Closing::done);
        if let Some(d) = self.displays.as_mut() {
            d.note_connected();
            keep_screens(&mut self.store, d, false);
        }
        drop(self.displays.take());
        self.store.close();
    }
}

/// Tells a headset why it gets no session, instead of a snapshot, and closes its
/// connection gently (on a thread of its own: that takes a moment).
fn turn_away(mut conn: pairing::Connection, text: String) {
    let spawned = std::thread::Builder::new()
        .name("turn-away".into())
        .spawn(move || {
            let _ = conn.send(HostMsg::Error { text });
            pairing::close_gently(&mut conn.link);
        });
    if let Err(e) = spawned {
        warn!("cannot tell the headset why: {e}");
    }
}

fn spawn_discovery(port: u16) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let beacon = prostir_proto::encode_beacon(port, &platform::hostname());
    std::thread::Builder::new()
        .name("discovery".into())
        .spawn(move || {
            loop {
                // 255.255.255.255 only leaves through the default route, so also send to the
                // broadcast address of every interface (Wi-Fi, Ethernet, hotspot).
                let mut targets = platform::broadcast_addresses();
                targets.push(Ipv4Addr::BROADCAST);
                for ip in targets {
                    let _ = socket.send_to(&beacon, (ip, prostir_proto::DISCOVERY_PORT));
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })?;
    Ok(())
}

fn create_displays(set_primary: bool, store: &mut Store) -> Result<Displays> {
    let wanted = store.wanted();
    let (backend, codec) = encoder::pick(wanted.codec, wanted.software)?;
    info!("encoding with {backend:?} {codec:?}");
    let cfg = EncoderConfig {
        codec,
        bitrate_kbps: wanted.bitrate * 1000,
        backend,
        sharp: wanted.sharp,
    };
    let set = store.screens().clone();
    let displays = Displays::create(&set, &wanted, cfg, set_primary, &mut || store.new_id())?;
    keep_screens(store, &displays, false);
    Ok(displays)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let quiet = || {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    };
    match cli.command {
        None => serve(cli.serve),
        Some(Sub::Serve(args)) => serve(args),
        Some(Sub::Ctl { args }) => {
            quiet();
            let client = platform::local_api::Client::connect(ctl::WAIT);
            let client = client.as_ref().map(|c| c as &dyn ctl::CtlClient);
            let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
            let mut io = ctl::Output {
                out: &mut out,
                err: &mut err,
            };
            std::process::exit(ctl::run(&args, client.map_err(Clone::clone), &mut io));
        }
        Some(Sub::Open) => {
            quiet();
            let code = match platform::local_api::Client::connect(ctl::WAIT) {
                Ok(client) => ctl::open(&client, &local::start_detached),
                Err(e) => {
                    eprintln!("prostir open: no session bus ({e:?})");
                    ctl::NOT_RUNNING
                }
            };
            std::process::exit(code);
        }
    }
}

/// `prostir serve`: waits for the headset and serves it until asked to quit.
fn serve(args: Args) -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let (events, received) = events::channel();
    // A signal from here on quits the usual way, putting everything back.
    {
        let events = events.clone();
        platform::quit_on_signals(move |_| {
            events.send(Event::Quit);
        });
    }
    let _instance = platform::single_instance()?;
    // The command line holds for this run only.
    let run = store::Run {
        bitrate: args.bitrate,
        codec: args.codec.map(|c| match c {
            CodecArg::H264 => Codec::H264,
            CodecArg::H265 => Codec::H265,
        }),
        software: args.software.then_some(true),
        screens: None,
    };
    let mut store = Store::open(store::config_dir(), run);
    store.start_with(&args.monitors);
    // The first start says that this build counts its users and how to turn it off,
    // a day before the first count.
    usage::announce_once(store.wanted().count);
    // Monitors from a run that crashed: without the headset nobody sees their windows.
    platform::clean_up_after_crash();

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, args.port))
        .with_context(|| format!("cannot listen on port {}", args.port))?;
    // The one listened on: with `--port 0` the system picks a free one (tests).
    let port = listener.local_addr()?.port();
    info!(
        "Prostir {} waiting for the headset on port {port}",
        prostir_proto::app_version(env!("CARGO_PKG_VERSION")),
    );
    if !args.no_discovery {
        spawn_discovery(port)?;
    }
    if !args.no_adb {
        platform::keep_adb_reverse(port);
    }

    let security = Arc::new(pairing::Security::load()?);
    let tray = tray::spawn(events.clone());
    // The local API: requests come in as events, what the control loop says goes out.
    let (outlet, notes) = api::channel();
    pairing::tell_about_codes(events.clone());
    listen::spawn(listener, security.clone(), tray.clone(), events.clone())?;
    let mut control = Control {
        tray,
        shown: None,
        shown_settings: None,
        shown_battery: None,
        for_headset: VecDeque::new(),
    };
    control.show(None, &store, &TrayLine::default(), 0);
    let mut brain = Brain {
        linger: Duration::from_secs(args.linger),
        set_primary: !args.keep_primary,
        store,
        control,
        security,
        events,
        displays: None,
        sessions: BTreeMap::new(),
        roles: brain::Roles::NONE,
        arrivals: brain::Arrivals::default(),
        closing: Vec::new(),
        phase: Phase::Idle,
        last_id: 0,
        focus: None,
        quit: false,
        api: outlet,
        shown: None,
        revision: 0,
        answers: api::Answers::default(),
        worn_told: BTreeMap::new(),
    };
    // The local API has a state to give from the moment it is on the bus.
    brain.publish_api();
    let api = platform::local_api::serve(brain.events.clone(), brain.api.clone(), notes);
    // On the bus as early as the state can be: GStreamer's start (a scan of its
    // plugins, the first time) comes after.
    gstreamer::init().context("GStreamer init")?;
    if args.open_settings
        && let Err(e) = local::open_settings()
    {
        warn!("{e}");
    }
    brain.run(&received);
    if let Some(api) = api {
        api.finish();
    }
    Ok(())
}
