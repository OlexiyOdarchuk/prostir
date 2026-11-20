// Every unsafe block says why it is sound.
#![deny(clippy::undocumented_unsafe_blocks)]
mod brain;
mod ctl_hook;
mod displays;
mod encoder;
mod errors;
mod events;
mod i18n;
mod layout;
mod listen;
mod outbox;
mod pacing;
mod pairing;
mod platform;
mod publish;
mod requests;
mod screen;
mod session;
mod store;
mod usage;

use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use log::{error, info, warn};
use prostir_proto::request::{Asked, Command};
use prostir_proto::state::ErrorCode;
use prostir_proto::{ClientMsg, Codec, HostMsg, PcGoodbye};

use brain::{Admit, LastEnded, Phase};
use displays::Displays;
use encoder::EncoderConfig;
use events::{Accepted, Event, EventSink, SessionId};
use platform::MonitorSpec;
use platform::{TrayCommand, TrayStatus, focus, tray};
use screen::ScreenId;
use session::{Closing, Session};
use store::Store;

#[derive(Parser, Debug)]
#[command(about = "Virtual monitors for KDE Plasma and Hyprland, streamed to a Meta Quest 3")]
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
    /// How the headset is connected, while one is.
    via: Option<&'static str>,
    /// The headset's "No limits", while one that takes commands is connected.
    no_limits: Option<bool>,
    /// The connected headset's microphone is on but the system does not let it record.
    mic_denied: bool,
    /// Commands for the headset from the tray and the test hook, and who waits for
    /// what came of each.
    for_headset: VecDeque<(Asked<Command>, Option<Sender<String>>)>,
}

impl Control {
    /// Commands for the headset with no headset to take them.
    fn no_headset(&mut self) {
        for (asked, reply) in self.for_headset.drain(..) {
            info!("{asked:?}: no headset is connected");
            if let Some(r) = reply {
                let _ = r.send("no headset".into());
            }
        }
    }

    /// Shows the current state and settings in the tray (only what changed).
    fn show(&mut self, displays: Option<&Displays>, store: &Store) {
        let status = TrayStatus {
            no_limits: self.no_limits,
            mic_denied: self.mic_denied,
            connected: self.via,
            monitors: displays.map_or(0, Displays::monitor_count),
            mirror: displays.is_some_and(Displays::mirror_active),
            paused: displays.is_some_and(|d| !d.streaming()),
        };
        if self.shown.as_ref() != Some(&status) {
            tray::set_status(self.tray.as_ref(), status.clone());
            self.shown = Some(status);
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
    /// The sessions, by the headset's key: one each (see `brain::admit`).
    sessions: BTreeMap<[u8; 32], Session>,
    /// Sessions that said goodbye, until their headset closes or their moment ends.
    closing: Vec<Closing>,
    phase: Phase,
    last_id: SessionId,
    /// The output that has the mouse, as last looked at.
    focus: Option<ScreenId>,
    active_output: focus::ActiveOutput,
    quit: bool,
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
                    return self.end_session(key, None, Ok(()));
                }
                let (Some(session), Some(displays)) =
                    (self.sessions.get_mut(&key), self.displays.as_mut())
                else {
                    return;
                };
                session.handle(msg, came, displays, &mut self.control, &mut self.store);
            }
            Event::SessionEnded(id, e) => {
                if let Some(i) = self.closing.iter().position(|c| c.id == id) {
                    // The headset read the goodbye and closed.
                    return self.closing.swap_remove(i).done();
                }
                let Some(key) = self.key_of(id) else {
                    return;
                };
                let result = match e.kind() {
                    ErrorKind::UnexpectedEof => Ok(()),
                    ErrorKind::TimedOut => Err(anyhow::anyhow!(
                        "silence: nothing from the headset for {} s",
                        session::SILENCE.as_secs()
                    )),
                    _ => Err(e.into()),
                };
                self.end_session(key, None, result);
            }
            Event::Tray(command) => self.tray(command),
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
        match brain::admit(self.sessions.keys(), &key) {
            Admit::Open => {}
            Admit::Replace => {
                info!("the same headset connected again: its earlier session gives way");
                if let (Some(old), Some(displays)) =
                    (self.sessions.remove(&key), self.displays.as_mut())
                {
                    // It no longer reads (or soon will not): no goodbye.
                    old.finish(None, displays, &mut self.control);
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
        let displays = self
            .displays
            .as_mut()
            .expect("made above if it was missing");
        self.last_id += 1;
        match Session::start(
            self.last_id,
            conn,
            open,
            displays,
            &mut self.control,
            &mut self.store,
            &self.events,
        ) {
            Ok(session) => {
                self.sessions.insert(key, session);
                self.phase.session_opened(Instant::now());
            }
            Err(e) => {
                warn!("session ended: {e:#}");
                if self.sessions.is_empty() {
                    self.after_the_last_session();
                }
            }
        }
    }

    /// Ends the session of headset `key`: with `goodbye`, saying why.
    fn end_session(&mut self, key: [u8; 32], goodbye: Option<PcGoodbye>, result: Result<()>) {
        let (Some(session), Some(displays)) = (self.sessions.remove(&key), self.displays.as_mut())
        else {
            return;
        };
        if let Some(closing) = session.finish(goodbye, displays, &mut self.control) {
            self.closing.push(closing);
        }
        match result {
            Ok(()) => info!("headset disconnected"),
            Err(e) => warn!("session ended: {e:#}"),
        }
        if self.sessions.is_empty() {
            self.after_the_last_session();
        }
    }

    /// No session is left: the monitors stay for a while, or go now when the headset
    /// was taken off (it reconnects by itself once worn again) or they broke.
    fn after_the_last_session(&mut self) {
        self.control.no_headset();
        let how = match &self.displays {
            Some(d) if d.taken_off_for().is_some() => {
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
        self.control.show(self.displays.as_ref(), &self.store);
    }

    /// Removes the monitors and gives their memory back.
    fn drop_displays(&mut self) {
        if self.displays.take().is_some() {
            platform::give_memory_back();
        }
    }

    /// An item of the tray (or a command from the test hook).
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
            TrayCommand::Headset { asked, reply } => {
                self.control.for_headset.push_back((asked, reply));
                if self.sessions.is_empty() {
                    self.control.no_headset();
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
                for key in keys {
                    self.end_session(key, Some(PcGoodbye::Forgotten), Ok(()));
                }
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
        self.control.show(self.displays.as_ref(), &self.store);
        self.store.flush();
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
            s.volume_due(displays);
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
        // Commands for the headset go to the main one.
        if let Some(s) = self.sessions.values_mut().find(|s| s.is_main()) {
            s.commands_due(displays, &mut self.control);
        } else if let Some(s) = self.sessions.values_mut().next() {
            s.commands_due(displays, &mut self.control);
        }
        let Phase::Live(clocks) = &mut self.phase else {
            return;
        };
        if clocks.focus(now) {
            self.focus = self
                .active_output
                .get()
                .and_then(|name| displays.id_of_output(&name));
            for s in self.sessions.values_mut() {
                s.look_at_sound(displays);
            }
        }
        if !clocks.housekeeping(now) {
            return;
        }
        self.control.show(Some(displays), &self.store);
        displays.tick(&mut || self.store.new_id());
        // A display of the PC seen for the first time is kept.
        keep_screens(&mut self.store, displays, false);
        if displays.taken_off_for() >= Some(TAKEN_OFF_GRACE) {
            info!("headset taken off");
            let main: Vec<[u8; 32]> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.is_main())
                .map(|(key, _)| *key)
                .collect();
            for key in main {
                self.end_session(key, Some(PcGoodbye::TakenOff), Ok(()));
            }
        } else if !displays.healthy() {
            let why = "a virtual monitor or encoder stopped on the PC";
            warn!("{why}");
            displays.router.send(HostMsg::Problem {
                error: ErrorCode::ENCODER,
                detail: why.into(),
                display: None,
            });
            let keys: Vec<[u8; 32]> = self.sessions.keys().copied().collect();
            for key in keys {
                let failed = anyhow::anyhow!("virtual monitor or encoder stopped");
                self.end_session(key, Some(PcGoodbye::Failed), Err(failed));
            }
        }
    }

    /// Asked to quit: every session ends with a goodbye, which each headset gets a
    /// moment to read; the monitors go, the files are written.
    fn shut_down(mut self, events: &Receiver<Event>) {
        let keys: Vec<[u8; 32]> = self.sessions.keys().copied().collect();
        for key in keys {
            self.end_session(key, Some(PcGoodbye::Quit), Ok(()));
        }
        info!("quitting");
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

/// Keeps `adb reverse` set up so a headset on USB reaches the host via 127.0.0.1.
fn spawn_adb_reverse(port: u16) {
    let _ = std::thread::Builder::new()
        .name("adb-reverse".into())
        .spawn(move || {
            let spec = format!("tcp:{port}");
            let mut active: Vec<String> = Vec::new();
            loop {
                // The first call starts adb's server, which takes a few seconds.
                let out = match platform::run(
                    std::process::Command::new("adb").arg("devices"),
                    Duration::from_secs(15),
                ) {
                    Ok(out) => out,
                    Err(e) if e.kind() == ErrorKind::NotFound => return, // no adb installed
                    Err(_) => {
                        std::thread::sleep(Duration::from_secs(3));
                        continue;
                    }
                };
                // Only USB devices: "serial\tdevice"; adb-over-Wi-Fi serials are "ip:port".
                let usb: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(|l| l.strip_suffix("\tdevice"))
                    .filter(|serial| !serial.contains(':'))
                    .map(str::to_owned)
                    .collect();
                for serial in &usb {
                    let ok = platform::run(
                        std::process::Command::new("adb")
                            .args(["-s", serial, "reverse", &spec, &spec]),
                        Duration::from_secs(5),
                    )
                    .is_ok_and(|out| out.status.success());
                    if ok && !active.contains(serial) {
                        info!("USB headset {serial}: adb reverse {spec} active");
                    }
                }
                active = usb;
                std::thread::sleep(Duration::from_secs(3));
            }
        });
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
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
    gstreamer::init().context("GStreamer init")?;

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
        spawn_adb_reverse(port);
    }

    let security = Arc::new(pairing::Security::load()?);
    let tray = tray::spawn(events.clone());
    ctl_hook::spawn(events.clone());
    listen::spawn(listener, security.clone(), tray.clone(), events.clone())?;
    let mut control = Control {
        tray,
        shown: None,
        shown_settings: None,
        via: None,
        no_limits: None,
        mic_denied: false,
        for_headset: VecDeque::new(),
    };
    control.show(None, &store);
    Brain {
        linger: Duration::from_secs(args.linger),
        set_primary: !args.keep_primary,
        store,
        control,
        security,
        events,
        displays: None,
        sessions: BTreeMap::new(),
        closing: Vec::new(),
        phase: Phase::Idle,
        last_id: 0,
        focus: None,
        active_output: focus::ActiveOutput::new(),
        quit: false,
    }
    .run(&received);
    Ok(())
}
