use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use prostir_proto::handshake::{self, Answer};
use prostir_proto::link::tcp::{NoiseTcp, Plain, SocketOptions};
use prostir_proto::link::{Closer, Link, PeerInfo, RecordReceiver, RecordSender};
use prostir_proto::packet::{self, Assembler, FRAGMENT, lane};
use prostir_proto::request::{Outcome, Request};
use prostir_proto::secure::{Identity, Pairing, TrustStore, confirmation, tags_match};
use prostir_proto::state::{
    Decoder, Display, DisplayId, Encoding, Gen, HeadsetCaps, HeadsetState, PcState, Prefs,
    Presence, ReqId, caps,
};
use prostir_proto::stats::{Gaps, Pings};
use prostir_proto::{ClientMsg, Codec, HostMsg};

use crate::nal;

/// The headset reports a shown frame at most this often per stream, like the Quest app.
const ACK_EVERY: Duration = Duration::from_millis(100);
/// The PC answers within this, and takes what is sent within this.
const REPLY: Duration = Duration::from_secs(10);
/// In a session, nothing at all from the PC for this long ends it, as in the Quest app.
const SILENCE: Duration = Duration::from_secs(5);
/// Pairing waits for a person typing the code on the PC.
const PAIRING: Duration = Duration::from_secs(300);
/// The headset pings the PC this often in a session, like the Quest app.
const PING_EVERY: Duration = Duration::from_secs(1);
/// After a goodbye, how long the PC has to close the connection.
const GOODBYE_WAIT: Duration = Duration::from_secs(1);

/// A headset's long-term key and the PCs it trusts, kept in a directory.
pub struct Keys {
    pub identity: Identity,
    pub trusted: TrustStore,
}

impl Keys {
    /// The keys in `dir` (`identity`, `paired_hosts`), made on first use.
    pub fn load(dir: &Path) -> Result<Self> {
        let identity = Identity::load_or_create(&dir.join("identity"))
            .with_context(|| format!("headset key in {}", dir.display()))?;
        Ok(Self {
            identity,
            trusted: TrustStore::load(&dir.join("paired_hosts")),
        })
    }
}

/// Why there is no session.
#[derive(Debug)]
pub enum ConnectError {
    /// The PC does not know this headset, and no pairing code was given.
    NotPaired,
    /// The PC tried pairing and gave up, or the codes did not match.
    PairingFailed(String),
    /// The PC speaks another protocol version.
    Incompatible {
        protocol: u32,
        app: String,
    },
    /// The PC cannot open the session, and said why.
    Refused(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPaired => write!(f, "the PC does not know this headset"),
            Self::Refused(why) => write!(f, "the PC cannot open the session: {why}"),
            Self::PairingFailed(why) => write!(f, "pairing failed: {why}"),
            Self::Incompatible { protocol, app } => {
                write!(f, "the PC speaks protocol v{protocol} (app {app})")
            }
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<std::io::Error> for ConnectError {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.into())
    }
}

impl From<anyhow::Error> for ConnectError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// One stream as the headset sees it: a monitor, a window panel or a display of the PC.
#[derive(Debug, Clone)]
pub struct Stream {
    /// The display's name, as `Displays` had it when the stream started.
    pub name: String,
    pub generation: Gen,
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    /// `StreamStart`s for this id in this session (it starts over with a new encoder).
    pub starts: u32,
    /// Since the last start.
    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    /// Bytes of the keyframes among them, and of the biggest one.
    pub keyframe_bytes: u64,
    pub keyframe_max: u64,
    pub first_is_key: Option<bool>,
    /// Keyframes a decoder could not start from (no parameter sets or no IDR).
    pub bad_keyframes: u64,
    /// The number of the last frame; frames of a generation count from 0.
    pub last_seq: Option<u32>,
}

impl Stream {
    /// A decoder would show a picture: a good keyframe came since the last start.
    pub fn has_picture(&self) -> bool {
        self.keyframes > self.bad_keyframes
    }
}

/// A message from the PC, in short, in the order it came: for tests of the order of
/// messages within a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// `Displays` with these displays.
    Displays(Vec<DisplayId>),
    Reply(ReqId),
    StreamStart(DisplayId, Gen),
    StreamEnd(DisplayId, Gen),
    /// The first frame after a start.
    FirstFrame(DisplayId, Gen),
    Other(&'static str),
}

/// What the PC told the headset in this session.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// The PC's state: the snapshot, then every part since.
    pub pc: PcState,
    /// Streams running now, by display.
    pub streams: BTreeMap<DisplayId, Stream>,
    /// Streams the PC ended, in order.
    pub ended: Vec<DisplayId>,
    /// Displays that left `Displays` in this session.
    pub removed: Vec<DisplayId>,
    /// Replies to requests, by number.
    pub replies: BTreeMap<ReqId, Outcome>,
    /// What went wrong, as the PC said: failed replies, problems, errors (the detail).
    pub errors: Vec<String>,
    pub notices: Vec<String>,
    /// How many times `Encoding` came, to tell an answer from what it said before.
    pub encoding_count: u32,
    /// The PC asks for the microphone.
    pub mic_active: Option<bool>,
    /// Bytes of PC sound received.
    pub audio_bytes: u64,
    /// How late each slice of sound arrived, in order: from when the fake desktop made
    /// it (the time it starts with) to when it was read here. Only a PC with the fake
    /// desktop stamps its sound so; from any other PC these numbers mean nothing.
    pub sound_delays: Vec<Duration>,
    /// Pauses between slices of sound arriving.
    pub sound_gaps: Gaps,
    /// Pings from the PC (each answered at once).
    pub pings: u32,
    /// The PC ended the session on purpose, with this reason code.
    pub goodbye: Option<u8>,
    /// Round trips of this headset's pings, as their pongs came.
    pub round_trips: Vec<Duration>,
    pub messages: u64,
    /// The PC closed the connection.
    pub closed: bool,
    /// Why reading stopped: `UnexpectedEof` when the PC closed the connection,
    /// `TimedOut` after silence.
    pub end: Option<std::io::ErrorKind>,
    /// Slices of sound that came in while a frame was half way in: they went between
    /// its packets.
    pub sound_between_fragments: u32,
    /// What came, in order.
    pub arrivals: Vec<Arrival>,
    /// Broken rules: a stream started for a display not in `Displays`, a frame of a
    /// generation that never started, a frame number out of order.
    pub broken: Vec<String>,
    /// The displays of the last `Displays`.
    listed: Vec<DisplayId>,
}

impl View {
    fn apply(&mut self, arrived: &Arrived) {
        self.messages += 1;
        let msg = arrived.msg.clone();
        let msg = match self.pc.apply(msg) {
            Ok(None) => {
                self.part(&arrived.msg);
                self.round_trips.extend(arrived.round_trip);
                return;
            }
            Ok(Some(msg)) => msg,
            Err(e) => {
                self.broken.push(format!("{e}: {:?}", arrived.msg));
                return;
            }
        };
        match msg {
            HostMsg::StreamStart {
                id,
                generation,
                w,
                h,
                codec,
            } => {
                self.arrivals.push(Arrival::StreamStart(id, generation));
                let name = match self.display(id) {
                    Some(d) => d.name.clone(),
                    None => {
                        self.broken
                            .push(format!("{id} started before it was listed"));
                        format!("{id}")
                    }
                };
                let starts = self.streams.get(&id).map_or(0, |s| s.starts) + 1;
                self.streams.insert(
                    id,
                    Stream {
                        name,
                        generation,
                        width: w,
                        height: h,
                        codec,
                        starts,
                        frames: 0,
                        keyframes: 0,
                        bytes: 0,
                        keyframe_bytes: 0,
                        keyframe_max: 0,
                        first_is_key: None,
                        bad_keyframes: 0,
                        last_seq: None,
                    },
                );
            }
            HostMsg::Frame {
                id,
                generation,
                seq,
                keyframe,
                data,
                ..
            } => {
                let Some(s) = self
                    .streams
                    .get_mut(&id)
                    .filter(|s| s.generation == generation)
                else {
                    self.broken.push(format!(
                        "a frame of {id} {generation:?}, which never started"
                    ));
                    return;
                };
                if s.last_seq.is_none() {
                    self.arrivals.push(Arrival::FirstFrame(id, generation));
                }
                if s.last_seq.is_some_and(|last| seq <= last) || (s.last_seq.is_none() && seq != 0)
                {
                    self.broken
                        .push(format!("frame {seq} of {id} out of order"));
                }
                s.last_seq = Some(seq);
                s.first_is_key.get_or_insert(keyframe);
                s.frames += 1;
                s.bytes += data.len() as u64;
                if keyframe {
                    s.keyframes += 1;
                    s.keyframe_bytes += data.len() as u64;
                    s.keyframe_max = s.keyframe_max.max(data.len() as u64);
                    if !nal::starts_decoder(&data, s.codec) {
                        s.bad_keyframes += 1;
                    }
                }
            }
            HostMsg::StreamEnd { id, generation } => {
                self.arrivals.push(Arrival::StreamEnd(id, generation));
                if self
                    .streams
                    .get(&id)
                    .is_some_and(|s| s.generation == generation)
                {
                    self.streams.remove(&id);
                }
                self.ended.push(id);
            }
            HostMsg::Reply { req, outcome } => {
                self.arrivals.push(Arrival::Reply(req));
                if let Outcome::Failed { detail, .. } = &outcome {
                    self.errors.push(detail.clone());
                }
                self.replies.insert(req, outcome);
            }
            HostMsg::Problem { detail, .. } => self.errors.push(detail),
            HostMsg::MicActive { active } => self.mic_active = Some(active),
            HostMsg::Audio { pcm } => {
                self.audio_bytes += pcm.len() as u64;
                self.sound_between_fragments += u32::from(arrived.between_fragments);
                if let Some(made) = pcm.first_chunk::<8>() {
                    let made = u64::from_le_bytes(*made);
                    let late = arrived.monotonic_ns.saturating_sub(made);
                    self.sound_delays.push(Duration::from_nanos(late));
                }
                self.sound_gaps.event(arrived.at);
            }
            HostMsg::Error { text } => self.errors.push(text),
            HostMsg::Notice { text } => self.notices.push(text),
            HostMsg::Ping { .. } => self.pings += 1,
            HostMsg::Goodbye { reason } => self.goodbye = Some(reason),
            other => self.arrivals.push(Arrival::Other(other_name(&other))),
        }
        self.round_trips.extend(arrived.round_trip);
    }

    /// A part of the state came (already in `pc`).
    fn part(&mut self, msg: &HostMsg) {
        match msg {
            HostMsg::Displays { part } => {
                let ids: Vec<DisplayId> = part.ids().collect();
                self.arrivals.push(Arrival::Displays(ids.clone()));
                let listed: Vec<DisplayId> = self.listed.clone();
                for id in listed.iter().filter(|id| !ids.contains(id)) {
                    self.removed.push(*id);
                }
                self.listed = ids;
            }
            HostMsg::Encoding { .. } => {
                self.encoding_count += 1;
                self.arrivals.push(Arrival::Other("Encoding"));
            }
            other => self.arrivals.push(Arrival::Other(other_name(other))),
        }
    }

    /// The displays the PC has, as it listed them.
    pub fn displays(&self) -> &[Display] {
        self.pc.displays.as_ref().map_or(&[], |d| &d.displays)
    }

    pub fn display(&self, id: DisplayId) -> Option<&Display> {
        self.displays().iter().find(|d| d.id == id)
    }

    /// The virtual monitors, in the order of the rows.
    pub fn order(&self) -> Vec<DisplayId> {
        self.displays()
            .iter()
            .filter(|d| d.is_virtual())
            .map(|d| d.id)
            .collect()
    }

    pub fn primary(&self) -> Option<DisplayId> {
        self.pc.displays.as_ref()?.primary
    }

    pub fn encoding(&self) -> Option<&Encoding> {
        self.pc.encoding.as_ref()
    }

    /// Every virtual monitor streams and shows a picture.
    pub fn all_monitors_show(&self) -> bool {
        let order = self.order();
        !order.is_empty()
            && order
                .iter()
                .all(|id| self.streams.get(id).is_some_and(Stream::has_picture))
    }
}

/// A message's name, for [`Arrival::Other`].
fn other_name(msg: &HostMsg) -> &'static str {
    match msg {
        HostMsg::Displays { .. } => "Displays",
        HostMsg::Encoding { .. } => "Encoding",
        HostMsg::Sound { .. } => "Sound",
        HostMsg::Limits { .. } => "Limits",
        HostMsg::Access { .. } => "Access",
        HostMsg::PcInfo { .. } => "PcInfo",
        HostMsg::HostCaps { .. } => "HostCaps",
        HostMsg::Focus { .. } => "Focus",
        HostMsg::SoundFrom { .. } => "SoundFrom",
        HostMsg::Snapshot { .. } => "Snapshot",
        HostMsg::Recenter => "Recenter",
        HostMsg::Problem { .. } => "Problem",
        _ => "other",
    }
}

/// A message from the PC and when it was read off the connection.
struct Arrived {
    msg: HostMsg,
    at: Instant,
    /// `CLOCK_MONOTONIC` at the same moment, in nanoseconds.
    monotonic_ns: u64,
    /// For a pong: how long since its ping went out.
    round_trip: Option<Duration>,
    /// Another message was half way in when this one completed.
    between_fragments: bool,
}

/// What a test asks of the headset's threads, and what they found.
#[derive(Default)]
struct Shared {
    /// Neither reading nor sending until then, as when Wi-Fi freezes.
    stalled_until: Mutex<Option<Instant>>,
    /// Hang up as soon as a message is half way in.
    cut_mid_message: AtomicBool,
    /// Why reading stopped.
    end: Mutex<Option<std::io::ErrorKind>>,
    pings: Mutex<Pings>,
}

impl Shared {
    fn wait_while_stalled(&self) {
        loop {
            let until = *self.stalled_until.lock().unwrap();
            match until.map(|t| t.saturating_duration_since(Instant::now())) {
                Some(left) if !left.is_zero() => std::thread::sleep(left),
                _ => return,
            }
        }
    }
}

/// Keeps the receive buffer small, so a headset that reads slowly holds the PC back
/// at once instead of after megabytes.
struct SmallReceiveBuffer;

impl SocketOptions for SmallReceiveBuffer {
    fn prepare(&self, stream: &TcpStream) {
        use std::os::fd::AsRawFd;
        let size: libc::c_int = 64 * 1024;
        // SAFETY: a valid socket and a c_int option value that lives through the call.
        unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    fn set_pace(&self, _stream: &TcpStream, _bytes_per_sec: u64) {}
}

/// `CLOCK_MONOTONIC` now, in nanoseconds (the clock the fake desktop stamps sound with).
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// A connected fake headset. A reader thread takes everything the PC sends (so the
/// PC never waits on this side) and acknowledges shown frames like the Quest app;
/// [`FakeHeadset::recv`] and [`FakeHeadset::wait`] go through what it took.
pub struct FakeHeadset {
    closer: Closer,
    writer: Arc<Mutex<Box<dyn RecordSender>>>,
    shared: Arc<Shared>,
    incoming: Receiver<Arrived>,
    view: View,
    /// The number of the last request sent.
    last_req: u32,
    /// The PC's protocol version and language, from its welcome.
    pub version: u32,
    pub language: String,
}

impl FakeHeadset {
    /// What this fake headset says about itself in `Open`: it shows everything the
    /// Quest app shows and decodes what it decodes, worn.
    pub fn state() -> HeadsetState {
        HeadsetState {
            caps: Some(HeadsetCaps {
                features: vec![caps::headset::PHYSICAL, caps::headset::WINDOWS],
                decoders: vec![
                    Decoder {
                        codec: 0,
                        max_w: 4096,
                        max_h: 4096,
                    },
                    Decoder {
                        codec: 1,
                        max_w: 8192,
                        max_h: 4320,
                    },
                ],
                streams_max: 30,
                layers_max: 16,
                model: "prostir-testkit".into(),
            }),
            prefs: Some(Prefs::default()),
            presence: Some(Presence { worn: true }),
            battery: None,
        }
    }

    /// Connects to the PC at `addr`, says hello and opens the session. A PC this
    /// headset does not know, or one that does not know this headset, is paired with
    /// the code `pairing` gives (shown by the headset, typed on the PC); without it a
    /// PC that does not know the headset is [`ConnectError::NotPaired`].
    pub fn connect(
        addr: SocketAddr,
        keys: &mut Keys,
        pairing: Option<&mut dyn FnMut() -> String>,
    ) -> Result<Self, ConnectError> {
        let app = prostir_proto::app_version(env!("CARGO_PKG_VERSION"));
        Self::open(addr, keys, pairing, None, &app, &Self::state())
    }

    /// As [`FakeHeadset::connect`] for a trusted headset whose app says it is `app` in
    /// its hello (another build, say).
    pub fn connect_as(addr: SocketAddr, keys: &mut Keys, app: &str) -> Result<Self, ConnectError> {
        Self::open(addr, keys, None, None, app, &Self::state())
    }

    /// As [`FakeHeadset::connect`] for a trusted headset that says `state` about
    /// itself in `Open` (fewer capabilities, say).
    pub fn connect_saying(
        addr: SocketAddr,
        keys: &mut Keys,
        state: &HeadsetState,
    ) -> Result<Self, ConnectError> {
        let app = prostir_proto::app_version(env!("CARGO_PKG_VERSION"));
        Self::open(addr, keys, None, None, &app, state)
    }

    /// As [`FakeHeadset::connect`] for a trusted headset that reads at most
    /// `bytes_per_sec` (a slow link), with a small receive buffer.
    pub fn connect_reading_at(
        addr: SocketAddr,
        keys: &mut Keys,
        bytes_per_sec: u64,
    ) -> Result<Self, ConnectError> {
        let app = prostir_proto::app_version(env!("CARGO_PKG_VERSION"));
        Self::open(addr, keys, None, Some(bytes_per_sec), &app, &Self::state())
    }

    fn open(
        addr: SocketAddr,
        keys: &mut Keys,
        mut pairing: Option<&mut dyn FnMut() -> String>,
        read_rate: Option<u64>,
        app: &str,
        state: &HeadsetState,
    ) -> Result<Self, ConnectError> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(REPLY))?;
        let options: Arc<dyn SocketOptions> = match read_rate {
            Some(_) => Arc::new(SmallReceiveBuffer),
            None => Arc::new(Plain),
        };
        let Link {
            mut sender,
            mut receiver,
            closer,
            peer,
        } = NoiseTcp::connect(stream, &keys.identity, options)?;
        let mut packets = HostMsg::assembler();
        let mut ends = Ends {
            rx: &mut *receiver,
            tx: &mut *sender,
            packets: &mut packets,
        };
        let (version, language, pc) = open(&mut ends, keys, &peer, addr, &mut pairing, app, state)?;
        receiver.set_idle_limit(Some(SILENCE));
        let writer = Arc::new(Mutex::new(sender));
        let shared = Arc::new(Shared::default());
        let incoming = spawn_reader(
            receiver,
            packets,
            writer.clone(),
            shared.clone(),
            closer.clone(),
            read_rate,
        )?;
        spawn_pinger(writer.clone(), shared.clone())?;
        let listed = pc.displays.iter().flat_map(|d| d.ids()).collect();
        Ok(Self {
            closer,
            writer,
            shared,
            incoming,
            view: View {
                pc,
                listed,
                ..View::default()
            },
            last_req: 0,
            version,
            language,
        })
    }

    /// Sends request `asked`, numbered after the last one: its number.
    pub fn request(&mut self, asked: Request) -> Result<ReqId> {
        self.last_req += 1;
        let req = ReqId(self.last_req);
        self.send(&ClientMsg::request(req, asked))?;
        Ok(req)
    }

    /// Takes what the PC sends until it answers request `req`, for up to `timeout`.
    pub fn reply(&mut self, req: ReqId, timeout: Duration) -> Result<Outcome> {
        self.wait(timeout, &format!("the reply to request {}", req.0), |v| {
            v.replies.contains_key(&req)
        })?;
        Ok(self.view.replies[&req].clone())
    }

    /// Sends request `asked` and waits for its reply.
    pub fn ask(&mut self, asked: Request) -> Result<Outcome> {
        let req = self.request(asked)?;
        self.reply(req, REPLY)
    }

    /// Sends `msg` to the PC.
    pub fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        send(&mut **self.writer.lock().unwrap(), msg.clone())?;
        Ok(())
    }

    /// What the PC said so far in this session.
    pub fn view(&self) -> &View {
        &self.view
    }

    /// The next message from the PC, within `timeout`. `None`: nothing came in time.
    /// Once the PC closed the connection, an error.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<HostMsg>> {
        match self.incoming.recv_timeout(timeout) {
            Ok(arrived) => {
                self.view.apply(&arrived);
                Ok(Some(arrived.msg))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                self.view.closed = true;
                self.view.end = *self.shared.end.lock().unwrap();
                bail!("the PC closed the connection")
            }
        }
    }

    /// Takes what the PC sends until `done` holds for the view, for up to `timeout`.
    /// The error says `what` was awaited and what the headset saw.
    pub fn wait(
        &mut self,
        timeout: Duration,
        what: &str,
        done: impl Fn(&View) -> bool,
    ) -> Result<()> {
        let until = Instant::now() + timeout;
        while !done(&self.view) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "no {what} within {timeout:?}; the headset saw {:#?}",
                    self.view
                );
            }
            self.recv(left)
                .with_context(|| format!("waiting for {what}; saw {:#?}", self.view))?;
        }
        Ok(())
    }

    /// Takes what the PC sends for `time` (the session must stay open).
    pub fn take_for(&mut self, time: Duration) -> Result<()> {
        let until = Instant::now() + time;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            self.recv(left)?;
        }
    }

    /// Waits up to `timeout` for the PC to close the connection.
    pub fn wait_closed(&mut self, timeout: Duration) -> Result<()> {
        let until = Instant::now() + timeout;
        while !self.view.closed {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("the PC kept the connection open for {timeout:?}");
            }
            let _ = self.recv(left);
        }
        Ok(())
    }

    /// Counts start again from here: frames, sound, ended streams, errors and notices.
    /// What the headset knows stays: the streams, the PC's state.
    pub fn reset_counts(&mut self) {
        let v = &mut self.view;
        for s in v.streams.values_mut() {
            (s.frames, s.keyframes, s.bad_keyframes, s.bytes) = (0, 0, 0, 0);
            (s.keyframe_bytes, s.keyframe_max) = (0, 0);
        }
        v.ended.clear();
        v.removed.clear();
        v.arrivals.clear();
        v.errors.clear();
        v.notices.clear();
        v.pings = 0;
        v.round_trips.clear();
        v.audio_bytes = 0;
        v.sound_between_fragments = 0;
        v.sound_delays.clear();
        v.sound_gaps.restart();
        v.messages = 0;
    }

    /// Neither reads nor sends for `time` from now, as when its Wi-Fi freezes.
    pub fn stall(&self, time: Duration) {
        *self.shared.stalled_until.lock().unwrap() = Some(Instant::now() + time);
    }

    /// Hangs up as soon as the next message is half way in (a frame, mostly).
    pub fn cut_mid_message(&self) {
        self.shared.cut_mid_message.store(true, Ordering::Relaxed);
    }

    /// Sends `record` as it is, a broken packet say.
    pub fn send_record(&self, record: &[u8]) -> Result<()> {
        self.writer.lock().unwrap().send(&[record])?;
        Ok(())
    }

    /// Hangs up, as a headset that lost the network would.
    pub fn disconnect(self) {
        self.closer.close();
    }

    /// Leaves as the Quest app does: a goodbye saying `reason`, nothing after it, and
    /// the connection closed once the PC closed it (or after `GOODBYE_WAIT`).
    pub fn leave(mut self, reason: prostir_proto::HeadsetGoodbye) -> Result<()> {
        {
            let mut w = self.writer.lock().unwrap();
            send(
                &mut **w,
                ClientMsg::Goodbye {
                    reason: reason.code(),
                },
            )?;
            w.finish()?;
        }
        self.wait_closed(GOODBYE_WAIT)
    }
}

impl Drop for FakeHeadset {
    fn drop(&mut self) {
        self.closer.close();
    }
}

/// Sends `msg` whole: microphone sound in its lane, everything else in the control one.
fn send(tx: &mut dyn RecordSender, msg: ClientMsg) -> std::io::Result<()> {
    let lane = match msg {
        ClientMsg::Mic { .. } => lane::SOUND,
        _ => lane::CONTROL,
    };
    packet::send_whole(tx, lane, msg.encode()?, FRAGMENT)
}

/// The two halves of a connection before the session, and the PC's packets put
/// together so far.
struct Ends<'a> {
    rx: &'a mut dyn RecordReceiver,
    tx: &'a mut dyn RecordSender,
    packets: &'a mut Assembler,
}

impl Ends<'_> {
    fn send(&mut self, msg: ClientMsg) -> std::io::Result<()> {
        send(self.tx, msg)
    }

    fn recv(&mut self) -> std::io::Result<HostMsg> {
        HostMsg::recv(self.rx, self.packets)
    }
}

/// Says hello, pairs if asked to and possible, and opens the session with `state`: the
/// PC's protocol version, language and state (its snapshot, which must come first).
fn open(
    ends: &mut Ends,
    keys: &mut Keys,
    peer: &PeerInfo,
    addr: SocketAddr,
    pairing: &mut Option<&mut dyn FnMut() -> String>,
    app: &str,
    state: &HeadsetState,
) -> Result<(u32, String, PcState), ConnectError> {
    // Versions first, before pairing.
    handshake::send_hello(ends.tx, app)?;
    let (version, language) = match handshake::read_answer(ends.rx)? {
        Answer::Welcome { version, language } => (version, language),
        Answer::Incompatible { protocol, app } => {
            return Err(ConnectError::Incompatible { protocol, app });
        }
    };
    if !keys.trusted.contains(&peer.key)
        && let Some(code) = pairing.as_mut()
    {
        pair(ends, &keys.identity, peer, &code())?;
        keys.trusted.add(peer.key, &addr.to_string())?;
    }
    let open = state.open().map_err(|e| anyhow!("{e}"))?;
    ends.send(open.clone())?;
    // The PC is trusted from here: its snapshot may be as big as it needs.
    ends.packets.set_limits(HostMsg::limits(true));
    let mut refused = None;
    loop {
        let msg = match ends.recv() {
            Ok(msg) => msg,
            // Said why, then closed the connection.
            Err(e) => {
                return Err(match refused {
                    Some(why) => ConnectError::Refused(why),
                    None => e.into(),
                });
            }
        };
        match msg {
            HostMsg::Snapshot { parts } => {
                let pc = PcState::from_snapshot(&parts).map_err(|e| anyhow!("snapshot: {e}"))?;
                return Ok((version, language, pc));
            }
            HostMsg::Error { text } => refused = Some(text),
            HostMsg::PairingRequired => {
                let Some(code) = pairing.as_mut() else {
                    return Err(ConnectError::NotPaired);
                };
                ends.packets.set_limits(HostMsg::limits(false));
                pair(ends, &keys.identity, peer, &code())?;
                keys.trusted.add(peer.key, &addr.to_string())?;
                ends.send(open.clone())?;
                ends.packets.set_limits(HostMsg::limits(true));
            }
            other => return Err(anyhow!("unexpected {other:?} before the snapshot").into()),
        }
    }
}

/// Reads everything the PC sends on a thread of its own, acknowledging frames as
/// shown (at most every [`ACK_EVERY`] per stream). The channel closes with the
/// connection.
fn spawn_reader(
    mut receiver: Box<dyn RecordReceiver>,
    mut packets: Assembler,
    writer: Arc<Mutex<Box<dyn RecordSender>>>,
    shared: Arc<Shared>,
    closer: Closer,
    read_rate: Option<u64>,
) -> std::io::Result<Receiver<Arrived>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("fake-headset".into())
        .spawn(move || {
            // However reading stops, the connection closes with it, as in the Quest app.
            struct Close(Closer);
            impl Drop for Close {
                fn drop(&mut self) {
                    self.0.close();
                }
            }
            let _close = Close(closer.clone());
            let mut acked: BTreeMap<DisplayId, Instant> = BTreeMap::new();
            // Bytes read but not yet waited for, when reading at a set rate.
            let mut unpaid = 0u64;
            loop {
                shared.wait_while_stalled();
                let record = match receiver.recv() {
                    Ok(record) => record,
                    Err(e) => {
                        *shared.end.lock().unwrap() = Some(e.kind());
                        return;
                    }
                };
                if let Some(rate) = read_rate {
                    unpaid += record.len() as u64;
                    let owed = Duration::from_secs_f64(unpaid as f64 / rate as f64);
                    if owed >= Duration::from_millis(2) {
                        std::thread::sleep(owed);
                        unpaid = 0;
                    }
                }
                let complete = match packets.push(record) {
                    Ok(complete) => complete,
                    Err(e) => {
                        *shared.end.lock().unwrap() = Some(std::io::Error::from(e).kind());
                        return;
                    }
                };
                let Some(m) = complete else {
                    // Half a message in: hang up now if asked to.
                    if shared.cut_mid_message.load(Ordering::Relaxed) {
                        *shared.end.lock().unwrap() = Some(std::io::ErrorKind::ConnectionAborted);
                        return;
                    }
                    continue;
                };
                let between_fragments = packets.assembling() > 0;
                let msg = match HostMsg::from_message(m) {
                    Ok(Some(msg)) => msg,
                    Ok(None) => continue,
                    Err(e) => {
                        *shared.end.lock().unwrap() = Some(e.kind());
                        return;
                    }
                };
                let (at, monotonic_ns) = (Instant::now(), monotonic_ns());
                let mut round_trip = None;
                match &msg {
                    // Answered at once, from here, as the Quest app does.
                    HostMsg::Ping { seq } => {
                        let pong = ClientMsg::Pong { seq: *seq };
                        let _ = send(&mut **writer.lock().unwrap(), pong);
                    }
                    HostMsg::Pong { seq } => {
                        round_trip = shared.pings.lock().unwrap().pong(*seq, at);
                    }
                    _ => {}
                }
                if let HostMsg::Frame {
                    id,
                    generation,
                    seq,
                    ..
                } = &msg
                    && acked.get(id).is_none_or(|t| t.elapsed() >= ACK_EVERY)
                {
                    acked.insert(*id, Instant::now());
                    let shown = ClientMsg::FrameShown {
                        id: *id,
                        generation: *generation,
                        seq: *seq,
                    };
                    let _ = send(&mut **writer.lock().unwrap(), shown);
                }
                let arrived = Arrived {
                    msg,
                    at,
                    monotonic_ns,
                    round_trip,
                    between_fragments,
                };
                if tx.send(arrived).is_err() {
                    return;
                }
            }
        })?;
    Ok(rx)
}

/// Pings the PC every [`PING_EVERY`] (but not while stalled) until the connection
/// closes.
fn spawn_pinger(
    writer: Arc<Mutex<Box<dyn RecordSender>>>,
    shared: Arc<Shared>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("fake-headset-ping".into())
        .spawn(move || {
            loop {
                std::thread::sleep(PING_EVERY);
                shared.wait_while_stalled();
                let seq = shared.pings.lock().unwrap().ping(Instant::now());
                if send(&mut **writer.lock().unwrap(), ClientMsg::Ping { seq }).is_err() {
                    return;
                }
            }
        })?;
    Ok(())
}

/// Pairs with the code this headset shows (the person at the PC types it), waiting
/// for the PC's answers as long as a person may take.
fn pair(
    ends: &mut Ends,
    identity: &Identity,
    peer: &PeerInfo,
    code: &str,
) -> Result<(), ConnectError> {
    ends.rx.set_idle_limit(Some(PAIRING));
    let paired = exchange_codes(ends, identity, peer, code);
    ends.rx.set_idle_limit(Some(REPLY));
    paired
}

fn exchange_codes(
    ends: &mut Ends,
    identity: &Identity,
    peer: &PeerInfo,
    code: &str,
) -> Result<(), ConnectError> {
    ends.send(ClientMsg::PairBegin {
        headset_shows: true,
        name: "prostir-testkit".into(),
    })?;
    let (pairing, msg) = Pairing::start(code);
    ends.send(ClientMsg::PairMsg { msg })?;
    let mut said = String::new();
    let theirs = loop {
        match ends.recv()? {
            HostMsg::PairMsg { msg } => break msg,
            HostMsg::PairResult { .. } => {
                return Err(ConnectError::PairingFailed(format!(
                    "the PC gave up: {said}"
                )));
            }
            HostMsg::Error { text } => said = text,
            other => return Err(anyhow!("unexpected {other:?} while pairing").into()),
        }
    };
    let key = pairing.finish(&theirs)?;
    ends.send(ClientMsg::PairConfirm {
        tag: confirmation(&key, "headset", peer, &identity.public),
    })?;
    match ends.recv()? {
        HostMsg::PairResult { tag: Some(tag) }
            if tags_match(&tag, &confirmation(&key, "pc", peer, &identity.public)) =>
        {
            Ok(())
        }
        HostMsg::PairResult { .. } => Err(ConnectError::PairingFailed(
            "the codes did not match".into(),
        )),
        other => Err(anyhow!("unexpected {other:?} after pairing").into()),
    }
}
