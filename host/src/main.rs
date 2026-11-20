// Every unsafe block says why it is sound.
#![deny(clippy::undocumented_unsafe_blocks)]
mod ctl_hook;
mod displays;
mod encoder;
mod errors;
mod i18n;
mod layout;
mod outbox;
mod pacing;
mod pairing;
mod platform;
mod publish;
mod requests;
mod screen;
mod store;
mod usage;

use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use log::{debug, error, info, warn};
use prostir_proto::link::{Closer, Link, Via};
use prostir_proto::request::{Asked, Command, Outcome};
use prostir_proto::state::{ErrorCode, HeadsetState, Part, ReqId, caps, clip, perms};
use prostir_proto::stats::Period;
use prostir_proto::{ClientMsg, Codec, HeadsetGoodbye, HostMsg, PcGoodbye};

use displays::Displays;
use encoder::EncoderConfig;
use platform::MonitorSpec;
use platform::{TrayCommand, TrayStatus, focus, notifications, sound_source, tray};
use screen::ScreenId;
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

/// Tray icon and the commands it sends.
struct Control {
    tray: Option<tray::Handle>,
    security: pairing::Security,
    commands: Receiver<TrayCommand>,
    quit: bool,
    shown: Option<TrayStatus>,
    /// The settings the tray shows: quality, the text preset, the counter.
    shown_settings: Option<(u32, bool, bool)>,
    /// How the headset is connected, while one is.
    via: Option<&'static str>,
    /// The headset's "No limits", while one that takes commands is connected.
    no_limits: Option<bool>,
    /// Commands for the headset from the tray and the test hook, and who waits for
    /// what came of each.
    for_headset: VecDeque<(Asked<Command>, Option<Sender<String>>)>,
}

impl Control {
    /// Applies tray commands (through the store, then to the screens); returns why a
    /// session must end, if it must (quitting, or the paired headsets forgotten).
    fn poll(&mut self, store: &mut Store, displays: Option<&mut Displays>) -> Option<PcGoodbye> {
        if platform::quit_requested() {
            self.quit = true;
        }
        let mut forgot = false;
        let mut displays = displays;
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                TrayCommand::SetBitrate(mbps) => {
                    info!("video quality: {mbps} Mbit/s per monitor");
                    store.set_bitrate(mbps);
                }
                TrayCommand::SetSharp(sharp) => {
                    info!("clearer text: {sharp}");
                    store.change(|s| s.sharp = sharp);
                }
                TrayCommand::SetCounting(on) => {
                    info!("counting this PC among users: {on}");
                    store.change(|s| s.count = on);
                }
                TrayCommand::Headset { asked, reply } => self.for_headset.push_back((asked, reply)),
                TrayCommand::ForgetHeadsets => {
                    info!(
                        "forgetting {} paired headset(s)",
                        self.security.trusted.len()
                    );
                    if let Err(e) = self.security.trusted.clear() {
                        warn!("cannot forget headsets: {e}");
                    }
                    forgot = true;
                }
                TrayCommand::Quit => self.quit = true,
            }
            if let Some(d) = displays.as_deref_mut() {
                d.apply(&store.wanted());
            }
        }
        if self.quit {
            Some(PcGoodbye::Quit)
        } else {
            forgot.then_some(PcGoodbye::Forgotten)
        }
    }

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

/// Longest notification passed to the headset, in bytes.
const MAX_NOTICE: usize = 8 * 1024;

/// Keeps the set and its rows as they are now (`chosen`: the headset changed which
/// monitors there are).
pub fn keep_screens(store: &mut Store, displays: &Displays, chosen: bool) {
    if let Some(set) = displays.saved_screens() {
        store.set_screens(set, chosen);
    }
}

/// Ends the session once the writer thread stops, however it stops: the outbox takes
/// nothing more and the connection closes, so the reader and the headset notice at once.
/// After a goodbye the connection stays open for the session loop to close.
struct EndSession {
    outbox: Arc<outbox::Outbox>,
    closer: Option<Closer>,
}

impl Drop for EndSession {
    fn drop(&mut self) {
        self.outbox.close();
        if let Some(closer) = &self.closer {
            closer.close();
        }
    }
}

/// How often the writer looks at what TCP measures, while it sends.
const LOOK_AT_THE_LINK: Duration = Duration::from_millis(20);
/// How often the writer logs how it paced.
const PACING_REPORT_EVERY: Duration = Duration::from_secs(10);

/// Bytes a second in Mbit/s, for the log.
fn mbit(bytes_per_sec: u64) -> u64 {
    bytes_per_sec * 8 / 1_000_000
}

/// After a goodbye, how long the headset has to read it and close the connection.
const GOODBYE_WAIT: Duration = Duration::from_secs(1);

/// What the headset's goodbye means, for the log.
fn headset_left(reason: u8) -> &'static str {
    match HeadsetGoodbye::from_code(reason) {
        HeadsetGoodbye::Quit => "the headset app was closed",
        HeadsetGoodbye::OtherPc => "the headset chose another PC",
        HeadsetGoodbye::Forgotten => "the headset forgot this PC",
        HeadsetGoodbye::Other => "the headset ended the session",
    }
}

/// Taken off for this long, the headset counts as gone.
const TAKEN_OFF_GRACE: Duration = Duration::from_secs(10);
/// Nothing at all from the headset for this long (it pings every second): the session
/// is over.
const SILENCE: Duration = Duration::from_secs(5);

/// How the tray names the way the headset is connected.
fn via_name(via: Via) -> &'static str {
    match via {
        Via::Usb => "USB",
        Via::Network => "Wi-Fi",
    }
}

fn serve_client(
    conn: pairing::Connection,
    open: Vec<Part>,
    displays: &mut Displays,
    control: &mut Control,
    store: &mut Store,
) -> Result<()> {
    let pairing::Connection {
        link:
            Link {
                mut sender,
                mut receiver,
                closer,
                peer,
            },
        mut packets,
    } = conn;
    let via = via_name(peer.via);
    // What the headset said about itself; without it there is no session.
    let headset = HeadsetState::from_open(&open).context("the headset's Open")?;
    let mut publisher = publish::Publisher::new(headset, store.wanted().access_of(&peer.key));
    displays.fit_codec(publisher.caps());
    let outbox = Arc::new(outbox::Outbox::default());
    // The session opens now that it can be served: the monitors exist, and the writer
    // (which also pings) starts below. The headset counts silence from here. The
    // snapshot is the first message, before any stream can start.
    let snapshot = publisher.snapshot(&publish::Sources {
        displays,
        wanted: &store.wanted(),
        focus: None,
        sound_from: None,
    })?;
    outbox.control(snapshot);
    packets.set_limits(ClientMsg::limits(true));
    let writer_thread = {
        let closer = closer.clone();
        let outbox = outbox.clone();
        std::thread::Builder::new()
            .name("net-writer".into())
            .spawn(move || {
                let mut end = EndSession {
                    outbox: outbox.clone(),
                    closer: Some(closer),
                };
                // The only one sending: it takes packet after packet, most important first,
                // and paces the connection by what it measures of the network.
                let mut lanes = outbox::Lanes::default();
                let mut pacer = pacing::Pacer::new(peer.via);
                let mut next_look = Instant::now();
                let mut report = Period::new(PACING_REPORT_EVERY);
                // The pace last logged: changes of a quarter or more go into the log.
                let mut logged = 0u64;
                // How long sending waited on the socket since the last look: most of the
                // time means the network (or the pace) held the PC back.
                let mut waited = Duration::ZERO;
                let mut last_look = Instant::now();
                while let Some(pick) = outbox.next(&mut lanes) {
                    let sending = Instant::now();
                    if let Err(e) = lanes.send(pick, &mut *sender) {
                        info!("connection closed while sending: {e}");
                        return;
                    }
                    let now = Instant::now();
                    waited += now - sending;
                    if now < next_look {
                        continue;
                    }
                    next_look = now + LOOK_AT_THE_LINK;
                    let held_back = waited * 2 >= now - last_look;
                    (waited, last_look) = (Duration::ZERO, now);
                    let queued = outbox.take_queued();
                    if let Some(rtt) = outbox.take_round_trip() {
                        pacer.pong(now, rtt);
                    }
                    let Some(link) = sender.link_info() else {
                        continue;
                    };
                    let look = pacing::Look {
                        now,
                        link,
                        held_back,
                        queued,
                    };
                    if let Some(pace) = pacer.look(look) {
                        sender.set_pace(pace);
                        let ms = |d: Duration| d.as_secs_f64() * 1e3;
                        let line = format!(
                            "pacing at {} Mbit/s, {} (the streams need {}; round trip {:.1} ms, \
                             lowest {:.1} ms)",
                            mbit(pace),
                            pacer.phase(),
                            mbit(pacer.floor()),
                            ms(link.rtt),
                            ms(pacer.min_rtt().unwrap_or_default()),
                        );
                        if pace.abs_diff(logged) > logged / 4 {
                            logged = pace;
                            info!("{line}");
                        } else {
                            debug!("{line}");
                        }
                    }
                    if report.due(now).is_some() {
                        info!("pacing: {}", pacer.take_stats().summary());
                    }
                }
                // After the goodbye nothing more goes out, but what the headset sends
                // until it closes the connection is still read (see `close_gently`).
                if outbox.said_goodbye() {
                    let _ = sender.finish();
                    end.closer = None;
                }
            })?
    };

    let (msg_tx, msg_rx) = mpsc::channel::<std::io::Result<ClientMsg>>();
    {
        let outbox = outbox.clone();
        receiver.set_idle_limit(Some(SILENCE));
        std::thread::Builder::new()
            .name("net-reader".into())
            .spawn(move || {
                loop {
                    let msg = ClientMsg::recv(&mut *receiver, &mut packets);
                    // Answered here, not in the session loop (which may wait on a
                    // desktop tool for a while): the round trip is the network's.
                    match msg {
                        Ok(ClientMsg::Ping { seq }) => {
                            outbox.control(HostMsg::Pong { seq });
                            continue;
                        }
                        Ok(ClientMsg::Pong { seq }) => {
                            outbox.pong(seq);
                            continue;
                        }
                        _ => {}
                    }
                    let stop = msg.is_err();
                    if msg_tx.send(msg).is_err() || stop {
                        break;
                    }
                }
            })?;
    }

    control.via = Some(via);
    displays.router.attach(outbox.clone());
    // The streams the snapshot listed may start (each starts over just below).
    let listed = publisher.listed().map(ScreenId::from_wire).collect();
    displays.router.publish(&listed);
    displays.headset_connected();
    let mut session = Session {
        publisher,
        notifications: None,
        sound_source: None,
        battery_warned: false,
        replies: Vec::new(),
        commands: BTreeMap::new(),
        last_command: 0,
    };
    session.headset_changed(displays, control);
    session.publish(displays, store, None, None);
    usage::headset_connected(store.wanted().count);

    // Housekeeping runs on a clock: the headset acknowledges every frame, so the
    // channel is rarely quiet long enough for a receive timeout.
    let mut next_tick = Instant::now();
    // The display under the mouse, for dimming the others in the headset.
    let active_output = focus::ActiveOutput::new();
    let mut sound_from = None;
    let mut focus = None;
    let mut next_focus = Instant::now();
    let (result, goodbye) = loop {
        // What changed in the last pass: the desktop laid out once, the headset told,
        // then the replies, and the files written, off this thread.
        displays.place();
        session.publish(displays, store, focus, sound_from);
        store.flush();
        if Instant::now() >= next_focus {
            next_focus = Instant::now() + Duration::from_millis(150);
            focus = active_output
                .get()
                .and_then(|name| displays.id_of_output(&name));
            if let Some(w) = &session.sound_source {
                let id = w.output().and_then(|name| displays.id_of_output(&name));
                if sound_from != id {
                    info!("sound comes from {id:?}");
                }
                sound_from = id;
            }
        }
        if Instant::now() >= next_tick {
            next_tick = Instant::now() + Duration::from_millis(500);
            if let Some(reason) = control.poll(store, Some(displays)) {
                break (Ok(()), Some(reason));
            }
            session.commands_due(displays, control);
            control.show(Some(displays), store);
            displays.tick(&mut || store.new_id());
            // A display of the PC seen for the first time is kept.
            keep_screens(store, displays, false);
            if displays.taken_off_for() >= Some(TAKEN_OFF_GRACE) {
                info!("headset taken off");
                break (Ok(()), Some(PcGoodbye::TakenOff));
            }
            if !displays.healthy() {
                let why = "a virtual monitor or encoder stopped on the PC";
                warn!("{why}");
                displays.router.send(HostMsg::Problem {
                    error: ErrorCode::ENCODER,
                    detail: why.into(),
                    display: None,
                });
                let failed = anyhow::anyhow!("virtual monitor or encoder stopped");
                break (Err(failed), Some(PcGoodbye::Failed));
            }
        }
        let wait = next_tick
            .min(next_focus)
            .saturating_duration_since(Instant::now());
        match msg_rx.recv_timeout(wait) {
            Ok(Ok(ClientMsg::Goodbye { reason })) => {
                info!("{}", headset_left(reason));
                break (Ok(()), None);
            }
            Ok(Ok(msg)) => session.handle(msg, displays, control, store),
            Ok(Err(e)) if e.kind() == ErrorKind::UnexpectedEof => break (Ok(()), None),
            Ok(Err(e)) if e.kind() == ErrorKind::TimedOut => {
                let silence = anyhow::anyhow!(
                    "silence: nothing from the headset for {} s",
                    SILENCE.as_secs()
                );
                break (Err(silence), None);
            }
            Ok(Err(e)) => break (Err(e.into()), None),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break (Ok(()), None),
        }
    };
    displays.set_streaming(false);
    tray::set_battery(control.tray.as_ref(), None);
    session.end(control);
    displays.router.detach();
    match goodbye {
        Some(reason) => {
            info!("saying goodbye to the headset ({reason:?})");
            outbox.say_goodbye(reason);
            // The headset closes once it read the goodbye; until then what it sends is
            // read and dropped, so a reset cannot throw the goodbye away.
            let until = Instant::now() + GOODBYE_WAIT;
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                if !matches!(msg_rx.recv_timeout(left), Ok(Ok(_))) {
                    break;
                }
            }
        }
        None => outbox.close(),
    }
    closer.close();
    let _ = writer_thread.join();
    result
}

/// One session's state on the PC: what the headset said and was told, the watchers
/// its preferences ask for, and the replies waiting for this pass's state to go first.
struct Session {
    publisher: publish::Publisher,
    /// PC notifications passed to the headset, while it asks for them.
    notifications: Option<notifications::Watcher>,
    /// Which monitor the sound comes from, while the headset wants to know.
    sound_source: Option<sound_source::Watcher>,
    battery_warned: bool,
    /// Replies to send once the parts they changed went out.
    replies: Vec<HostMsg>,
    /// Commands sent to the headset without an answer yet: what each was, who waits
    /// for what came of it, and since when.
    commands: BTreeMap<ReqId, (String, Option<Sender<String>>, Instant)>,
    last_command: u32,
}

/// A command the headset does not answer within this is given up (a late answer is
/// let go).
const COMMAND_ANSWER: Duration = Duration::from_secs(30);

/// What came of a command, as the one who asked reads it.
fn command_result(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Failed { error, detail } => format!("failed {}: {detail}", error.code()),
        _ => "done".into(),
    }
}

impl Session {
    /// Sends the commands waiting for the headset; gives up on those it did not answer
    /// in time; shows its "No limits" in the tray.
    fn commands_due(&mut self, displays: &Displays, control: &mut Control) {
        let takes = self.publisher.caps().has(caps::headset::COMMANDS);
        control.no_limits = takes.then(|| self.publisher.prefs().no_limits);
        for (asked, reply) in control.for_headset.drain(..) {
            let what = format!("{asked:?}");
            if !takes {
                info!("{what}: the headset takes no commands");
                if let Some(r) = reply {
                    let _ = r.send(format!(
                        "failed {}: the headset cannot do this",
                        ErrorCode::UNSUPPORTED.code()
                    ));
                }
                continue;
            }
            self.last_command += 1;
            let req = ReqId(self.last_command);
            info!("command {} for the headset: {what}", req.0);
            displays.router.send(HostMsg::Command { req, asked });
            self.commands.insert(req, (what, reply, Instant::now()));
        }
        let late: Vec<ReqId> = self
            .commands
            .iter()
            .filter(|(_, (_, _, at))| at.elapsed() >= COMMAND_ANSWER)
            .map(|(req, _)| *req)
            .collect();
        for req in late {
            if let Some((what, reply, _)) = self.commands.remove(&req) {
                warn!("command {} ({what}): the headset does not answer", req.0);
                if let Some(r) = reply {
                    let _ = r.send("the headset does not answer".into());
                }
            }
        }
    }

    /// The session is over: commands still waiting get no answer.
    fn end(&mut self, control: &mut Control) {
        control.via = None;
        control.no_limits = None;
        for (_, (what, reply, _)) in std::mem::take(&mut self.commands) {
            info!("{what}: the headset left");
            if let Some(r) = reply {
                let _ = r.send("the headset disconnected".into());
            }
        }
        control.no_headset();
    }

    /// Tells the headset what changed, then the replies.
    fn publish(
        &mut self,
        displays: &mut Displays,
        store: &Store,
        focus: Option<ScreenId>,
        sound_from: Option<ScreenId>,
    ) {
        let sources = publish::Sources {
            displays,
            wanted: &store.wanted(),
            focus,
            sound_from,
        };
        for msg in self.publisher.changed(&sources) {
            displays.router.send(msg);
        }
        let listed = self.publisher.listed().map(ScreenId::from_wire).collect();
        for id in displays.router.publish(&listed) {
            displays.fresh_picture(id);
        }
        for reply in self.replies.drain(..) {
            displays.router.send(reply);
        }
    }

    /// Follows what the headset said about itself: worn or not (the main headset
    /// pauses the desktop), its battery, and the watchers its preferences ask for
    /// (notifications only for a headset with the right to private things).
    fn headset_changed(&mut self, displays: &mut Displays, control: &Control) {
        let access = self.publisher.access;
        let headset = &self.publisher.headset;
        if let Some(p) = headset.presence
            && access.is_main()
        {
            displays.set_worn(p.worn);
        }
        if let Some(b) = headset.battery {
            tray::set_battery(control.tray.as_ref(), Some((b.percent, b.charging)));
            // Once per session, when it gets low.
            if b.percent <= 15 && !b.charging && !self.battery_warned {
                self.battery_warned = true;
                notify_battery(b.percent);
            }
        }
        let prefs = self.publisher.prefs();
        self.notifications = if prefs.notifications && access.may(perms::PRIVATE) {
            self.notifications.take().or_else(|| {
                let router = displays.router.clone();
                notifications::watch(move |text| {
                    let text = clip(&text, MAX_NOTICE);
                    router.send(HostMsg::Notice { text })
                })
            })
        } else {
            None
        };
        self.sound_source = prefs.sound_from_screen.then(|| {
            self.sound_source
                .take()
                .unwrap_or_else(sound_source::Watcher::start)
        });
    }

    /// One message from the headset.
    fn handle(
        &mut self,
        msg: ClientMsg,
        displays: &mut Displays,
        control: &mut Control,
        store: &mut Store,
    ) {
        let msg = match self.publisher.headset.apply(msg) {
            None => return self.headset_changed(displays, control),
            Some(msg) => msg,
        };
        match msg {
            ClientMsg::RequestKeyframe { id, generation } => {
                let id = ScreenId::from_wire(id);
                if displays.router.is_current(id, generation) {
                    displays.keyframe_asked(id);
                }
            }
            ClientMsg::FrameShown {
                id,
                generation,
                seq,
            } => {
                displays
                    .router
                    .frame_shown(ScreenId::from_wire(id), generation, seq);
            }
            ClientMsg::Request { req, asked } => {
                let outcome = match asked {
                    Asked::Known(request) => {
                        let limits = self.publisher.limits(displays);
                        let asker = requests::Asker {
                            caps: self.publisher.caps(),
                            limits: &limits,
                            access: self.publisher.access,
                        };
                        requests::handle(request, displays, store, &asker)
                    }
                    Asked::Unknown { kind } => {
                        info!("the headset asked for something unknown (kind {kind})");
                        Outcome::failed(ErrorCode::UNSUPPORTED, "")
                    }
                };
                if let Outcome::Failed { detail, .. } = &outcome {
                    warn!("request {}: {detail}", req.0);
                }
                self.replies.push(HostMsg::Reply { req, outcome });
                control.show(Some(displays), store);
            }
            // Only the main headset's arrangement is the desktop's.
            ClientMsg::Arrange { rows } if self.publisher.access.is_main() => {
                displays.set_layout(&rows);
                keep_screens(store, displays, false);
            }
            ClientMsg::Arrange { .. } => info!("a guest's arrangement leaves the desktop as it is"),
            ClientMsg::Mic { pcm } => displays.push_mic(pcm),
            ClientMsg::SetMic { enabled } => {
                store.change(|s| s.mic = enabled);
                displays.apply(&store.wanted());
            }
            ClientMsg::MouseTo { id, u, v } if self.publisher.access.may(perms::INPUT) => {
                displays.mouse_to(ScreenId::from_wire(id), u, v);
            }
            ClientMsg::MouseTo { .. } => info!("the headset has no right to move the mouse"),
            // Used for priorities from step 10 on.
            ClientMsg::Gaze { .. } => {}
            ClientMsg::Reply { req, outcome } => match self.commands.remove(&req) {
                Some((what, reply, _)) => {
                    let result = command_result(&outcome);
                    info!("command {} ({what}): {result}", req.0);
                    if let Some(r) = reply {
                        let _ = r.send(result);
                    }
                }
                None => info!("an answer to command {}, which nothing waits for", req.0),
            },
            other => warn!("ignoring {other:?}"),
        }
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

fn create_displays(args: &Args, store: &mut Store) -> Result<Displays> {
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
    let displays = Displays::create(&set, &wanted, cfg, !args.keep_primary, &mut || {
        store.new_id()
    })?;
    keep_screens(store, &displays, false);
    Ok(displays)
}

/// The headset's battery is low: say so on the PC, where the user is looking.
fn notify_battery(percent: u8) {
    let body = format!(
        "{} {percent}%.",
        i18n::t("Заряд шолома:", "Headset battery:")
    );
    platform::dialogs::notify(
        i18n::t("Шолом скоро розрядиться", "The headset battery is low"),
        &body,
    );
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
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
    platform::quit_on_signals();
    // Monitors from a run that crashed: without the headset nobody sees their windows.
    platform::clean_up_after_crash();
    gstreamer::init().context("GStreamer init")?;

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, args.port))
        .with_context(|| format!("cannot listen on port {}", args.port))?;
    listener.set_nonblocking(true)?;
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

    let (command_tx, commands) = mpsc::channel();
    let mut control = Control {
        security: pairing::Security::load()?,
        tray: tray::spawn(command_tx.clone()),
        commands,
        quit: false,
        shown: None,
        shown_settings: None,
        via: None,
        no_limits: None,
        for_headset: VecDeque::new(),
    };
    ctl_hook::spawn(command_tx);
    control.show(None, &store);

    let mut displays: Option<Displays> = None;
    let mut idle_since = Instant::now();
    loop {
        store.flush();
        if control.quit {
            info!("quitting");
            drop(displays);
            store.close();
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, addr)) => {
                if let Err(e) = stream.set_nonblocking(false) {
                    warn!("connection from {addr}: {e}");
                    continue;
                }
                let (mut conn, open) =
                    match pairing::accept(stream, &mut control.security, control.tray.as_ref()) {
                        Ok(accepted) => accepted,
                        Err(e) if e.is::<pairing::TurnedAwayAgain>() => {
                            debug!("rejected connection from {addr}: {e:#}");
                            continue;
                        }
                        Err(e) => {
                            warn!("rejected connection from {addr}: {e:#}");
                            continue;
                        }
                    };
                info!(
                    "headset connected via {} from {addr}",
                    via_name(conn.link.peer.via)
                );
                if displays.as_ref().is_some_and(|d| !d.healthy()) {
                    displays = None;
                }
                if displays.is_none() {
                    match create_displays(&args, &mut store) {
                        Ok(d) => displays = Some(d),
                        Err(e) => {
                            error!("{e:#}");
                            // Tell the headset why, instead of a snapshot.
                            let _ = conn.send(HostMsg::Error {
                                text: format!("{e:#}"),
                            });
                            pairing::close_gently(&mut conn.link);
                            continue;
                        }
                    }
                }
                if let Err(e) = serve_client(
                    conn,
                    open,
                    displays.as_mut().expect("made above if it was missing"),
                    &mut control,
                    &mut store,
                ) {
                    warn!("session ended: {e:#}");
                } else {
                    info!("headset disconnected");
                }
                idle_since = Instant::now();
                // Taken off: give the desktop back to the laptop right away (the
                // headset reconnects by itself once it is worn again).
                if displays
                    .as_ref()
                    .is_some_and(|d| d.taken_off_for().is_some())
                {
                    info!("removing virtual monitors until the headset is worn again");
                    displays = None;
                    platform::give_memory_back();
                }
                control.show(displays.as_ref(), &store);
                if args.linger > 0 && displays.as_ref().is_some_and(|d| d.healthy()) {
                    info!("keeping virtual monitors for {} s", args.linger);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                control.poll(&mut store, displays.as_mut());
                control.no_headset();
                control.show(displays.as_ref(), &store);
                if let Some(d) = displays.as_mut() {
                    d.place();
                }
                if displays.is_some() && idle_since.elapsed() >= Duration::from_secs(args.linger) {
                    displays = None;
                    control.show(None, &store);
                    platform::give_memory_back();
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            // Pending network errors (ENETDOWN, EHOSTUNREACH…) or running out of file
            // descriptors: not a reason to quit.
            Err(e) => {
                warn!("accepting a connection failed: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}
