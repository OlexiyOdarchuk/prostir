//! One headset's session on the PC: its connection's two threads (the writer, which
//! paces what goes out, and the reader, which answers what is instant and hands the
//! rest to the control loop as events), what the headset said and was told, and the
//! watchers its preferences ask for. The control loop (`main.rs`) owns every session
//! and calls into it; nothing here waits for the headset.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{debug, info, warn};
use prostir_proto::link::{Closer, Link, Via};
use prostir_proto::request::{Asked, Outcome};
use prostir_proto::state::{
    ErrorCode, HeadsetState, Part, ReqId, UpFormat, UpKind, caps, clip, permission, perms,
};
use prostir_proto::stats::Period;
use prostir_proto::volume::{Act, PcVolume};
use prostir_proto::{ClientMsg, HeadsetGoodbye, HostMsg, PcGoodbye};

use crate::displays::Displays;
use crate::events::{Event, EventSink, SessionId};
use crate::platform::audio::SinkVolume;
use crate::platform::{notifications, sound_source, tray};
use crate::screen::ScreenId;
use crate::store::Store;
use crate::{Control, i18n, keep_screens, outbox, pacing, pairing, platform, publish, requests};

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

/// One session's state on the PC: what the headset said and was told, the watchers
/// its preferences ask for, and the replies waiting for this pass's state to go first.
pub struct Session {
    pub id: SessionId,
    publisher: publish::Publisher,
    outbox: Arc<outbox::Outbox>,
    closer: Closer,
    writer: Option<JoinHandle<()>>,
    /// The PC's volume is shared with this headset.
    shares_volume: bool,
    /// PC notifications passed to the headset, while it asks for them.
    notifications: Option<notifications::Watcher>,
    /// Which monitor the sound comes from, while the headset wants to know.
    sound_source: Option<sound_source::Watcher>,
    /// The monitor the sound comes from, as last looked at.
    sound_from: Option<ScreenId>,
    battery_warned: bool,
    /// Replies to send once the parts they changed went out.
    replies: Vec<HostMsg>,
    /// Commands sent to the headset without an answer yet: what each was, who waits
    /// for what came of it, and since when.
    commands: BTreeMap<ReqId, (String, Option<std::sync::mpsc::Sender<String>>, Instant)>,
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

/// Does what the volume logic says: asks the headset (`displays` routes it) or puts
/// the device at the headset's level.
fn do_volume(sink: &SinkVolume, act: Option<Act>, displays: Option<&Displays>) {
    match act {
        Some(Act::Ask(asked)) => {
            if let Some(d) = displays {
                // At most every 50 ms while a slider moves.
                info!("volume: asking the headset for {}", asked.level);
                d.router.send(HostMsg::VolumeAsk { asked });
            }
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

/// What came of a command, as the one who asked reads it.
fn command_result(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Failed { error, detail } => format!("failed {}: {detail}", error.code()),
        _ => "done".into(),
    }
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
    router: Arc<crate::displays::Router>,
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
                    } => router.frame_shown(ScreenId::from_wire(display), generation, seq),
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
    /// Opens session `id` on a connection set up and trusted: the snapshot first, then
    /// the writer and the reader, and every stream starts over for this headset.
    pub fn start(
        id: SessionId,
        conn: pairing::Connection,
        open: Vec<Part>,
        displays: &mut Displays,
        control: &mut Control,
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
        let mut publisher = publish::Publisher::new(headset, store.wanted().access_of(&peer.key));
        displays.fit_codec(publisher.caps());
        let outbox = Arc::new(outbox::Outbox::default());
        // The session opens now that it can be served: the monitors exist, and the
        // writer (which also pings) starts below. The headset counts silence from
        // here. The snapshot is the first message, before any stream can start.
        // The headset's sound device, where the PC can follow its volume: with the main
        // headset only (a guest's volume does not move the PC's, nor is it asked to).
        let sink = SinkVolume::open().filter(|_| publisher.access.is_main());
        let shares_volume = sink.is_some();
        let snapshot = publisher.snapshot(&publish::Sources {
            displays,
            wanted: &store.wanted(),
            focus: None,
            sound_from: None,
            volume: shares_volume,
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

        control.via = Some(via);
        displays
            .router
            .attach(outbox.clone(), publisher.caps().clone());
        // Streams too big for this headset stand failed in the first `Displays` after
        // the snapshot, before anything of them could go.
        displays.stop_too_big();
        // The streams the snapshot listed may start (each starts over just below).
        let listed = publisher.listed().map(ScreenId::from_wire).collect();
        displays.router.publish(&listed);
        displays.headset_connected();
        let mut session = Session {
            id,
            publisher,
            outbox,
            closer,
            writer: Some(writer),
            shares_volume,
            notifications: None,
            sound_source: None,
            sound_from: None,
            battery_warned: false,
            replies: Vec::new(),
            commands: BTreeMap::new(),
            last_command: 0,
            volume: None,
            ups: BTreeMap::new(),
        };
        // One volume for the PC's slider and the headset's buttons, where both can.
        if session.publisher.caps().has(caps::headset::VOLUME) {
            session.volume = sink.map(|sink| (sink, PcVolume::default()));
        }
        let opened_with = session.publisher.headset.volume;
        if let Some(v) = opened_with {
            session.volume_from_headset(v, displays);
        }
        session.headset_changed(displays, control);
        session.publish(displays, store, None);
        crate::usage::headset_connected(store.wanted().count);
        Ok(session)
    }

    /// This headset's access is the main one (its arrangement is the desktop's).
    pub fn is_main(&self) -> bool {
        self.publisher.access.is_main()
    }

    /// Ends the session. With `goodbye` the headset is told why and gets a moment to
    /// read it (the [`Closing`] returned); without, the connection closes now.
    pub fn finish(
        mut self,
        goodbye: Option<PcGoodbye>,
        displays: &mut Displays,
        control: &mut Control,
    ) -> Option<Closing> {
        displays.set_streaming(false);
        tray::set_battery(control.tray.as_ref(), None);
        self.end(control);
        displays.router.detach();
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
    /// in time; shows its "No limits" in the tray.
    pub fn commands_due(&mut self, displays: &Displays, control: &mut Control) {
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
        control.mic_denied = false;
        for (_, (what, reply, _)) in std::mem::take(&mut self.commands) {
            info!("{what}: the headset left");
            if let Some(r) = reply {
                let _ = r.send("the headset disconnected".into());
            }
        }
    }

    /// A volume from the headset (in `Open` or since): the main headset's goes to the
    /// device. The device's own changes come first: one not read yet makes this level
    /// an old one, which must not be written while the slider moves.
    fn volume_from_headset(&mut self, v: prostir_proto::state::Volume, displays: &Displays) {
        if !self.publisher.access.is_main() {
            return;
        }
        self.device_changes(displays);
        if let Some((sink, pc)) = &mut self.volume {
            let act = pc.from_headset(&v, Instant::now());
            do_volume(sink, act, Some(displays));
        }
    }

    /// The device's changes since the last look, each at the moment it happened: they
    /// ask the headset.
    fn device_changes(&mut self, displays: &Displays) {
        let Some((sink, pc)) = &mut self.volume else {
            return;
        };
        while let Some((level, muted, at)) = sink.changed() {
            let act = pc.device(level, muted, at);
            do_volume(sink, act, Some(displays));
        }
    }

    /// Follows the device's volume: changes ask the headset, its answers go to the
    /// device once the slider rests.
    pub fn volume_due(&mut self, displays: &Displays) {
        self.device_changes(displays);
        let Some((sink, pc)) = &mut self.volume else {
            return;
        };
        let act = pc.tick(Instant::now());
        do_volume(sink, act, Some(displays));
    }

    /// When the volume has something to do next.
    pub fn volume_at(&self) -> Option<Instant> {
        self.volume.as_ref().and_then(|(_, pc)| pc.due())
    }

    /// Looks again which monitor the sound comes from, while the headset wants to know.
    pub fn look_at_sound(&mut self, displays: &Displays) {
        if let Some(w) = &self.sound_source {
            let id = w.output().and_then(|name| displays.id_of_output(&name));
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
    /// pauses the desktop), its battery, its microphone, and the watchers its
    /// preferences ask for (notifications only for a headset with the right to private
    /// things).
    fn headset_changed(&mut self, displays: &mut Displays, control: &mut Control) {
        let access = self.publisher.access;
        let headset = &self.publisher.headset;
        // The PC's end of the microphone is there while the headset's switch is on.
        let mic = headset.up.get(&UpKind::Mic).copied();
        displays.set_mic(mic.is_some_and(|m| m.switch) && access.may(perms::MIC));
        let denied = mic.is_some_and(|m| m.switch && m.permission == permission::DENIED);
        if denied && !control.mic_denied {
            warn!("the headset's microphone has no permission: the system does not let it record");
        }
        control.mic_denied = denied;
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
    pub fn handle(
        &mut self,
        msg: ClientMsg,
        came: Instant,
        displays: &mut Displays,
        control: &mut Control,
        store: &mut Store,
    ) {
        let volume = match &msg {
            ClientMsg::Volume { part } => Some(*part),
            _ => None,
        };
        let msg = match self.publisher.headset.apply(msg) {
            None => {
                if let Some(v) = volume {
                    self.volume_from_headset(v, displays);
                }
                return self.headset_changed(displays, control);
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
            ClientMsg::Up { up, data } => match self.ups.get(&up) {
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
                if displays.router.is_current(id, generation) {
                    displays.keyframe_asked(id);
                }
            }
            ClientMsg::Request { req, asked } => {
                let started = Instant::now();
                let what = format!("{asked:?}");
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
                info!(
                    "request {} ({what}): waited {} ms, done in {} ms",
                    req.0,
                    (started - came).as_millis(),
                    started.elapsed().as_millis()
                );
                self.replies.push(HostMsg::Reply { req, outcome });
                control.show(Some(displays), store);
            }
            // Only the main headset's arrangement is the desktop's.
            ClientMsg::Arrange { rows } if self.publisher.access.is_main() => {
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
