use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use prostir_proto::link::tcp::{NoiseTcp, Plain};
use prostir_proto::link::{
    Closer, Link, PeerInfo, RecordReceiver, RecordSender, StreamReader, StreamWriter,
};
use prostir_proto::secure::{Identity, Pairing, TrustStore, confirmation, tags_match};
use prostir_proto::stats::Gaps;
use prostir_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION, PcSettings};

use crate::nal;

/// The headset reports a shown frame at most this often per stream, like the Quest app.
const ACK_EVERY: Duration = Duration::from_millis(100);
/// The PC answers within this, and takes what is sent within this.
const REPLY: Duration = Duration::from_secs(10);
/// Pairing waits for a person typing the code on the PC.
const PAIRING: Duration = Duration::from_secs(300);

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
    Other(anyhow::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPaired => write!(f, "the PC does not know this headset"),
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

/// One monitor (or window panel, or mirror) stream as the headset sees it.
#[derive(Debug, Clone)]
pub struct Stream {
    pub name: String,
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
}

impl Stream {
    /// A decoder would show a picture: a good keyframe came since the last start.
    pub fn has_picture(&self) -> bool {
        self.keyframes > self.bad_keyframes
    }
}

/// What the PC told the headset in this session.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// Streams running now, by id.
    pub streams: BTreeMap<u8, Stream>,
    /// Ids of streams the PC ended, in order.
    pub ended: Vec<u8>,
    /// Left-to-right order of the monitors, as last sent.
    pub order: Vec<u8>,
    pub primary: Option<u8>,
    /// How the PC encodes and what it could encode with.
    pub pc_settings: Option<(PcSettings, u8)>,
    /// How many times the PC sent them, to tell an answer from what it said before.
    pub pc_settings_count: u32,
    pub audio_wanted: Option<bool>,
    pub mic_wanted: Option<bool>,
    pub mirror_wanted: Option<bool>,
    /// Bytes of PC sound received.
    pub audio_bytes: u64,
    /// How late each slice of sound arrived, in order: from when the fake desktop made
    /// it (the time it starts with) to when it was read here. Only a PC with the fake
    /// desktop stamps its sound so; from any other PC these numbers mean nothing.
    pub sound_delays: Vec<Duration>,
    /// Pauses between slices of sound arriving.
    pub sound_gaps: Gaps,
    pub errors: Vec<String>,
    pub notices: Vec<String>,
    pub messages: u64,
    /// The PC closed the connection.
    pub closed: bool,
}

impl View {
    fn apply(&mut self, arrived: &Arrived) {
        let msg = &arrived.msg;
        self.messages += 1;
        match msg {
            HostMsg::StreamStart {
                id,
                width,
                height,
                codec,
                name,
            } => {
                let starts = self.streams.get(id).map_or(0, |s| s.starts) + 1;
                self.streams.insert(
                    *id,
                    Stream {
                        name: name.clone(),
                        width: *width,
                        height: *height,
                        codec: *codec,
                        starts,
                        frames: 0,
                        keyframes: 0,
                        bytes: 0,
                        keyframe_bytes: 0,
                        keyframe_max: 0,
                        first_is_key: None,
                        bad_keyframes: 0,
                    },
                );
            }
            HostMsg::Frame {
                id, keyframe, data, ..
            } => {
                if let Some(s) = self.streams.get_mut(id) {
                    s.first_is_key.get_or_insert(*keyframe);
                    s.frames += 1;
                    s.bytes += data.len() as u64;
                    if *keyframe {
                        s.keyframes += 1;
                        s.keyframe_bytes += data.len() as u64;
                        s.keyframe_max = s.keyframe_max.max(data.len() as u64);
                        if !nal::starts_decoder(data, s.codec) {
                            s.bad_keyframes += 1;
                        }
                    }
                }
            }
            HostMsg::StreamEnd { id } => {
                self.streams.remove(id);
                self.ended.push(*id);
            }
            HostMsg::Order { ids } => self.order = ids.clone(),
            HostMsg::PrimaryChanged { id } => self.primary = Some(*id),
            HostMsg::PcSettings { settings, encoders } => {
                self.pc_settings = Some((*settings, *encoders));
                self.pc_settings_count += 1;
            }
            HostMsg::AudioWanted { enabled } => self.audio_wanted = Some(*enabled),
            HostMsg::MicWanted { enabled } => self.mic_wanted = Some(*enabled),
            HostMsg::MirrorWanted { enabled } => self.mirror_wanted = Some(*enabled),
            HostMsg::Audio { pcm } => {
                self.audio_bytes += pcm.len() as u64;
                if let Some(made) = pcm.first_chunk::<8>() {
                    let made = u64::from_le_bytes(*made);
                    let late = arrived.monotonic_ns.saturating_sub(made);
                    self.sound_delays.push(Duration::from_nanos(late));
                }
                self.sound_gaps.event(arrived.at);
            }
            HostMsg::Error { text } => self.errors.push(text.clone()),
            HostMsg::Notice { text } => self.notices.push(text.clone()),
            _ => {}
        }
    }

    /// Every monitor in `order` streams and shows a picture.
    pub fn all_monitors_show(&self) -> bool {
        !self.order.is_empty()
            && self
                .order
                .iter()
                .all(|id| self.streams.get(id).is_some_and(Stream::has_picture))
    }
}

/// A message from the PC and when it was read off the connection.
struct Arrived {
    msg: HostMsg,
    at: Instant,
    /// `CLOCK_MONOTONIC` at the same moment, in nanoseconds.
    monotonic_ns: u64,
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
    incoming: Receiver<Arrived>,
    view: View,
    /// The PC's protocol version and language, from its welcome.
    pub version: u32,
    pub language: String,
}

impl FakeHeadset {
    /// Connects to the PC at `addr`, says hello and opens the session. A PC this
    /// headset does not know, or one that does not know this headset, is paired with
    /// the code `pairing` gives (shown by the headset, typed on the PC); without it a
    /// PC that does not know the headset is [`ConnectError::NotPaired`].
    pub fn connect(
        addr: SocketAddr,
        keys: &mut Keys,
        mut pairing: Option<&mut dyn FnMut() -> String>,
    ) -> Result<Self, ConnectError> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(REPLY))?;
        let Link {
            mut sender,
            mut receiver,
            closer,
            peer,
        } = NoiseTcp::connect(stream, &keys.identity, Arc::new(Plain))?;
        let (version, language) = open(
            &mut *receiver,
            &mut *sender,
            keys,
            &peer,
            addr,
            &mut pairing,
        )?;
        receiver.set_idle_limit(None);
        let writer = Arc::new(Mutex::new(sender));
        let incoming = spawn_reader(receiver, writer.clone())?;
        Ok(Self {
            closer,
            writer,
            incoming,
            view: View::default(),
            version,
            language,
        })
    }

    /// Sends `msg` to the PC.
    pub fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        msg.write(&mut StreamWriter(&mut **w))?;
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
    /// What the headset knows stays: the streams, the order, the settings.
    pub fn reset_counts(&mut self) {
        let v = &mut self.view;
        for s in v.streams.values_mut() {
            (s.frames, s.keyframes, s.bad_keyframes, s.bytes) = (0, 0, 0, 0);
            (s.keyframe_bytes, s.keyframe_max) = (0, 0);
        }
        v.ended.clear();
        v.errors.clear();
        v.notices.clear();
        v.audio_bytes = 0;
        v.sound_delays.clear();
        v.sound_gaps.restart();
        v.messages = 0;
    }

    /// Hangs up, as a headset that lost the network would.
    pub fn disconnect(self) {
        self.closer.close();
    }
}

impl Drop for FakeHeadset {
    fn drop(&mut self) {
        self.closer.close();
    }
}

/// Says hello, pairs if asked to and possible, and opens the session: the PC's protocol
/// version and language.
fn open(
    receiver: &mut dyn RecordReceiver,
    sender: &mut dyn RecordSender,
    keys: &mut Keys,
    peer: &PeerInfo,
    addr: SocketAddr,
    pairing: &mut Option<&mut dyn FnMut() -> String>,
) -> Result<(u32, String), ConnectError> {
    let mut r = StreamReader::new(receiver);
    let mut w = StreamWriter(sender);
    // Versions first, before pairing.
    ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        min: prostir_proto::MIN_PROTOCOL,
        app: env!("CARGO_PKG_VERSION").into(),
    }
    .write(&mut w)?;
    let (version, language) = loop {
        match HostMsg::read_setup(&mut r)? {
            HostMsg::Welcome { version, language } => break (version, language),
            HostMsg::Incompatible { protocol, app } => {
                return Err(ConnectError::Incompatible { protocol, app });
            }
            // Older headsets show this text; `Incompatible` follows.
            HostMsg::Error { .. } => {}
            other => return Err(anyhow!("unexpected {other:?} before welcome").into()),
        }
    };
    if !keys.trusted.contains(&peer.key)
        && let Some(code) = pairing.as_mut()
    {
        pair(&mut r, &mut w, &keys.identity, peer, &code())?;
        keys.trusted.add(peer.key, &addr.to_string())?;
    }
    ClientMsg::Open.write(&mut w)?;
    loop {
        match HostMsg::read_setup(&mut r)? {
            HostMsg::Opened => return Ok((version, language)),
            HostMsg::PairingRequired => {
                let Some(code) = pairing.as_mut() else {
                    return Err(ConnectError::NotPaired);
                };
                pair(&mut r, &mut w, &keys.identity, peer, &code())?;
                keys.trusted.add(peer.key, &addr.to_string())?;
                ClientMsg::Open.write(&mut w)?;
            }
            other => return Err(anyhow!("unexpected {other:?} before the session").into()),
        }
    }
}

/// Reads everything the PC sends on a thread of its own, acknowledging frames as
/// shown (at most every [`ACK_EVERY`] per stream). The channel closes with the
/// connection.
fn spawn_reader(
    mut receiver: Box<dyn RecordReceiver>,
    writer: Arc<Mutex<Box<dyn RecordSender>>>,
) -> std::io::Result<Receiver<Arrived>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("fake-headset".into())
        .spawn(move || {
            let mut r = StreamReader::new(&mut *receiver);
            let mut acked: BTreeMap<u8, Instant> = BTreeMap::new();
            while let Ok(msg) = HostMsg::read(&mut r) {
                let (at, monotonic_ns) = (Instant::now(), monotonic_ns());
                if let HostMsg::Frame { id, pts_us, .. } = &msg
                    && acked.get(id).is_none_or(|t| t.elapsed() >= ACK_EVERY)
                {
                    acked.insert(*id, Instant::now());
                    let mut w = writer.lock().unwrap();
                    let shown = ClientMsg::FrameShown {
                        id: *id,
                        pts_us: *pts_us,
                    };
                    let _ = shown.write(&mut StreamWriter(&mut **w));
                }
                let arrived = Arrived {
                    msg,
                    at,
                    monotonic_ns,
                };
                if tx.send(arrived).is_err() {
                    return;
                }
            }
        })?;
    Ok(rx)
}

/// Pairs with the code this headset shows (the person at the PC types it), waiting
/// for the PC's answers as long as a person may take.
fn pair(
    r: &mut StreamReader,
    w: &mut StreamWriter,
    identity: &Identity,
    peer: &PeerInfo,
    code: &str,
) -> Result<(), ConnectError> {
    r.receiver().set_idle_limit(Some(PAIRING));
    let paired = exchange_codes(r, w, identity, peer, code);
    r.receiver().set_idle_limit(Some(REPLY));
    paired
}

fn exchange_codes(
    r: &mut StreamReader,
    w: &mut StreamWriter,
    identity: &Identity,
    peer: &PeerInfo,
    code: &str,
) -> Result<(), ConnectError> {
    ClientMsg::PairBegin {
        headset_shows: true,
        name: "prostir-testkit".into(),
    }
    .write(w)?;
    let (pairing, msg) = Pairing::start(code);
    ClientMsg::PairMsg { msg }.write(w)?;
    let mut said = String::new();
    let theirs = loop {
        match HostMsg::read_setup(r)? {
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
    ClientMsg::PairConfirm {
        tag: confirmation(&key, "headset", peer, &identity.public),
    }
    .write(w)?;
    match HostMsg::read_setup(r)? {
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
