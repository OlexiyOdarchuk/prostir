//! Finds the PC host, keeps the TCP connection alive and fans frames out to decoders.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::time::Duration;

use log::{info, warn};
use qmon_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION};

pub struct Frame {
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub enum NetEvent {
    Connected(SocketAddr),
    StreamStart { id: u8, width: u32, height: u32, codec: Codec, frames: Receiver<Frame> },
    StreamEnd { id: u8 },
    Disconnected,
}

/// `host_file` may contain `IP` or `IP:PORT` to skip discovery.
pub fn spawn(events: Sender<NetEvent>, host_file: Option<PathBuf>) {
    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let beacon = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, qmon_proto::DISCOVERY_PORT))
                .inspect_err(|e| warn!("discovery socket: {e}"))
                .ok();
            loop {
                if let Some(stream) = connect(beacon.as_ref(), host_file.as_ref()) {
                    let peer = stream.peer_addr().ok();
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

fn connect(beacon: Option<&UdpSocket>, host_file: Option<&PathBuf>) -> Option<TcpStream> {
    if let Some(text) = host_file.and_then(|p| std::fs::read_to_string(p).ok()) {
        let text = text.trim();
        let addr = text
            .parse::<SocketAddr>()
            .ok()
            .or_else(|| text.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, qmon_proto::TCP_PORT)));
        if let Some(stream) = addr.and_then(try_connect) {
            return Some(stream);
        }
    }
    // USB: the host keeps `adb reverse` active, so localhost reaches the PC.
    if let Some(stream) = try_connect(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), qmon_proto::TCP_PORT)) {
        return Some(stream);
    }
    // Wi-Fi: wait for a broadcast beacon.
    let beacon = beacon?;
    beacon.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut buf = [0u8; 512];
    let (n, from) = beacon.recv_from(&mut buf).ok()?;
    let (port, name) = qmon_proto::decode_beacon(&buf[..n])?;
    info!("found host '{name}' at {}", from.ip());
    try_connect(SocketAddr::new(from.ip(), port))
}

struct StreamSlot {
    tx: SyncSender<Frame>,
    waiting_for_keyframe: bool,
}

fn run_session(stream: TcpStream, events: &Sender<NetEvent>) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    ClientMsg::Hello { version: PROTOCOL_VERSION }.write(&mut writer)?;
    let mut reader = BufReader::with_capacity(1 << 20, stream.try_clone()?);
    match HostMsg::read(&mut reader)? {
        HostMsg::Welcome { .. } => {}
        other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
    }
    // The host re-sends the last frame every 500 ms, so 10 s of silence means a dead link.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    if let Ok(peer) = stream.peer_addr() {
        let _ = events.send(NetEvent::Connected(peer));
    }

    let mut streams: HashMap<u8, StreamSlot> = HashMap::new();
    let result = loop {
        let msg = match HostMsg::read(&mut reader) {
            Ok(m) => m,
            Err(e) => break Err(e),
        };
        match msg {
            HostMsg::StreamStart { id, width, height, codec, name } => {
                info!("monitor {name}: {width}x{height} {codec:?}");
                if streams.remove(&id).is_some() {
                    let _ = events.send(NetEvent::StreamEnd { id });
                }
                let (tx, rx) = mpsc::sync_channel(32);
                streams.insert(id, StreamSlot { tx, waiting_for_keyframe: true });
                let _ = events.send(NetEvent::StreamStart { id, width, height, codec, frames: rx });
            }
            HostMsg::Frame { id, pts_us, keyframe, data } => {
                let Some(slot) = streams.get_mut(&id) else { continue };
                if slot.waiting_for_keyframe && !keyframe {
                    continue;
                }
                match slot.tx.try_send(Frame { pts_us, keyframe, data }) {
                    Ok(()) => slot.waiting_for_keyframe = false,
                    Err(TrySendError::Full(_)) => {
                        warn!("decoder {id} is behind, dropping until next keyframe");
                        slot.waiting_for_keyframe = true;
                        ClientMsg::RequestKeyframe { id }.write(&mut writer)?;
                        writer.flush()?;
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
            }
            HostMsg::StreamEnd { id } => {
                if streams.remove(&id).is_some() {
                    let _ = events.send(NetEvent::StreamEnd { id });
                }
            }
            HostMsg::Welcome { .. } => {}
        }
    };
    for id in streams.into_keys() {
        let _ = events.send(NetEvent::StreamEnd { id });
    }
    result
}
