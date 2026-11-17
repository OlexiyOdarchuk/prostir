//! Finds the PC host, keeps the TCP connection alive and fans frames out to decoders.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{info, warn};
use qmon_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION};

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

pub enum NetEvent {
    /// Connected; the handle sends requests (add monitor…) to the host.
    Connected(SocketAddr, Feedback),
    PrimaryChanged(u8),
    MirrorWanted(bool),
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

pub fn spawn(events: Sender<NetEvent>, discovery: Discovery) {
    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let beacon = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, qmon_proto::DISCOVERY_PORT))
                .inspect_err(|e| warn!("discovery socket: {e}"))
                .ok();
            loop {
                if let Some(stream) = connect(beacon.as_ref(), &discovery) {
                    let peer = stream.peer_addr().ok();
                    if let Some(p) = peer.filter(|p| !p.ip().is_loopback()) {
                        let _ = std::fs::write(&discovery.last_host_file, p.to_string());
                    }
                    match run_session(stream, &events) {
                        Ok(()) => info!("host {peer:?} closed the connection"),
                        Err(e) => warn!("connection to {peer:?} lost: {e}"),
                    }
                    if events.send(NetEvent::Disconnected).is_err() {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })
        .expect("spawn net thread");
}

fn try_connect(addr: SocketAddr) -> Option<TcpStream> {
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok()
}

fn read_addr(path: &PathBuf) -> Option<SocketAddr> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    text.parse::<SocketAddr>().ok().or_else(|| {
        text.parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, qmon_proto::TCP_PORT))
    })
}

fn connect(beacon: Option<&UdpSocket>, discovery: &Discovery) -> Option<TcpStream> {
    if let Some(stream) = discovery
        .host_file
        .as_ref()
        .and_then(read_addr)
        .and_then(try_connect)
    {
        return Some(stream);
    }
    // USB: the host keeps `adb reverse` active, so localhost reaches the PC.
    if let Some(stream) = try_connect(SocketAddr::new(
        Ipv4Addr::LOCALHOST.into(),
        qmon_proto::TCP_PORT,
    )) {
        return Some(stream);
    }
    if let Some(stream) = read_addr(&discovery.last_host_file).and_then(try_connect) {
        return Some(stream);
    }
    // Wi-Fi: wait for the host's broadcast beacon.
    if let Some(beacon) = beacon {
        let _ = beacon.set_read_timeout(Some(Duration::from_millis(1500)));
        let mut buf = [0u8; 512];
        if let Ok((n, from)) = beacon.recv_from(&mut buf)
            && let Some((port, name)) = qmon_proto::decode_beacon(&buf[..n])
        {
            info!("found host '{name}' at {}", from.ip());
            if let Some(stream) = try_connect(SocketAddr::new(from.ip(), port)) {
                return Some(stream);
            }
        }
    }
    // Some networks drop broadcasts: probe every address of the local /24.
    scan_subnet()
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
                let addr = SocketAddr::new(Ipv4Addr::new(a, b, c, d).into(), qmon_proto::TCP_PORT);
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

fn run_session(stream: TcpStream, events: &Sender<NetEvent>) -> std::io::Result<()> {
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
        let _ = events.send(NetEvent::Connected(peer, feedback.clone()));
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
