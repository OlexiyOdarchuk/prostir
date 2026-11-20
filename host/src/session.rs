//! One headset's session on the PC: its connection's two threads (the writer, which
//! paces what goes out, and the reader, which answers what is instant and hands the
//! rest to the control loop as events), what the headset said and was told, and the
//! watchers its preferences ask for. The control loop (`main.rs`) owns every session
//! and calls into it; nothing here waits for the headset.
//!
//! Each session is one headset's, next to the others: its own outbox, what it was
//! told, whether it is worn. What only one headset can have (the desktop's
//! arrangement, the PC's volume, the PC's end of the microphone, "No limits" for the
//! PC's limits) the control loop gives one session at a time (see [`Standing`]).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{debug, info, warn};
use prostir_proto::link::{Closer, Link, Via};
use prostir_proto::request::{Asked, Command, Outcome};
use prostir_proto::state::{
    self, Access, ErrorCode, HeadsetState, Part, ReqId, UpFormat, UpKind, caps, clip, permission,
    perms, role,
};
use prostir_proto::stats::Period;
use prostir_proto::volume::{Act, PcVolume};
use prostir_proto::{ClientMsg, HeadsetGoodbye, HostMsg, PcGoodbye};

use crate::api::{self, Answers, ApiError, CtlReply, Reply};
use crate::displays::Displays;
use crate::events::{Event, EventSink, SessionId};
use crate::platform::audio::SinkVolume;
use crate::platform::{notifications, sound_source};
use crate::router::Router;
use crate::screen::ScreenId;
use crate::store::Store;
use crate::{i18n, keep_screens, outbox, pacing, pairing, platform, publish, requests};

/// Longest notification passed to the headset, in bytes.
const MAX_NOTICE: usize = 8 * 1024;

/// How often the writer looks at what TCP measures, while it sends.
const LOOK_AT_THE_LINK: Duration = Duration::from_millis(20);
/// How often the writer logs how it paced.
const PACING_REPORT_EVERY: Duration = Duration::from_secs(10);

/// After a goodbye, how long the headset has to read it and close the connection.
const GOODBYE_WAIT: Duration = Duration::from_secs(1);

/// Nothing at all from the headset for this long (it pings every second): the session
/// is over.
pub const SILENCE: Duration = Duration::from_secs(5);

/// A command the headset does not answer within this is given up (a late answer is
/// let go).
const COMMAND_ANSWER: Duration = Duration::from_secs(30);

/// Bytes a second in Mbit/s, for the log.
fn mbit(bytes_per_sec: u64) -> u64 {
    bytes_per_sec * 8 / 1_000_000
}

/// What the headset's goodbye means, for the log.
pub fn headset_left(reason: u8) -> &'static str {
    match HeadsetGoodbye::from_code(reason) {
        HeadsetGoodbye::Quit => "the headset app was closed",
        HeadsetGoodbye::OtherPc => "the headset chose another PC",
        HeadsetGoodbye::Forgotten => "the headset forgot this PC",
        HeadsetGoodbye::Other => "the headset ended the session",
    }
}

/// How the tray names the way the headset is connected.
pub fn via_name(via: Via) -> &'static str {
    match via {
        Via::Usb => "USB",
        Via::Network => "Wi-Fi",
    }
}

/// Ends the session once the writer thread stops, however it stops: the outbox takes
/// nothing more and the connection closes, so the reader and the headset notice at once.
/// After a goodbye the connection stays open for the control loop to close.
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

/// What a session has of what only one headset can have at a time, as the control
/// loop gives it (see `brain::roles`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Standing {
    /// The main session: its arrangement is the desktop's, its volume the PC's, and its
    /// "No limits" the PC's limits.
    pub main: bool,
    /// Its microphone is the PC's end of the headset microphone.
    pub mic: bool,
    /// The tray shows this headset, commands go to it, and it says when its battery
    /// is low: the main session, else the first.
    pub lead: bool,
    /// "No limits" of the main session, for the PC's limits.
    pub no_limits: bool,
}

/// What the tray shows of a headset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayLine {
    /// How it is connected.
    pub via: Option<&'static str>,
    /// Its "No limits", where it takes commands.
    pub no_limits: Option<bool>,
    /// Its microphone is on, but its system does not let it record.
    pub mic_denied: bool,
    /// Its battery: percent and whether it charges.
    pub battery: Option<(u8, bool)>,
}

/// The role and rights a headset has in its session: those of its line in
/// `settings.txt` (`configured`), but the main role only in the main session.
fn access_in(configured: Access, main: bool) -> Access {
    Access {
        role: if main && configured.is_main() {
            role::MAIN
        } else {
            role::GUEST
        },
        perms: configured.perms,
    }
}

/// One session's state on the PC: what the headset said and was told, the watchers
/// its preferences ask for, and the replies waiting for this pass's state to go first.
pub struct Session {
    pub id: SessionId,
    publisher: publish::Publisher,
    /// The role and rights of its line in `settings.txt`.
    pub configured: Access,
    standing: Standing,
    outbox: Arc<outbox::Outbox>,
    closer: Closer,
    writer: Option<JoinHandle<()>>,
    /// How the headset is connected, for the tray.
    via: &'static str,
    /// When the headset was taken off (as last reported by it).
    taken_off_at: Option<Instant>,
    /// Streams the headset needs a fresh picture of, put on again.
    fresh: Vec<ScreenId>,
    /// The PC's volume is shared with this headset.
    shares_volume: bool,
    /// PC notifications passed to the headset, while it asks for them.
    notifications: Option<notifications::Watcher>,
    /// Which monitor the sound comes from, while the headset wants to know.
    sound_source: Option<sound_source::Watcher>,
    /// The monitor the sound comes from, as last looked at.
    sound_from: Option<ScreenId>,
    battery_warned: bool,
    /// Its microphone is on, but its system does not let it record.
    mic_denied: bool,
    /// Replies to send once the parts they changed went out.
    replies: Vec<HostMsg>,
    /// Commands sent to the headset without an answer yet: what each was, who waits
    /// for what came of it, and since when.
    commands: BTreeMap<ReqId, (String, Option<Reply>, Instant)>,
    /// Where the answers to them wait for the control loop's pass (see `set_answers`).
    answers: Answers,
    last_command: u32,
    /// The headset's sound device and the logic of one volume, where both can.
    volume: Option<(SinkVolume, PcVolume)>,
    /// Up streams the headset started: by number, what each carries and in how many
    /// channels.
    ups: BTreeMap<u8, (UpKind, u8)>,
}

/// A session that said goodbye: the headset has a moment to read it and close the
/// connection, and what it still sends is read and dropped meanwhile, so a reset
/// cannot throw the goodbye away.
pub struct Closing {
    pub id: SessionId,
    /// Closed by then whatever the headset does.
    pub until: Instant,
    closer: Closer,
    writer: Option<JoinHandle<()>>,
}

impl Closing {
    /// Closes the connection and waits for the writer, which has nothing left to send.
    pub fn done(mut self) {
        self.closer.close();
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

/// Does what the volume logic says: asks the headset (through `outbox`) or puts the
/// device at the headset's level.
fn do_volume(sink: &SinkVolume, act: Option<Act>, outbox: &outbox::Outbox) {
    match act {
        Some(Act::Ask(asked)) => {
            // At most every 50 ms while a slider moves.
            info!("volume: asking the headset for {}", asked.level);
            outbox.control(HostMsg::VolumeAsk { asked });
        }
        Some(Act::Write { level, muted }) => sink.set(level, muted),
        None => {}
    }
}

/// Interleaved 16-bit stereo as mono: each pair of samples is their mean.
fn to_mono(stereo: &[u8]) -> Vec<u8> {
    stereo
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|[l0, l1, r0, r1]| {
            let l = i32::from(i16::from_le_bytes([*l0, *l1]));
            let r = i32::from(i16::from_le_bytes([*r0, *r1]));
            (((l + r) / 2) as i16).to_le_bytes()
        })
        .collect()
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

/// The writer thread: the only one sending, packet after packet, most important first,
/// pacing the connection by what it measures of the network.
fn spawn_writer(
    outbox: Arc<outbox::Outbox>,
    mut sender: Box<dyn prostir_proto::link::RecordSender>,
    closer: Closer,
    via: Via,
) -> Result<JoinHandle<()>> {
    Ok(std::thread::Builder::new()
        .name("net-writer".into())
        .spawn(move || {
            let mut end = EndSession {
                outbox: outbox.clone(),
                closer: Some(closer),
            };
            let mut lanes = outbox::Lanes::default();
            let mut pacer = pacing::Pacer::new(via);
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
            // until it closes the connection is still read (see `Closing`).
            if outbox.said_goodbye() {
                let _ = sender.finish();
                end.closer = None;
            }
        })?)
}

/// The reader thread: what is instant is answered here, not in the control loop
/// (which may wait on a desktop tool for a while), so the round trip is the
/// network's; the rest goes to the control loop as events of session `id`.
fn spawn_reader(
    id: SessionId,
    mut receiver: Box<dyn prostir_proto::link::RecordReceiver>,
    mut packets: prostir_proto::packet::Assembler,
    outbox: Arc<outbox::Outbox>,
    router: Arc<Router>,
    events: EventSink,
) -> Result<()> {
    receiver.set_idle_limit(Some(SILENCE));
    std::thread::Builder::new()
        .name("net-reader".into())
        .spawn(move || {
            loop {
                let msg = match ClientMsg::recv(&mut *receiver, &mut packets) {
                    Ok(msg) => msg,
                    Err(e) => {
                        events.send(Event::SessionEnded(id, e));
                        return;
                    }
                };
                match msg {
                    ClientMsg::Ping { seq } => {
                        outbox.control(HostMsg::Pong { seq });
                    }
                    ClientMsg::Pong { seq } => outbox.pong(seq),
                    // Also here: how long a frame took to show is the headset's and the
                    // network's time, not the control loop's.
                    ClientMsg::FrameShown {
                        id: display,
                        generation,
                        seq,
                    } => router.frame_shown(id, ScreenId::from_wire(display), generation, seq),
                    msg => {
                        if !events.send(Event::Headset(id, Instant::now(), msg)) {
                            return;
                        }
                    }
                }
            }
        })?;
    Ok(())
}

impl Session {
    /// Opens session `id` on a connection set up and trusted, with what it has of the
    /// jobs only one headset can have (`standing`): the snapshot first, then the writer
    /// and the reader, and every stream starts over for this headset. The streams go
    /// in a codec this headset and those of the other sessions (`others`) decode.
    #[expect(
        clippy::too_many_arguments,
        reason = "what a session starts from, each from its own owner"
    )]
    pub fn start(
        id: SessionId,
        conn: pairing::Connection,
        open: Vec<Part>,
        standing: Standing,
        others: &[state::HeadsetCaps],
        displays: &mut Displays,
        store: &mut Store,
        events: &EventSink,
    ) -> Result<Self> {
        let pairing::Connection {
            link:
                Link {
                    sender,
                    receiver,
                    closer,
                    peer,
                },
            mut packets,
        } = conn;
        let via = via_name(peer.via);
        // What the headset said about itself; without it there is no session.
        let headset = HeadsetState::from_open(&open).context("the headset's Open")?;
        let configured = store.wanted().access_of(&peer.key);
        let main = standing.main && configured.is_main();
        let standing = Standing {
            main,
            // The main session's own.
            no_limits: if main {
                headset.prefs.unwrap_or_default().no_limits
            } else {
                standing.no_limits
            },
            ..standing
        };
        let mut publisher = publish::Publisher::new(headset, access_in(configured, main));
        let mut headsets = others.to_vec();
        headsets.push(publisher.caps().clone());
        displays.fit_codec(&headsets);
        let outbox = Arc::new(outbox::Outbox::default());
        // The session opens now that it can be served: the monitors exist, and the
        // writer (which also pings) starts below. The headset counts silence from
        // here. The snapshot is the first message, before any stream can start.
        // The headset's sound device, where the PC can follow its volume: with the main
        // headset only (a guest's volume does not move the PC's, nor is it asked to).
        let sink = SinkVolume::open().filter(|_| main);
        let shares_volume = sink.is_some();
        let snapshot = publisher.snapshot(&publish::Sources {
            displays,
            wanted: &store.wanted(),
            focus: None,
            sound_from: None,
            volume: shares_volume,
            mic: standing.mic,
            no_limits: standing.no_limits,
        })?;
        outbox.control(snapshot);
        packets.set_limits(ClientMsg::limits(true));
        let writer = spawn_writer(outbox.clone(), sender, closer.clone(), peer.via)?;
        let reader = spawn_reader(
            id,
            receiver,
            packets,
            outbox.clone(),
            displays.router.clone(),
            events.clone(),
        );
        if let Err(e) = reader {
            outbox.close();
            closer.close();
            let _ = writer.join();
            return Err(e);
        }

        let worn = worn(&publisher.headset);
        displays
            .router
            .attach(id, outbox.clone(), publisher.caps().clone(), worn);
        // Streams too big for every headset stand failed in the first `Displays` after
        // the snapshot, before anything of them could go.
        displays.stop_too_big();
        // The streams the snapshot listed may start (each starts over just below).
        let listed = publisher.streamable().map(ScreenId::from_wire).collect();
        displays.router.publish(id, &listed);
        displays.headset_connected();
        let mut session = Session {
            id,
            publisher,
            configured,
            standing,
            outbox,
            closer,
            writer: Some(writer),
            via,
            taken_off_at: (!worn).then(Instant::now),
            fresh: Vec::new(),
            shares_volume,
            notifications: None,
            sound_source: None,
            sound_from: None,
            battery_warned: false,
            mic_denied: false,
            replies: Vec::new(),
            commands: BTreeMap::new(),
            answers: Answers::default(),
            last_command: 0,
            volume: None,
            ups: BTreeMap::new(),
        };
        // One volume for the PC's slider and the headset's buttons, where both can.
        if session.publisher.caps().has(caps::headset::VOLUME) {
            session.volume = sink.map(|sink| (sink, PcVolume::default()));
        }
        // The first headset brings its level to the PC; one that comes while others
        // listen takes the PC's instead, so their sound does not jump.
        let opened_with = session.publisher.headset.volume;
        match opened_with {
            Some(v) if others.is_empty() => session.volume_from_headset(v),
            _ => session.take_pc_volume(),
        }
        session.headset_changed(displays);
        session.publish(displays, store, None);
        crate::usage::headset_connected(store.wanted().count);
        Ok(session)
    }

    /// The headset's own "No limits".
    pub fn no_limits(&self) -> bool {
        self.publisher.prefs().no_limits
    }

    /// Its microphone's switch is on (it reaches the PC only in the session the
    /// control loop gives the microphone, and with the right to it).
    pub fn mic_on(&self) -> bool {
        self.publisher
            .headset
            .up
            .get(&UpKind::Mic)
            .is_some_and(|m| m.switch)
    }

    /// The headset streams its microphone now.
    pub fn mic_streams(&self) -> bool {
        self.ups.values().any(|(kind, _)| *kind == UpKind::Mic)
    }

    /// What the headset can do.
    pub fn caps(&self) -> &state::HeadsetCaps {
        self.publisher.caps()
    }

    /// On the head (a headset that never said counts as worn).
    pub fn worn(&self) -> bool {
        self.taken_off_at.is_none()
    }

    /// How long the headset has been off the head, if it is.
    pub fn taken_off_for(&self) -> Option<Duration> {
        self.taken_off_at.map(|t| t.elapsed())
    }

    /// The streams this headset needs a fresh picture of since the last call (it was
    /// put on again).
    pub fn take_fresh(&mut self) -> Vec<ScreenId> {
        std::mem::take(&mut self.fresh)
    }

    /// Where the answers to commands go: they wait for the end of the control loop's
    /// pass, so a reply never goes out before the state that shows what it changed.
    pub fn set_answers(&mut self, answers: Answers) {
        self.answers = answers;
    }

    /// What the PC allows this headset, as its limits say now.
    pub fn limits(&self, displays: &Displays) -> state::Limits {
        self.publisher.limits(displays, self.standing.no_limits)
    }

    /// The session as the local API shows it.
    pub fn api_session(&self) -> api::Session {
        let headset = &self.publisher.headset;
        let line = self.tray_line();
        let mic = headset.up.get(&UpKind::Mic);
        api::Session {
            via: match self.via {
                "USB" => api::Via::Usb,
                _ => api::Via::Wifi,
            },
            worn: self.worn(),
            battery: line
                .battery
                .map(|(percent, charging)| api::Battery { percent, charging }),
            profile: None,
            mic_allowed: mic.and_then(|m| match m.permission {
                permission::GRANTED => Some(true),
                permission::DENIED => Some(false),
                _ => None,
            }),
            mic_on: self.mic_on(),
            main: self.standing.main,
            commands: self.publisher.caps().has(caps::headset::COMMANDS),
            no_limits: line.no_limits,
        }
    }

    /// The headset's volume, while the PC's follows it.
    pub fn shared_volume(&self) -> Option<api::Volume> {
        self.volume.as_ref()?;
        let v = self.publisher.headset.volume?;
        Some(api::Volume {
            level: v.level,
            muted: v.muted,
        })
    }

    /// What the tray shows of this headset.
    pub fn tray_line(&self) -> TrayLine {
        let headset = &self.publisher.headset;
        let mic = headset.up.get(&UpKind::Mic);
        TrayLine {
            via: Some(self.via),
            no_limits: self
                .publisher
                .caps()
                .has(caps::headset::COMMANDS)
                .then(|| self.no_limits()),
            mic_denied: mic.is_some_and(|m| m.switch && m.permission == permission::DENIED),
            battery: headset.battery.map(|b| (b.percent, b.charging)),
        }
    }

    /// What the session has now of the jobs only one headset can have. Made the main
    /// one, it takes the PC's volume over (from its own level); no longer the main
    /// one, its volume is its own again. Its `Access` and the PC's capabilities go out
    /// with the next pass.
    pub fn set_standing(&mut self, standing: Standing) {
        let main = standing.main && self.configured.is_main();
        let standing = Standing { main, ..standing };
        if standing == self.standing {
            return;
        }
        let was_main = self.standing.main;
        self.standing = standing;
        self.publisher.access = access_in(self.configured, main);
        if main == was_main {
            return;
        }
        info!(
            "session {}: {}",
            self.id,
            if main {
                "the main headset now"
            } else {
                "a guest now"
            }
        );
        let sink = SinkVolume::open().filter(|_| main);
        self.shares_volume = sink.is_some();
        self.volume = sink
            .filter(|_| self.publisher.caps().has(caps::headset::VOLUME))
            .map(|sink| (sink, PcVolume::default()));
        // The PC's sound does not jump to this headset's level: the headset is asked
        // to take the PC's.
        self.take_pc_volume();
    }

    /// The headset is asked to take the level the PC's device has now, where the PC
    /// knows it (the device does not move).
    fn take_pc_volume(&mut self) {
        let Some((sink, pc)) = &mut self.volume else {
            return;
        };
        if let Some((level, muted)) = sink.level() {
            let act = pc.device(level, muted, Instant::now());
            do_volume(sink, act, &self.outbox);
        }
    }

    /// Ends the session. With `goodbye` the headset is told why and gets a moment to
    /// read it (the [`Closing`] returned); without, the connection closes now. The
    /// other sessions go on: only this one's headset leaves the router.
    pub fn finish(
        mut self,
        goodbye: Option<PcGoodbye>,
        displays: &mut Displays,
    ) -> Option<Closing> {
        self.end();
        displays.router.detach(self.id);
        let closing = Closing {
            id: self.id,
            until: Instant::now() + GOODBYE_WAIT,
            closer: self.closer.clone(),
            writer: self.writer.take(),
        };
        match goodbye {
            Some(reason) => {
                info!("saying goodbye to the headset ({reason:?})");
                self.outbox.say_goodbye(reason);
                Some(closing)
            }
            None => {
                self.outbox.close();
                closing.done();
                None
            }
        }
    }

    /// Sends the commands waiting for the headset; gives up on those it did not answer
    /// in time.
    pub fn commands_due(&mut self, waiting: &mut VecDeque<(Asked<Command>, Option<Reply>)>) {
        let takes = self.publisher.caps().has(caps::headset::COMMANDS);
        for (asked, reply) in waiting.drain(..) {
            let what = format!("{asked:?}");
            if !takes {
                info!("{what}: the headset takes no commands");
                if let Some(r) = reply {
                    self.answers.push(
                        r,
                        CtlReply::failed(
                            ApiError::Unsupported,
                            i18n::t("Шолом не виконує команд", "The headset takes no commands"),
                        ),
                    );
                }
                continue;
            }
            // One that cannot go as it is gets an answer, and the session goes on.
            if let Asked::Known(command) = &asked
                && let Err(why) = crate::local::fits_the_wire(command)
            {
                warn!("{what}: {}", why.text);
                if let Some(r) = reply {
                    self.answers.push(r, CtlReply::Failed(why));
                }
                continue;
            }
            self.last_command += 1;
            let req = ReqId(self.last_command);
            info!("command {} for the headset: {what}", req.0);
            self.outbox.control(HostMsg::Command { req, asked });
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
                    self.answers.push(
                        r,
                        CtlReply::failed(
                            ApiError::TimedOut,
                            i18n::t("Шолом не відповідає", "The headset does not answer"),
                        ),
                    );
                }
            }
        }
    }

    /// The session is over: commands still waiting get no answer.
    fn end(&mut self) {
        for (_, (what, reply, _)) in std::mem::take(&mut self.commands) {
            info!("{what}: the headset left");
            if let Some(r) = reply {
                self.answers.push(
                    r,
                    CtlReply::failed(
                        ApiError::Disconnected,
                        i18n::t("Шолом відключився", "The headset disconnected"),
                    ),
                );
            }
        }
    }

    /// A volume from the headset (in `Open` or since): the main headset's goes to the
    /// device. The device's own changes come first: one not read yet makes this level
    /// an old one, which must not be written while the slider moves.
    fn volume_from_headset(&mut self, v: prostir_proto::state::Volume) {
        if !self.standing.main {
            return;
        }
        self.device_changes();
        if let Some((sink, pc)) = &mut self.volume {
            let act = pc.from_headset(&v, Instant::now());
            do_volume(sink, act, &self.outbox);
        }
    }

    /// The device's changes since the last look, each at the moment it happened: they
    /// ask the headset.
    fn device_changes(&mut self) {
        let Some((sink, pc)) = &mut self.volume else {
            return;
        };
        while let Some((level, muted, at)) = sink.changed() {
            let act = pc.device(level, muted, at);
            do_volume(sink, act, &self.outbox);
        }
    }

    /// Follows the device's volume: changes ask the headset, its answers go to the
    /// device once the slider rests.
    pub fn volume_due(&mut self) {
        self.device_changes();
        let Some((sink, pc)) = &mut self.volume else {
            return;
        };
        let act = pc.tick(Instant::now());
        do_volume(sink, act, &self.outbox);
    }

    /// When the volume has something to do next.
    pub fn volume_at(&self) -> Option<Instant> {
        self.volume.as_ref().and_then(|(_, pc)| pc.due())
    }

    /// Looks again which monitor the sound comes from, while the headset wants to know.
    pub fn look_at_sound(&mut self, displays: &Displays) {
        if let Some(w) = &self.sound_source {
            let id = w.output().and_then(|key| displays.id_of_output(&key));
            if self.sound_from != id {
                info!("sound comes from {id:?}");
            }
            self.sound_from = id;
        }
    }

    /// Tells the headset what changed, then the replies.
    pub fn publish(&mut self, displays: &mut Displays, store: &Store, focus: Option<ScreenId>) {
        let sources = publish::Sources {
            displays,
            wanted: &store.wanted(),
            focus,
            sound_from: self.sound_from,
            volume: self.shares_volume,
            mic: self.standing.mic,
            no_limits: self.standing.no_limits,
        };
        for msg in self.publisher.changed(&sources) {
            self.outbox.control(msg);
        }
        let listed = self
            .publisher
            .streamable()
            .map(ScreenId::from_wire)
            .collect();
        for id in displays.router.publish(self.id, &listed) {
            displays.fresh_picture(id);
        }
        for reply in self.replies.drain(..) {
            self.outbox.control(reply);
        }
    }

    /// Follows what the headset said about itself: worn or not (frames and sound go
    /// to it only while worn; the control loop pauses the desktop once no headset is),
    /// its battery, and the watchers its preferences ask for (notifications only for a
    /// headset with the right to private things).
    fn headset_changed(&mut self, displays: &mut Displays) {
        let access = self.publisher.access;
        let headset = &self.publisher.headset;
        let worn = worn(headset);
        self.taken_off_at = match (worn, self.taken_off_at) {
            (true, _) => None,
            (false, Some(t)) => Some(t),
            (false, None) => Some(Instant::now()),
        };
        let fresh = displays.router.set_worn(self.id, worn);
        self.fresh.extend(fresh);
        let denied = self.tray_line().mic_denied;
        if denied && !self.mic_denied {
            warn!("the headset's microphone has no permission: the system does not let it record");
        }
        self.mic_denied = denied;
        if let Some(b) = headset.battery
            && self.standing.lead
            // Once per session, when it gets low.
            && b.percent <= 15
            && !b.charging
            && !self.battery_warned
        {
            self.battery_warned = true;
            notify_battery(b.percent);
        }
        let prefs = self.publisher.prefs();
        self.notifications = if prefs.notifications && access.may(perms::PRIVATE) {
            self.notifications.take().or_else(|| {
                let outbox = self.outbox.clone();
                notifications::watch(move |text| {
                    let text = clip(&text, MAX_NOTICE);
                    outbox.control(HostMsg::Notice { text });
                })
            })
        } else {
            None
        };
        self.sound_source = prefs.sound_from_screen.then(|| {
            self.sound_source
                .take()
                .unwrap_or_else(|| sound_source::Watcher::start(displays.window_list()))
        });
    }

    /// One message from the headset. Returns whether the headset's state changed
    /// (worn or not, its microphone, its preferences), which the control loop looks at
    /// for every session.
    pub fn handle(
        &mut self,
        msg: ClientMsg,
        came: Instant,
        displays: &mut Displays,
        store: &mut Store,
    ) -> bool {
        let volume = match &msg {
            ClientMsg::Volume { part } => Some(*part),
            _ => None,
        };
        let msg = match self.publisher.headset.apply(msg) {
            None => {
                if let Some(v) = volume {
                    self.volume_from_headset(v);
                }
                self.headset_changed(displays);
                return true;
            }
            Some(msg) => msg,
        };
        match msg {
            // The microphone, from a headset with the right to it.
            ClientMsg::UpStart { up, kind, format } => {
                let pcm = match format {
                    UpFormat::Pcm16 { rate, channels } if rate == prostir_proto::AUDIO_RATE => {
                        Some(channels)
                    }
                    _ => None,
                };
                match pcm {
                    _ if kind != UpKind::Mic || !self.publisher.access.may(perms::MIC) => {
                        info!("an up stream of {kind:?} this PC does not take");
                    }
                    Some(channels @ (1 | 2)) => {
                        info!("the headset's microphone streams ({format:?}, up stream {up})");
                        // A new recording replaces the one before, whose end may still
                        // be on its way: that end then ends nothing.
                        self.ups.retain(|_, (k, _)| *k != UpKind::Mic);
                        self.ups.insert(up, (kind, channels));
                    }
                    _ => {
                        warn!("the headset's microphone in {format:?}, which this PC does not take")
                    }
                }
            }
            // Only the microphone of the session that has it reaches the PC.
            ClientMsg::Up { up, data } => match self.ups.get(&up) {
                _ if !self.standing.mic => {}
                // The PC's end of the microphone is mono.
                Some((UpKind::Mic, 2)) => displays.push_mic(to_mono(&data)),
                Some((UpKind::Mic, _)) => displays.push_mic(data),
                _ => {}
            },
            ClientMsg::UpEnd { up } => {
                if self.ups.remove(&up).is_some() {
                    info!("the headset's microphone stopped (up stream {up})");
                }
            }
            ClientMsg::RequestKeyframe { id, generation } => {
                let id = ScreenId::from_wire(id);
                if displays.router.is_current(self.id, id, generation) {
                    displays.keyframe_asked(id);
                } else {
                    info!(
                        "{id}: a keyframe asked for {generation:?}, not the generation it \
                         streams now: none made"
                    );
                }
            }
            ClientMsg::Request { req, asked } => {
                let started = Instant::now();
                let what = format!("{asked:?}");
                let outcome = match asked {
                    Asked::Known(request) => {
                        let limits = self.publisher.limits(displays, self.standing.no_limits);
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
                info!(
                    "request {} ({what}): waited {} ms, done in {} ms",
                    req.0,
                    (started - came).as_millis(),
                    started.elapsed().as_millis()
                );
                self.replies.push(HostMsg::Reply { req, outcome });
            }
            // Only the main headset's arrangement is the desktop's.
            ClientMsg::Arrange { rows } if self.standing.main => {
                displays.set_layout(&rows);
                keep_screens(store, displays, false);
            }
            ClientMsg::Arrange { .. } => info!("a guest's arrangement leaves the desktop as it is"),
            ClientMsg::MouseTo { id, u, v } if self.publisher.access.may(perms::INPUT) => {
                displays.mouse_to(ScreenId::from_wire(id), u, v);
            }
            ClientMsg::MouseTo { .. } => info!("the headset has no right to move the mouse"),
            // Used for priorities from step 10 on.
            ClientMsg::Gaze { .. } => {}
            ClientMsg::Reply { req, outcome } => match self.commands.remove(&req) {
                Some((what, reply, _)) => {
                    let result = CtlReply::of_outcome(&outcome);
                    info!("command {} ({what}): {result:?}", req.0);
                    if let Some(r) = reply {
                        self.answers.push(r, result);
                    }
                }
                None => info!("an answer to command {}, which nothing waits for", req.0),
            },
            other => warn!("ignoring {other:?}"),
        }
        false
    }
}

/// Whether the headset is on the head, as it last said (one that never said counts as
/// worn).
fn worn(headset: &HeadsetState) -> bool {
    headset.presence.is_none_or(|p| p.worn)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stereo microphone reaches the PC's mono end as the mean of its channels.
    #[test]
    fn stereo_becomes_mono() {
        let frame = |l: i16, r: i16| [l.to_le_bytes(), r.to_le_bytes()].concat();
        let stereo = [frame(100, 300), frame(-32768, -32768), frame(32767, -32767)].concat();
        let mono: Vec<i16> = to_mono(&stereo)
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(mono, [200, -32768, 0]);
        assert!(to_mono(&[1, 2, 3]).is_empty(), "no half frame");
    }
}
