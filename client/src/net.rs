//! Finds the PC host, keeps the TCP connection alive and fans frames out to decoders.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};
use prostir_proto::secure::{
    self, Identity, Pairing, SecureReader, SecureWriter, TrustStore, confirmation, tags_match,
};
use prostir_proto::{ClientMsg, Codec, HostMsg, NO_FOCUS, PROTOCOL_VERSION};

pub struct Frame {
    /// When the frame arrived from the network.
    pub received: std::time::Instant,
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Messages to the PC. Sending only queues them: a writer thread puts them on the
/// network, so the frame loop, the microphone and the decoders never wait on Wi-Fi.
#[derive(Clone)]
pub struct Feedback(SyncSender<ClientMsg>);

impl Feedback {
    /// Starts the writer thread for one session. It ends (and shuts the socket down,
    /// which ends the session) when a write fails or times out.
    fn start(mut writer: SecureWriter, stream: TcpStream) -> std::io::Result<Self> {
        // About 2.5 s of microphone sound, plus room for everything else.
        let (tx, rx) = mpsc::sync_channel::<ClientMsg>(256);
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        std::thread::Builder::new()
            .name("net-writer".into())
            .spawn(move || {
                let mut buf = Vec::with_capacity(4096);
                while let Ok(msg) = rx.recv() {
                    // Everything already waiting goes out as one encrypted chunk.
                    buf.clear();
                    let _ = msg.write(&mut buf);
                    while buf.len() < 64 * 1024
                        && let Ok(more) = rx.try_recv()
                    {
                        let _ = more.write(&mut buf);
                    }
                    if let Err(e) = writer.write_all(&buf) {
                        warn!("sending to the PC failed: {e}");
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                }
            })?;
        Ok(Self(tx))
    }

    /// Queues a message. When the queue is full (the network is stuck) the message is
    /// dropped; an error means the session is over.
    pub fn send(&self, msg: &ClientMsg) -> std::io::Result<()> {
        match self.0.try_send(msg.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(std::io::ErrorKind::BrokenPipe.into()),
        }
    }
}

/// A PC announcing itself on the local network.
#[derive(Debug, Clone, PartialEq)]
pub struct FoundHost {
    pub name: String,
    pub addr: SocketAddr,
}

pub enum NetEvent {
    /// Connected; the handle sends requests (add monitor…) to the host.
    Connected {
        addr: SocketAddr,
        name: Option<String>,
        feedback: Feedback,
        /// The PC's interface language.
        language: Option<crate::i18n::Lang>,
    },
    /// PCs currently seen on the network (sent when the list changes).
    Hosts(Vec<FoundHost>),
    PrimaryChanged(u8),
    MirrorWanted(bool),
    /// Whether PC sound comes to the headset.
    AudioWanted(bool),
    /// The monitor the mouse is on, if it is one of ours.
    Focus(Option<u8>),
    /// Left-to-right order of the Quest monitors.
    Order(Vec<u8>),
    /// A request failed on the host; shown in the menu.
    HostError(String),
    StreamStart {
        id: u8,
        width: u32,
        height: u32,
        codec: Codec,
        frames: Receiver<Frame>,
        feedback: Feedback,
    },
    /// `removed`: the host took the monitor away (not just a lost connection).
    StreamEnd {
        id: u8,
        removed: bool,
    },
    /// Pairing with a new PC: show `code` (the PC asks for it), or, with `None`,
    /// ask for the code the PC shows.
    Pairing {
        host: String,
        code: Option<String>,
    },
    /// The codes did not match; a new attempt follows.
    PairingFailed,
    /// How the PC encodes and what it can encode with.
    PcSettings(prostir_proto::PcSettings, u8),
    /// The PC's tray asks to bring the monitors in front.
    Recenter,
    /// Something done on the PC, to show briefly.
    Notice(String),
    /// A PC announces itself on the network but does not accept the connection
    /// (a firewall on the PC); `None` once connecting works again.
    Unreachable(Option<String>),
    /// An unknown PC: ask the user whether to pair with it (`failed`: after several
    /// wrong codes).
    PairOffer {
        host: String,
        failed: bool,
    },
    Paired,
    Disconnected,
    /// The PC speaks another protocol version: `pc_newer` says which side to update.
    Incompatible {
        pc_newer: bool,
        pc_app: String,
    },
}

/// Where to look for the PC, and this headset's keys.
pub struct Discovery {
    /// This headset's long-term key.
    pub identity_file: PathBuf,
    /// PCs paired with.
    pub trust_file: PathBuf,
    /// User-provided `IP` or `IP:PORT`, tried first.
    pub host_file: Option<PathBuf>,
    /// Address of the last host that worked over the network, tried before discovery.
    pub last_host_file: PathBuf,
}

/// Lets the menu choose which PC to use and take part in pairing.
#[derive(Clone, Default)]
pub struct Steering(Arc<(Mutex<Steer>, Condvar)>);

#[derive(Default)]
struct Steer {
    /// Picked in the menu (or typed in); tried before anything else.
    choice: Option<SocketAddr>,
    /// "Change PC": do not reconnect on our own until the user picks one.
    picking: bool,
    current: Option<TcpStream>,
    /// Pairing the other way round: the PC shows the code, the headset asks for it.
    pc_shows_code: bool,
    /// Code typed in the menu while pairing.
    typed_code: Option<String>,
    /// The headset is off the head: stay away from the PC, so it can give its
    /// desktop back to the laptop.
    taken_off: bool,
    /// A PC this headset is not paired with, waiting for the user to say whether to
    /// pair. Nothing connects on its own meanwhile.
    offer: Option<(PeerKey, SocketAddr)>,
    /// The user agreed to pair with this PC (until when).
    approved: Option<(PeerKey, Instant)>,
}

type PeerKey = [u8; 32];

/// How long a "Pair" answer holds for reconnecting and typing the code.
const APPROVAL: Duration = Duration::from_secs(600);
/// Wrong codes in a row before the headset stops and asks again.
const PAIR_ATTEMPTS: u32 = 3;

impl Steering {
    fn lock(&self) -> std::sync::MutexGuard<'_, Steer> {
        self.0.0.lock().unwrap()
    }

    /// Connects to `addr`, dropping the current connection if it is another PC.
    pub fn connect_to(&self, addr: SocketAddr) {
        let mut s = self.lock();
        s.choice = Some(addr);
        s.picking = false;
        if let Some(cur) = &s.current
            && cur.peer_addr().ok() != Some(addr)
        {
            let _ = cur.shutdown(Shutdown::Both);
        }
    }

    /// Disconnects and waits for the user to pick a PC in the menu.
    pub fn change_host(&self) {
        let mut s = self.lock();
        s.choice = None;
        s.picking = true;
        if let Some(cur) = s.current.take() {
            let _ = cur.shutdown(Shutdown::Both);
        }
        self.0.1.notify_all();
    }

    /// Put on or taken off; connecting waits until the headset is worn.
    pub fn set_worn(&self, worn: bool) {
        self.lock().taken_off = !worn;
        self.0.1.notify_all();
    }

    fn wait_until_worn(&self) {
        let mut s = self.lock();
        if s.taken_off {
            info!("headset not worn: not connecting");
        }
        while s.taken_off {
            s = self.0.1.wait(s).unwrap();
        }
    }

    /// The user's answer to "Pair with this PC?".
    pub fn answer_offer(&self, pair: bool) {
        let mut s = self.lock();
        if let Some((key, addr)) = s.offer.take() {
            if pair {
                s.approved = Some((key, Instant::now() + APPROVAL));
                s.choice = Some(addr);
                s.picking = false;
            } else {
                // Not this PC: wait for the user to pick one.
                s.approved = None;
                s.choice = None;
                s.picking = true;
            }
        }
        self.0.1.notify_all();
    }

    fn approved(&self, key: &PeerKey) -> bool {
        self.lock()
            .approved
            .is_some_and(|(k, until)| &k == key && Instant::now() < until)
    }

    /// Asks the user about an unknown PC; the connection is closed meanwhile.
    fn offer(&self, key: PeerKey, addr: SocketAddr) {
        let mut s = self.lock();
        s.offer = Some((key, addr));
        s.approved = None;
    }

    /// While the user has not answered about an unknown PC, nothing connects.
    fn wait_for_answer(&self) {
        let mut s = self.lock();
        while s.offer.is_some() && !s.taken_off {
            s = self.0.1.wait(s).unwrap();
        }
    }

    /// Swaps which side shows the pairing code and restarts pairing.
    pub fn switch_pairing_side(&self) {
        let mut s = self.lock();
        s.pc_shows_code = !s.pc_shows_code;
        s.typed_code = None;
        if let Some(cur) = &s.current {
            let _ = cur.shutdown(Shutdown::Both);
        }
        self.0.1.notify_all();
    }

    /// The code the user typed for the PC's code.
    pub fn submit_code(&self, code: String) {
        self.lock().typed_code = Some(code);
        self.0.1.notify_all();
    }

    fn pc_shows_code(&self) -> bool {
        self.lock().pc_shows_code
    }

    /// Waits for a typed code; `None` if the connection was dropped meanwhile.
    fn wait_code(&self, stream: &TcpStream) -> Option<String> {
        self.lock().typed_code = None;
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            {
                let mut s = self.lock();
                if let Some(code) = s.typed_code.take() {
                    return Some(code);
                }
                // Dropped by "change PC" or "the other way round".
                if s.current
                    .as_ref()
                    .is_none_or(|c| c.peer_addr().ok() != stream.peer_addr().ok())
                {
                    return None;
                }
                let _ = self.0.1.wait_timeout(s, Duration::from_millis(500));
            }
            // Checked without holding the lock (the menu takes it every frame), and
            // without blocking: the socket's read timeout is minutes long here.
            if !alive(stream) || Instant::now() >= deadline {
                return None;
            }
        }
    }
}

pub fn spawn(events: Sender<NetEvent>, discovery: Discovery, steering: Steering) {
    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let beacon = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, prostir_proto::DISCOVERY_PORT))
                .inspect_err(|e| warn!("discovery socket: {e}"))
                .ok();
            let identity = match Identity::load_or_create(&discovery.identity_file) {
                Ok(id) => id,
                Err(e) => {
                    warn!("cannot create the headset key: {e}");
                    return;
                }
            };
            info!("headset key {}", secure::fingerprint(&identity.public));
            let mut trusted = TrustStore::load(&discovery.trust_file);
            let mut finder = Finder::default();
            let mut round = 0u32;
            loop {
                steering.wait_until_worn();
                steering.wait_for_answer();
                round += 1;
                if let Some(stream) = connect(
                    beacon.as_ref(),
                    &discovery,
                    &steering,
                    &mut finder,
                    &events,
                    round,
                ) {
                    let peer = stream.peer_addr().ok();
                    let name = peer.and_then(|p| finder.name_of(p));
                    steering.lock().current = stream.try_clone().ok();
                    let mut ended = Ended::default();
                    match run_session(
                        stream,
                        &events,
                        name,
                        &steering,
                        &identity,
                        &mut trusted,
                        &mut ended,
                    ) {
                        Ok(()) => info!("host {peer:?} closed the connection"),
                        Err(e) => warn!("connection to {peer:?} lost: {e}"),
                    }
                    // Remembered only once the PC proved it is a trusted one: a single
                    // connection to anything else must not redirect the next start.
                    if ended.welcomed
                        && let Some(p) = peer.filter(|p| !p.ip().is_loopback())
                    {
                        let _ = std::fs::write(&discovery.last_host_file, p.to_string());
                    }
                    steering.lock().current = None;
                    if events.send(NetEvent::Disconnected).is_err() {
                        return;
                    }
                    // The PC said what is wrong (no encoder, say): the message stays
                    // up, and retrying every 0.3 s would only rebuild its monitors.
                    if ended
                        .refused
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
                    {
                        std::thread::sleep(Duration::from_secs(10));
                    }
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        })
        .expect("spawn net thread");
}

/// The PC's name with its address, so a look-alike name from the network stands out.
fn offer_label(host: &str, addr: SocketAddr) -> String {
    let ip = addr.ip().to_string();
    if host == ip || addr.ip().is_loopback() {
        host.to_owned()
    } else {
        format!("{host} {ip}")
    }
}

/// Whether the PC is still connected, without waiting for anything to arrive.
fn alive(stream: &TcpStream) -> bool {
    if stream.take_error().ok().flatten().is_some() || stream.set_nonblocking(true).is_err() {
        return false;
    }
    let peeked = stream.peek(&mut [0u8; 1]);
    let _ = stream.set_nonblocking(false);
    match peeked {
        Ok(0) => false,
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
    }
}

/// A PC that announced itself refused the connection: most likely a firewall.
fn blocked(finder: &Finder, events: &Sender<NetEvent>, addr: SocketAddr) {
    if let Some(name) = finder.name_of(addr) {
        let _ = events.send(NetEvent::Unreachable(Some(name)));
    }
}

fn try_connect(addr: SocketAddr) -> Option<TcpStream> {
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok()
}

fn read_addr(path: &PathBuf) -> Option<SocketAddr> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_addr(text.trim())
}

/// `IP` or `IP:PORT`.
pub fn parse_addr(text: &str) -> Option<SocketAddr> {
    text.parse::<SocketAddr>().ok().or_else(|| {
        text.parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, prostir_proto::TCP_PORT))
    })
}

/// PCs heard on the network recently.
#[derive(Default)]
struct Finder {
    seen: HashMap<SocketAddr, (String, Instant)>,
    reported: Vec<FoundHost>,
}

impl Finder {
    /// Listens for beacons for a moment and reports changes to the menu.
    fn listen(&mut self, beacon: &UdpSocket, events: &Sender<NetEvent>) {
        let until = Instant::now() + Duration::from_millis(1200);
        let mut buf = [0u8; 512];
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            let _ = beacon.set_read_timeout(Some(left.max(Duration::from_millis(1))));
            let Ok((n, from)) = beacon.recv_from(&mut buf) else {
                break;
            };
            if let Some((port, name)) = prostir_proto::decode_beacon(&buf[..n]) {
                self.seen
                    .insert(SocketAddr::new(from.ip(), port), (name, Instant::now()));
            }
        }
        self.seen
            .retain(|_, (_, at)| at.elapsed() < Duration::from_secs(6));
        let mut hosts: Vec<FoundHost> = self
            .seen
            .iter()
            .map(|(addr, (name, _))| FoundHost {
                name: name.clone(),
                addr: *addr,
            })
            .collect();
        hosts.sort_by(|a, b| a.name.cmp(&b.name).then(a.addr.cmp(&b.addr)));
        if hosts != self.reported {
            self.reported = hosts.clone();
            let _ = events.send(NetEvent::Hosts(hosts));
        }
    }

    fn name_of(&self, addr: SocketAddr) -> Option<String> {
        self.seen.get(&addr).map(|(n, _)| n.clone())
    }
}

fn connect(
    beacon: Option<&UdpSocket>,
    discovery: &Discovery,
    steering: &Steering,
    finder: &mut Finder,
    events: &Sender<NetEvent>,
    round: u32,
) -> Option<TcpStream> {
    let (choice, picking) = {
        let s = steering.lock();
        (s.choice, s.picking)
    };
    // USB: the host keeps `adb reverse` active, so localhost reaches the PC. Not when
    // the user is picking a PC or picked one on the network: that choice comes first.
    let usb = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), prostir_proto::TCP_PORT);
    if !picking
        && choice.is_none_or(|c| c.ip().is_loopback())
        && let Some(stream) = try_connect(usb)
    {
        return Some(stream);
    }
    if let Some(addr) = choice {
        if let Some(stream) = try_connect(addr) {
            return Some(stream);
        }
        blocked(finder, events, addr);
    }
    if let Some(beacon) = beacon {
        finder.listen(beacon, events);
    }
    if picking || choice.is_some() {
        // The user is choosing (or their choice is unreachable): only listen.
        return None;
    }
    if let Some(stream) = discovery
        .host_file
        .as_ref()
        .and_then(read_addr)
        .and_then(try_connect)
    {
        return Some(stream);
    }
    if let Some(stream) = read_addr(&discovery.last_host_file).and_then(try_connect) {
        return Some(stream);
    }
    // Exactly one PC around: just use it. Several: wait for a pick in the menu.
    match finder.reported.as_slice() {
        [only] => {
            let addr = only.addr;
            let stream = try_connect(addr);
            if stream.is_none() {
                blocked(finder, events, addr);
            }
            return stream;
        }
        [] => {}
        _ => return None,
    }
    // Some networks drop broadcasts: now and then probe the local /24.
    if round % 5 == 1 { scan_subnet() } else { None }
}

/// The headset's own IPv4 address on the network the default route uses.
fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // UDP connect only picks a route; nothing is sent.
    socket
        .connect((Ipv4Addr::new(10, 255, 255, 254), 9))
        .or_else(|_| socket.connect((Ipv4Addr::new(192, 168, 255, 254), 9)))
        .ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

fn scan_subnet() -> Option<TcpStream> {
    let me = local_ipv4()?;
    let [a, b, c, own] = me.octets();
    let found = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for d in (1..=254u8).filter(|&d| d != own) {
            let found = &found;
            scope.spawn(move || {
                let addr =
                    SocketAddr::new(Ipv4Addr::new(a, b, c, d).into(), prostir_proto::TCP_PORT);
                if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
                    found.lock().unwrap().get_or_insert(stream);
                }
            });
        }
    });
    let stream = found.into_inner().unwrap()?;
    info!(
        "found host by scanning {a}.{b}.{c}.0/24: {:?}",
        stream.peer_addr()
    );
    Some(stream)
}

/// What pairing needs from the current connection.
struct Session<'a> {
    stream: &'a TcpStream,
    reader: &'a mut BufReader<SecureReader>,
    feedback: &'a Feedback,
    events: &'a Sender<NetEvent>,
    steering: &'a Steering,
    identity: &'a Identity,
    peer: &'a secure::Peer,
    host: &'a str,
}

impl Session<'_> {
    /// Pairs if the user agreed to; otherwise asks them and closes the connection
    /// (the PC must not be held waiting for an answer).
    fn pair_if_approved(&mut self, trusted: &mut TrustStore) -> std::io::Result<()> {
        if self.steering.approved(&self.peer.key) {
            return self.pair(trusted);
        }
        let addr = self.stream.peer_addr()?;
        self.steering.offer(self.peer.key, addr);
        let _ = self.events.send(NetEvent::PairOffer {
            host: offer_label(self.host, addr),
            failed: false,
        });
        Err(std::io::Error::other(
            "waiting for the user to approve pairing",
        ))
    }

    /// Pairs with the PC: up to [`PAIR_ATTEMPTS`] codes, then asks the user again.
    fn pair(&mut self, trusted: &mut TrustStore) -> std::io::Result<()> {
        // The user needs time to read and type the code.
        self.stream
            .set_read_timeout(Some(Duration::from_secs(300)))?;
        for _ in 0..PAIR_ATTEMPTS {
            let pc_shows = self.steering.pc_shows_code();
            // The PC makes its code only when asked, so ask first and then wait for
            // the user to type what it shows.
            self.feedback.send(&ClientMsg::PairBegin {
                headset_shows: !pc_shows,
                name: "Meta Quest".into(),
            })?;
            let code = if pc_shows {
                let _ = self.events.send(NetEvent::Pairing {
                    host: self.host.to_owned(),
                    code: None,
                });
                self.steering
                    .wait_code(self.stream)
                    .ok_or_else(|| std::io::Error::other("pairing interrupted"))?
            } else {
                let code = secure::pairing_code();
                let _ = self.events.send(NetEvent::Pairing {
                    host: self.host.to_owned(),
                    code: Some(format!("{} {}", &code[..3], &code[3..])),
                });
                code
            };
            let (pairing, msg) = Pairing::start(&code);
            self.feedback.send(&ClientMsg::PairMsg { msg })?;
            let theirs = loop {
                match HostMsg::read(self.reader)? {
                    HostMsg::PairMsg { msg } => break Some(msg),
                    // The PC gave up this attempt (e.g. its dialog was cancelled).
                    HostMsg::PairResult { .. } => break None,
                    // Not trusted yet: only a short line of plain text gets shown.
                    HostMsg::Error { text } => {
                        let text: String =
                            text.chars().filter(|c| !c.is_control()).take(160).collect();
                        let _ = self.events.send(NetEvent::HostError(text));
                    }
                    HostMsg::Unknown { .. } => {}
                    other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
                }
            };
            let Some(theirs) = theirs else {
                let _ = self.events.send(NetEvent::PairingFailed);
                continue;
            };
            let key = pairing.finish(&theirs)?;
            let own = &self.identity.public;
            self.feedback.send(&ClientMsg::PairConfirm {
                tag: confirmation(&key, "headset", self.peer, own),
            })?;
            match HostMsg::read(self.reader)? {
                HostMsg::PairResult { ok: true, tag }
                    if tags_match(&tag, &confirmation(&key, "pc", self.peer, own)) =>
                {
                    trusted.add(self.peer.key, self.host)?;
                    info!(
                        "paired with {} ({})",
                        self.host,
                        secure::fingerprint(&self.peer.key)
                    );
                    let _ = self.events.send(NetEvent::Paired);
                    self.stream
                        .set_read_timeout(Some(Duration::from_secs(10)))?;
                    return Ok(());
                }
                HostMsg::PairResult { .. } => {
                    warn!("pairing with {}: the codes did not match", self.host);
                    let _ = self.events.send(NetEvent::PairingFailed);
                }
                HostMsg::Unknown { .. } => {}
                other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
            }
        }
        // Several wrong codes: stop and let the user decide whether to go on.
        let addr = self.stream.peer_addr()?;
        self.steering.offer(self.peer.key, addr);
        let _ = self.events.send(NetEvent::PairOffer {
            host: offer_label(self.host, addr),
            failed: true,
        });
        Err(std::io::Error::other("pairing stopped after wrong codes"))
    }
}

struct StreamSlot {
    tx: SyncSender<Frame>,
    waiting_for_keyframe: bool,
}

/// How a session went, for what to do next.
#[derive(Default)]
struct Ended {
    /// The PC is trusted and the session started.
    welcomed: bool,
    /// When the PC last sent an error: right before it hung up means it cannot
    /// serve the headset right now.
    refused: Option<Instant>,
}

fn run_session(
    stream: TcpStream,
    events: &Sender<NetEvent>,
    name: Option<String>,
    steering: &Steering,
    identity: &Identity,
    trusted: &mut TrustStore,
    ended: &mut Ended,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (reader, writer, peer) = secure::handshake(&stream, identity, true)?;
    let mut reader = BufReader::with_capacity(1 << 20, reader);
    let feedback = Feedback::start(writer, stream.try_clone()?)?;
    // However the session ends, the socket closes: nothing stays blocked on it.
    struct CloseOnExit<'a>(&'a TcpStream);
    impl Drop for CloseOnExit<'_> {
        fn drop(&mut self) {
            let _ = self.0.shutdown(Shutdown::Both);
        }
    }
    let _close = CloseOnExit(&stream);
    let host = name.clone().unwrap_or_else(|| match stream.peer_addr() {
        Ok(a) if a.ip().is_loopback() => crate::i18n::current().t("ПК (USB)", "PC (USB)").into(),
        Ok(a) => a.ip().to_string(),
        Err(_) => crate::i18n::current().t("ПК", "PC").into(),
    });
    let mut session = Session {
        stream: &stream,
        reader: &mut reader,
        feedback: &feedback,
        events,
        steering,
        identity,
        peer: &peer,
        host: &host,
    };
    // A PC we have never paired with: pair only once the user said so in the menu,
    // and never on our own (a hostile PC on the network could keep guessing codes).
    if !trusted.contains(&peer.key) {
        session.pair_if_approved(trusted)?;
    }
    feedback.send(&ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        min: prostir_proto::MIN_PROTOCOL,
        app: env!("CARGO_PKG_VERSION").into(),
    })?;
    let language = loop {
        match HostMsg::read(session.reader)? {
            HostMsg::Welcome { language, .. } => break crate::i18n::Lang::from_code(&language),
            HostMsg::PairingRequired => {
                session.pair_if_approved(trusted)?;
                feedback.send(&ClientMsg::Hello {
                    version: PROTOCOL_VERSION,
                    min: prostir_proto::MIN_PROTOCOL,
                    app: env!("CARGO_PKG_VERSION").into(),
                })?;
            }
            // The text is for headsets that predate `Incompatible`; this one shows a page.
            HostMsg::Error { .. } => {}
            HostMsg::Incompatible { protocol, app } => {
                let _ = events.send(NetEvent::Incompatible {
                    pc_newer: protocol > PROTOCOL_VERSION,
                    pc_app: app,
                });
                // Nothing changes until one side is updated: try again only now and then.
                std::thread::sleep(Duration::from_secs(10));
                return Ok(());
            }
            HostMsg::Unknown { .. } => {}
            other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
        }
    };
    ended.welcomed = true;
    // The host re-sends the last frame every 500 ms, so 10 s of silence means a dead link.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    if let Ok(peer) = stream.peer_addr() {
        let _ = events.send(NetEvent::Connected {
            addr: peer,
            name,
            feedback: feedback.clone(),
            language,
        });
    }

    let mut streams: HashMap<u8, StreamSlot> = HashMap::new();
    // Started on the first sound slice, stopped with the session.
    let mut player: Option<crate::audio::Player> = None;
    // Runs only while a PC app records from the headset microphone and the user's
    // microphone switch on the headset is on.
    let mut recorder: Option<crate::mic::Recorder> = None;
    let mut mic_asked = false;
    let result = loop {
        let msg = match HostMsg::read(&mut reader) {
            Ok(m) => m,
            Err(e) => break Err(e),
        };
        match msg {
            HostMsg::StreamStart {
                id,
                width,
                height,
                codec,
                name,
            } => {
                info!("monitor {name}: {width}x{height} {codec:?}");
                if streams.remove(&id).is_some() {
                    let _ = events.send(NetEvent::StreamEnd { id, removed: false });
                }
                let (tx, rx) = mpsc::sync_channel(32);
                streams.insert(
                    id,
                    StreamSlot {
                        tx,
                        waiting_for_keyframe: true,
                    },
                );
                let _ = events.send(NetEvent::StreamStart {
                    id,
                    width,
                    height,
                    codec,
                    frames: rx,
                    feedback: feedback.clone(),
                });
            }
            HostMsg::Frame {
                id,
                pts_us,
                keyframe,
                data,
            } => {
                let Some(slot) = streams.get_mut(&id) else {
                    continue;
                };
                if slot.waiting_for_keyframe && !keyframe {
                    continue;
                }
                match slot.tx.try_send(Frame {
                    received: std::time::Instant::now(),
                    pts_us,
                    keyframe,
                    data,
                }) {
                    Ok(()) => slot.waiting_for_keyframe = false,
                    Err(TrySendError::Full(_)) => {
                        warn!("decoder {id} is behind, dropping until next keyframe");
                        slot.waiting_for_keyframe = true;
                        feedback.send(&ClientMsg::RequestKeyframe { id })?;
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
            }
            HostMsg::StreamEnd { id } => {
                if streams.remove(&id).is_some() {
                    let _ = events.send(NetEvent::StreamEnd { id, removed: true });
                }
            }
            HostMsg::PrimaryChanged { id } => {
                let _ = events.send(NetEvent::PrimaryChanged(id));
            }
            HostMsg::MirrorWanted { enabled } => {
                let _ = events.send(NetEvent::MirrorWanted(enabled));
            }
            HostMsg::Focus { id } => {
                let _ = events.send(NetEvent::Focus((id != NO_FOCUS).then_some(id)));
            }
            HostMsg::Order { ids } => {
                let _ = events.send(NetEvent::Order(ids));
            }
            HostMsg::Error { text } => {
                ended.refused = Some(Instant::now());
                let _ = events.send(NetEvent::HostError(text));
            }
            HostMsg::Audio { pcm } => {
                player
                    .get_or_insert_with(crate::audio::Player::spawn)
                    .push(pcm);
            }
            HostMsg::AudioWanted { enabled } => {
                if !enabled {
                    player = None;
                }
                let _ = events.send(NetEvent::AudioWanted(enabled));
            }
            // The headset's own switch decides about its microphone.
            HostMsg::MicWanted { .. } => {}
            HostMsg::PcSettings { settings, encoders } => {
                let _ = events.send(NetEvent::PcSettings(settings, encoders));
            }
            HostMsg::Recenter => {
                let _ = events.send(NetEvent::Recenter);
            }
            HostMsg::Notice { text } => {
                let _ = events.send(NetEvent::Notice(text));
            }
            // The PC asks; the headset's own switch decides (checked below too).
            HostMsg::MicActive { active } => mic_asked = active,
            // From a newer PC: skipped, the session goes on.
            HostMsg::Unknown { .. }
            | HostMsg::Welcome { .. }
            | HostMsg::Incompatible { .. }
            | HostMsg::PairingRequired
            | HostMsg::PairMsg { .. }
            | HostMsg::PairResult { .. } => {}
        }
        let record = mic_asked && crate::mic::allowed();
        if record != recorder.is_some() {
            recorder = record.then(|| crate::mic::Recorder::start(feedback.clone()));
        }
    };
    for id in streams.into_keys() {
        let _ = events.send(NetEvent::StreamEnd { id, removed: false });
    }
    result
}
