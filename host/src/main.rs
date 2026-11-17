mod displays;
mod encoder;
mod virtual_output;

use std::io::{BufReader, BufWriter, ErrorKind, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use log::{error, info, warn};
use qmon_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION};

use displays::{Displays, SavedMonitors};
use encoder::EncoderConfig;
use virtual_output::MonitorSpec;

#[derive(Parser, Debug)]
#[command(about = "Virtual KDE Plasma monitors streamed to a Meta Quest 3")]
struct Args {
    /// Monitor in pixels, optionally with KWin scale: 2560x1440 or 3840x2160@1.5. Repeat for
    /// several monitors. Without it, the set from the last session (changed from the headset
    /// menu) is used, or a single 2560x1440.
    #[arg(short, long = "monitor", value_parser = parse_monitor)]
    monitors: Vec<MonitorSpec>,
    /// Leave KDE's primary monitor alone (by default the middle Quest monitor becomes primary).
    #[arg(long)]
    keep_primary: bool,
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
    let spec = MonitorSpec {
        width,
        height,
        scale,
    };
    spec.validate().map_err(|e| e.to_string())?;
    Ok(spec)
}

/// Validates the client's greeting before anything (like creating monitors) happens.
fn handshake(stream: &TcpStream) -> Result<BufReader<TcpStream>> {
    stream.set_nodelay(true)?;
    // Over Wi-Fi a vanished headset would otherwise block writes for minutes and
    // keep a reconnecting headset waiting in the accept queue.
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    match ClientMsg::read(&mut reader)? {
        ClientMsg::Hello { version } if version == PROTOCOL_VERSION => {}
        ClientMsg::Hello { version } => {
            bail!("client speaks protocol v{version}, host v{PROTOCOL_VERSION}")
        }
        other => bail!("unexpected first message {other:?}"),
    }
    stream.set_read_timeout(None)?;
    Ok(reader)
}

fn serve_client(
    stream: TcpStream,
    mut reader: BufReader<TcpStream>,
    displays: &mut Displays,
) -> Result<()> {
    let mut writer = BufWriter::with_capacity(1 << 20, stream.try_clone()?);
    HostMsg::Welcome {
        version: PROTOCOL_VERSION,
    }
    .write(&mut writer)?;
    writer.flush()?;

    let (tx, rx) = mpsc::sync_channel::<HostMsg>(16);
    let writer_thread = {
        let stream = stream.try_clone()?;
        std::thread::Builder::new()
            .name("net-writer".into())
            .spawn(move || {
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
    std::thread::Builder::new()
        .name("net-reader".into())
        .spawn(move || {
            loop {
                let msg = ClientMsg::read(&mut reader);
                let stop = msg.is_err();
                if msg_tx.send(msg).is_err() || stop {
                    break;
                }
            }
        })?;

    displays.router.attach(tx);
    if let Some(id) = displays.primary() {
        displays.router.send(HostMsg::PrimaryChanged { id });
    }
    displays.router.send(HostMsg::MirrorWanted {
        enabled: displays.mirror_wanted(),
    });
    displays.force_keyframes();

    let result = loop {
        match msg_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(ClientMsg::RequestKeyframe { id })) => displays.force_keyframe(id),
            Ok(Ok(ClientMsg::FrameShown { id, pts_us })) => displays.router.frame_shown(id, pts_us),
            Ok(Ok(ClientMsg::AddMonitor { width, height })) => {
                let spec = MonitorSpec {
                    width,
                    height,
                    scale: 1.0,
                };
                match displays.add(spec) {
                    Ok(id) => {
                        info!("headset added Quest-{}", id + 1);
                        displays.force_keyframe(id);
                        displays.save();
                        displays::sync_wallpapers();
                    }
                    Err(e) => report(displays, format!("{e:#}")),
                }
            }
            Ok(Ok(ClientMsg::RemoveMonitor { id })) => match displays.remove(id) {
                Ok(()) => {
                    info!("headset removed Quest-{}", id + 1);
                    displays.save();
                }
                Err(e) => report(displays, format!("{e:#}")),
            },
            Ok(Ok(ClientMsg::SetMirror { enabled })) => {
                displays.set_mirror(enabled);
                displays.save();
            }
            Ok(Ok(ClientMsg::SetPrimary { id })) => {
                displays.set_primary(id);
                displays.save();
                displays::sync_wallpapers();
            }
            Ok(Ok(other)) => warn!("ignoring {other:?}"),
            Ok(Err(e)) if e.kind() == ErrorKind::UnexpectedEof => break Ok(()),
            Ok(Err(e)) => break Err(e.into()),
            Err(RecvTimeoutError::Timeout) => {
                displays.tick();
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

/// Logs a failed request and shows it in the headset menu.
fn report(displays: &Displays, text: String) {
    warn!("{text}");
    displays.router.send(HostMsg::Error { text });
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "linux".into())
}

fn spawn_discovery(port: u16) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let beacon = qmon_proto::encode_beacon(port, &hostname());
    std::thread::Builder::new()
        .name("discovery".into())
        .spawn(move || {
            loop {
                // 255.255.255.255 only leaves through the default route, so also send to the
                // broadcast address of every interface (Wi-Fi, Ethernet, hotspot).
                let mut targets = broadcast_addresses();
                targets.push(Ipv4Addr::BROADCAST);
                for ip in targets {
                    let _ = socket.send_to(&beacon, (ip, qmon_proto::DISCOVERY_PORT));
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })?;
    Ok(())
}

/// IPv4 broadcast addresses of all interfaces that are up.
fn broadcast_addresses() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, which is walked read-only and freed once.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while let Some(ifa) = cur.as_ref() {
            let flags = ifa.ifa_flags;
            let up = flags & libc::IFF_UP as u32 != 0
                && flags & libc::IFF_BROADCAST as u32 != 0
                && flags & libc::IFF_LOOPBACK as u32 == 0;
            let addr = ifa.ifa_ifu; // broadcast address when IFF_BROADCAST is set
            if up && !addr.is_null() && i32::from((*addr).sa_family) == libc::AF_INET {
                let sin = &*(addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !out.contains(&ip) {
                    out.push(ip);
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Keeps `adb reverse` set up so a headset on USB reaches the host via 127.0.0.1.
fn spawn_adb_reverse(port: u16) {
    let _ = std::thread::Builder::new()
        .name("adb-reverse".into())
        .spawn(move || {
            let spec = format!("tcp:{port}");
            let mut active: Vec<String> = Vec::new();
            loop {
                let Ok(out) = std::process::Command::new("adb").arg("devices").output() else {
                    return; // no adb installed
                };
                // Only USB devices: "serial\tdevice"; adb-over-Wi-Fi serials are "ip:port".
                let usb: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(|l| l.strip_suffix("\tdevice"))
                    .filter(|serial| !serial.contains(':'))
                    .map(str::to_owned)
                    .collect();
                for serial in &usb {
                    let ok = std::process::Command::new("adb")
                        .args(["-s", serial, "reverse", &spec, &spec])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success());
                    if ok && !active.contains(serial) {
                        info!("USB headset {serial}: adb reverse {spec} active");
                    }
                }
                active = usb;
                std::thread::sleep(Duration::from_secs(3));
            }
        });
}

fn create_displays(args: &Args) -> Result<Displays> {
    let codec = if args.software {
        Codec::H264
    } else {
        match args.codec {
            CodecArg::H264 => Codec::H264,
            CodecArg::H265 => Codec::H265,
        }
    };
    let cfg = EncoderConfig {
        codec,
        bitrate_kbps: args.bitrate * 1000,
        software: args.software,
    };
    let stored = displays::load_saved(parse_monitor);
    let mirror = stored.as_ref().is_none_or(|s| s.mirror);
    let saved = match stored {
        Some(saved) if args.monitors.is_empty() => saved,
        _ if !args.monitors.is_empty() => SavedMonitors {
            specs: args.monitors.clone(),
            primary: None,
            mirror,
        },
        _ => SavedMonitors {
            specs: vec![parse_monitor("2560x1440").expect("valid default")],
            primary: None,
            mirror,
        },
    };
    let displays = Displays::create(&saved, cfg, !args.keep_primary)?;
    displays.save();
    Ok(displays)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    gstreamer::init().context("GStreamer init")?;

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, args.port))
        .with_context(|| format!("cannot listen on port {}", args.port))?;
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
                let reader = match handshake(&stream) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("rejected connection from {addr}: {e:#}");
                        continue;
                    }
                };
                let via = if addr.ip().is_loopback() {
                    "USB"
                } else {
                    "Wi-Fi"
                };
                info!("headset connected via {via} from {addr}");
                if displays.as_ref().is_some_and(|d| !d.healthy()) {
                    displays = None;
                }
                if displays.is_none() {
                    match create_displays(&args) {
                        Ok(d) => displays = Some(d),
                        Err(e) => {
                            error!("{e:#}");
                            continue;
                        }
                    }
                }
                if let Err(e) = serve_client(stream, reader, displays.as_mut().unwrap()) {
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
