//! Finds the PC host, keeps the TCP connection alive and fans frames out to decoders.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};
use prostir_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION};

pub struct Frame {
    /// When the frame arrived from the network.
    pub received: std::time::Instant,
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Sends small messages back to the host from any thread (net, decoders).
#[derive(Clone)]
pub struct Feedback(Arc<Mutex<TcpStream>>);

impl Feedback {
    pub fn send(&self, msg: &ClientMsg) -> std::io::Result<()> {
        let mut stream = self.0.lock().unwrap();
        // One write per message: TCP_NODELAY sends it right away.
        let mut buf = Vec::with_capacity(16);
        msg.write(&mut buf)?;
        stream.write_all(&buf)
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
    },
    /// PCs currently seen on the network (sent when the list changes).
    Hosts(Vec<FoundHost>),
    PrimaryChanged(u8),
    MirrorWanted(bool),
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
    Disconnected,
}

/// Where to look for the PC.
pub struct Discovery {
    /// User-provided `IP` or `IP:PORT`, tried first.
    pub host_file: Option<PathBuf>,
    /// Address of the last host that worked over the network, tried before discovery.
    pub last_host_file: PathBuf,
}

/// Lets the menu choose which PC to use.
#[derive(Clone, Default)]
pub struct Steering(Arc<Mutex<Steer>>);

#[derive(Default)]
struct Steer {
    /// Picked in the menu (or typed in); tried before anything else.
    choice: Option<SocketAddr>,
    /// "Change PC": do not reconnect on our own until the user picks one.
    picking: bool,
    current: Option<TcpStream>,
}

impl Steering {
    /// Connects to `addr`, dropping the current connection if it is another PC.
    pub fn connect_to(&self, addr: SocketAddr) {
        let mut s = self.0.lock().unwrap();
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
        let mut s = self.0.lock().unwrap();
        s.choice = None;
        s.picking = true;
        if let Some(cur) = s.current.take() {
            let _ = cur.shutdown(Shutdown::Both);
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
            let mut finder = Finder::default();
            let mut round = 0u32;
            loop {
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
                    if let Some(p) = peer.filter(|p| !p.ip().is_loopback()) {
                        let _ = std::fs::write(&discovery.last_host_file, p.to_string());
                    }
                    let name = peer.and_then(|p| finder.name_of(p));
                    steering.0.lock().unwrap().current = stream.try_clone().ok();
                    match run_session(stream, &events, name) {
                        Ok(()) => info!("host {peer:?} closed the connection"),
                        Err(e) => warn!("connection to {peer:?} lost: {e}"),
                    }
                    steering.0.lock().unwrap().current = None;
                    if events.send(NetEvent::Disconnected).is_err() {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        })
        .expect("spawn net thread");
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
    // USB: the host keeps `adb reverse` active, so localhost reaches the PC.
    if let Some(stream) = try_connect(SocketAddr::new(
        Ipv4Addr::LOCALHOST.into(),
        prostir_proto::TCP_PORT,
    )) {
        return Some(stream);
    }
    let (choice, picking) = {
        let s = steering.0.lock().unwrap();
        (s.choice, s.picking)
    };
    if let Some(stream) = choice.and_then(try_connect) {
        return Some(stream);
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
        [only] => return try_connect(only.addr),
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

struct StreamSlot {
    tx: SyncSender<Frame>,
    waiting_for_keyframe: bool,
}

fn run_session(
    stream: TcpStream,
    events: &Sender<NetEvent>,
    name: Option<String>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let feedback = Feedback(Arc::new(Mutex::new(stream.try_clone()?)));
    feedback.send(&ClientMsg::Hello {
        version: PROTOCOL_VERSION,
    })?;
    let mut reader = BufReader::with_capacity(1 << 20, stream.try_clone()?);
    match HostMsg::read(&mut reader)? {
        HostMsg::Welcome { .. } => {}
        other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
    }
    // The host re-sends the last frame every 500 ms, so 10 s of silence means a dead link.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    if let Ok(peer) = stream.peer_addr() {
        let _ = events.send(NetEvent::Connected {
            addr: peer,
            name,
            feedback: feedback.clone(),
        });
    }

    let mut streams: HashMap<u8, StreamSlot> = HashMap::new();
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
            HostMsg::Order { ids } => {
                let _ = events.send(NetEvent::Order(ids));
            }
            HostMsg::Error { text } => {
                let _ = events.send(NetEvent::HostError(text));
            }
            HostMsg::Welcome { .. } => {}
        }
    };
    for id in streams.into_keys() {
        let _ = events.send(NetEvent::StreamEnd { id, removed: false });
    }
    result
}
