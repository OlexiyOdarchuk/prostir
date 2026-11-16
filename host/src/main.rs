mod encoder;
mod virtual_output;

use std::io::{BufReader, BufWriter, ErrorKind, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use log::{error, info, warn};
use qmon_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION};

use encoder::{Encoder, EncoderConfig, EncoderEvent};
use virtual_output::{MonitorSpec, VirtualOutputs};

#[derive(Parser, Debug)]
#[command(about = "Virtual KDE Plasma monitors streamed to a Meta Quest 3")]
struct Args {
    /// Monitor in pixels, optionally with KWin scale: 2560x1440 or 3840x2160@1.5. Repeat for several monitors.
    #[arg(short, long = "monitor", value_parser = parse_monitor, default_value = "2560x1440")]
    monitors: Vec<MonitorSpec>,
    /// Video codec.
    #[arg(long, value_enum, default_value_t = CodecArg::H265)]
    codec: CodecArg,
    /// Bitrate per monitor, Mbit/s.
    #[arg(long, default_value_t = 30)]
    bitrate: u32,
    /// Software x264 encoder instead of VA-API (implies --codec h264).
    #[arg(long)]
    software: bool,
    #[arg(long, default_value_t = qmon_proto::TCP_PORT)]
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
    let (size, scale) = match s.split_once('@') {
        Some((a, b)) => (a, b.parse::<f64>().map_err(|e| format!("bad scale: {e}"))?),
        None => (s, 1.0),
    };
    let (w, h) = size.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
    let width: u32 = w.parse().map_err(|e| format!("bad width: {e}"))?;
    let height: u32 = h.parse().map_err(|e| format!("bad height: {e}"))?;
    if !(320..=7680).contains(&width) || !(240..=4320).contains(&height) || !(0.5..=4.0).contains(&scale) {
        return Err("monitor size or scale out of range".into());
    }
    Ok(MonitorSpec { width, height, scale })
}

/// Per-stream delivery state for the currently connected client.
struct StreamSlot {
    size: Option<(u32, u32)>,
    announced: bool,
    need_keyframe: bool,
}

/// Delivers encoder output to whichever client is connected.
struct Router {
    codec: Codec,
    inner: Mutex<RouterInner>,
    failed: AtomicBool,
}

struct RouterInner {
    tx: Option<SyncSender<HostMsg>>,
    streams: Vec<StreamSlot>,
}

impl Router {
    fn new(codec: Codec, count: usize) -> Self {
        let streams = (0..count).map(|_| StreamSlot { size: None, announced: false, need_keyframe: true }).collect();
        Self { codec, inner: Mutex::new(RouterInner { tx: None, streams }), failed: AtomicBool::new(false) }
    }

    fn attach(&self, tx: SyncSender<HostMsg>) {
        let mut inner = self.inner.lock().unwrap();
        inner.tx = Some(tx);
        for s in &mut inner.streams {
            s.announced = false;
            s.need_keyframe = true;
        }
    }

    fn detach(&self) {
        self.inner.lock().unwrap().tx = None;
    }

    /// Returns `true` when the encoder should produce a keyframe.
    fn on_event(&self, id: usize, event: EncoderEvent) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let RouterInner { tx, streams } = &mut *inner;
        let slot = &mut streams[id];
        match event {
            EncoderEvent::Size { width, height } => {
                info!("monitor {}: {width}x{height}", id + 1);
                slot.size = Some((width, height));
                slot.announced = false;
                slot.need_keyframe = true;
                true
            }
            EncoderEvent::Failed(reason) => {
                error!("monitor {}: {reason}", id + 1);
                self.failed.store(true, Ordering::Relaxed);
                false
            }
            EncoderEvent::Frame { pts_us, keyframe, data } => {
                let Some(sender) = tx else { return false };
                if !slot.announced {
                    let Some((width, height)) = slot.size else { return true };
                    let start = HostMsg::StreamStart {
                        id: id as u8,
                        width,
                        height,
                        codec: self.codec,
                        name: format!("Quest-{}", id + 1),
                    };
                    if sender.send(start).is_err() {
                        *tx = None;
                        return false;
                    }
                    slot.announced = true;
                }
                if slot.need_keyframe && !keyframe {
                    return true;
                }
                match sender.try_send(HostMsg::Frame { id: id as u8, pts_us, keyframe, data }) {
                    Ok(()) => {
                        slot.need_keyframe = false;
                        false
                    }
                    Err(TrySendError::Full(_)) => {
                        // The network can't keep up: drop until the next keyframe so the
                        // decoder never sees a broken reference chain.
                        slot.need_keyframe = true;
                        true
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        *tx = None;
                        false
                    }
                }
            }
        }
    }
}

struct Displays {
    // Field order matters: encoders must stop before the outputs go away.
    encoders: Vec<Encoder>,
    outputs: VirtualOutputs,
    router: Arc<Router>,
}

impl Displays {
    fn create(args: &Args) -> Result<Self> {
        let codec = if args.software { Codec::H264 } else { match args.codec { CodecArg::H264 => Codec::H264, CodecArg::H265 => Codec::H265 } };
        let outputs = VirtualOutputs::create(&args.monitors)?;
        let router = Arc::new(Router::new(codec, args.monitors.len()));
        let cfg = EncoderConfig { codec, bitrate_kbps: args.bitrate * 1000, software: args.software };
        let mut encoders = Vec::new();
        for (id, &node) in outputs.nodes.iter().enumerate() {
            let router = router.clone();
            encoders.push(Encoder::start(node, &cfg, move |ev| router.on_event(id, ev))?);
        }
        Ok(Self { encoders, outputs, router })
    }

    fn healthy(&self) -> bool {
        !self.router.failed.load(Ordering::Relaxed) && !self.outputs.closed.iter().any(|c| c.load(Ordering::Relaxed))
    }
}

fn serve_client(stream: TcpStream, displays: &Displays) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    match ClientMsg::read(&mut reader)? {
        ClientMsg::Hello { version } if version == PROTOCOL_VERSION => {}
        ClientMsg::Hello { version } => bail!("client speaks protocol v{version}, host v{PROTOCOL_VERSION}"),
        other => bail!("unexpected first message {other:?}"),
    }
    stream.set_read_timeout(None)?;
    let mut writer = BufWriter::with_capacity(1 << 20, stream.try_clone()?);
    HostMsg::Welcome { version: PROTOCOL_VERSION }.write(&mut writer)?;
    writer.flush()?;

    let (tx, rx) = mpsc::sync_channel::<HostMsg>(16);
    let writer_thread = {
        let stream = stream.try_clone()?;
        std::thread::Builder::new().name("net-writer".into()).spawn(move || {
            let result = (|| -> std::io::Result<()> {
                while let Ok(msg) = rx.recv() {
                    msg.write(&mut writer)?;
                    while let Ok(msg) = rx.try_recv() {
                        msg.write(&mut writer)?;
                    }
                    writer.flush()?;
                }
                Ok(())
            })();
            if let Err(e) = result {
                info!("connection closed while sending: {e}");
            }
            let _ = stream.shutdown(Shutdown::Both);
        })?
    };

    let (msg_tx, msg_rx) = mpsc::channel::<std::io::Result<ClientMsg>>();
    std::thread::Builder::new().name("net-reader".into()).spawn(move || {
        loop {
            let msg = ClientMsg::read(&mut reader);
            let stop = msg.is_err();
            if msg_tx.send(msg).is_err() || stop {
                break;
            }
        }
    })?;

    displays.router.attach(tx);
    for e in &displays.encoders {
        e.force_keyframe();
    }

    let result = loop {
        match msg_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(ClientMsg::RequestKeyframe { id })) => {
                if let Some(e) = displays.encoders.get(id as usize) {
                    e.force_keyframe();
                }
            }
            Ok(Ok(other)) => warn!("ignoring {other:?}"),
            Ok(Err(e)) if e.kind() == ErrorKind::UnexpectedEof => break Ok(()),
            Ok(Err(e)) => break Err(e.into()),
            Err(RecvTimeoutError::Timeout) => {
                if !displays.healthy() {
                    break Err(anyhow::anyhow!("virtual monitor or encoder stopped"));
                }
            }
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    displays.router.detach();
    let _ = stream.shutdown(Shutdown::Both);
    let _ = writer_thread.join();
    result
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "linux".into())
}

fn spawn_discovery(port: u16) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let beacon = qmon_proto::encode_beacon(port, &hostname());
    std::thread::Builder::new().name("discovery".into()).spawn(move || {
        loop {
            let _ = socket.send_to(&beacon, (Ipv4Addr::BROADCAST, qmon_proto::DISCOVERY_PORT));
            std::thread::sleep(Duration::from_secs(1));
        }
    })?;
    Ok(())
}

/// Keeps `adb reverse` set up so a headset on USB reaches the host via 127.0.0.1.
fn spawn_adb_reverse(port: u16) {
    let _ = std::thread::Builder::new().name("adb-reverse".into()).spawn(move || {
        let mut connected = false;
        loop {
            let spec = format!("tcp:{port}");
            let ok = std::process::Command::new("adb")
                .args(["reverse", &spec, &spec])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success());
            match ok {
                Err(_) => return, // no adb installed
                Ok(true) if !connected => {
                    info!("USB headset detected, adb reverse {spec} active");
                    connected = true;
                }
                Ok(false) => connected = false,
                _ => {}
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    gstreamer::init().context("GStreamer init")?;

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, args.port)).with_context(|| format!("cannot listen on port {}", args.port))?;
    listener.set_nonblocking(true)?;
    info!("waiting for the headset on port {}", args.port);
    if !args.no_discovery {
        spawn_discovery(args.port)?;
    }
    if !args.no_adb {
        spawn_adb_reverse(args.port);
    }

    let mut displays: Option<Displays> = None;
    let mut idle_since = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                stream.set_nonblocking(false)?;
                info!("headset connected from {addr}");
                if displays.as_ref().is_some_and(|d| !d.healthy()) {
                    displays = None;
                }
                if displays.is_none() {
                    match Displays::create(&args) {
                        Ok(d) => displays = Some(d),
                        Err(e) => {
                            error!("{e:#}");
                            continue;
                        }
                    }
                }
                if let Err(e) = serve_client(stream, displays.as_ref().unwrap()) {
                    warn!("session ended: {e:#}");
                } else {
                    info!("headset disconnected");
                }
                idle_since = Instant::now();
                if args.linger > 0 && displays.as_ref().is_some_and(|d| d.healthy()) {
                    info!("keeping virtual monitors for {} s", args.linger);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if displays.is_some() && idle_since.elapsed() >= Duration::from_secs(args.linger) {
                    displays = None;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.into()),
        }
    }
}
