// Every unsafe block says why it is sound.
#![deny(clippy::undocumented_unsafe_blocks)]
mod audio;
mod displays;
mod encoder;
mod focus;
mod i18n;
mod pairing;
mod tray;
mod usage;
mod virtual_output;

use std::io::{BufReader, BufWriter, ErrorKind, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use log::{error, info, warn};
use prostir_proto::secure::{SecureReader, SecureWriter};
use prostir_proto::{ClientMsg, Codec, HostMsg, NO_FOCUS};

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
    /// Bitrate per monitor, Mbit/s (default: the quality chosen in the tray, or 30).
    #[arg(long)]
    bitrate: Option<u32>,
    /// Software x264 encoder instead of VA-API (implies --codec h264).
    #[arg(long)]
    software: bool,
    #[arg(long, default_value_t = prostir_proto::TCP_PORT)]
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

/// Tray icon and the commands it sends.
struct Control {
    tray: Option<tray::Handle>,
    security: pairing::Security,
    commands: Receiver<tray::Command>,
    bitrate: u32,
    sharp: bool,
    quit: bool,
    shown: Option<tray::Status>,
}

impl Control {
    /// Applies tray commands; returns `true` when asked to quit.
    fn poll(&mut self, displays: Option<&mut Displays>) -> bool {
        let mut displays = displays;
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                tray::Command::SetBitrate(mbps) => {
                    info!("video quality: {mbps} Mbit/s per monitor");
                    self.bitrate = mbps;
                    if let Some(d) = displays.as_deref_mut() {
                        d.set_bitrate(mbps);
                        d.save();
                    }
                }
                tray::Command::SetSharp(sharp) => {
                    info!("clearer text: {sharp}");
                    self.sharp = sharp;
                    if let Some(d) = displays.as_deref_mut() {
                        d.set_sharp(sharp);
                        d.save();
                    }
                }
                tray::Command::SetCounting(on) => usage::set_enabled(on),
                tray::Command::ForgetHeadsets => {
                    info!(
                        "forgetting {} paired headset(s)",
                        self.security.trusted.len()
                    );
                    if let Err(e) = self.security.trusted.clear() {
                        warn!("cannot forget headsets: {e}");
                    }
                }
                tray::Command::Quit => self.quit = true,
            }
        }
        self.quit
    }

    /// Shows the current state in the tray (only when it changed).
    fn show(&mut self, via: Option<&'static str>, displays: Option<&Displays>) {
        let status = tray::Status {
            connected: via,
            monitors: displays.map_or(0, Displays::monitor_count),
            mirror: displays.is_some_and(Displays::mirror_active),
            paused: displays.is_some_and(|d| !d.streaming()),
        };
        if self.shown.as_ref() != Some(&status) {
            tray::set_status(self.tray.as_ref(), status.clone());
            self.shown = Some(status);
        }
    }
}

/// Taken off for this long, the headset counts as gone.
const TAKEN_OFF_GRACE: Duration = Duration::from_secs(10);

/// Notices a headset that vanished without closing the connection (switched off,
/// out of Wi-Fi) within about 10 s, instead of waiting forever.
fn set_keepalive(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let set = |level, name, value: libc::c_int| {
        // SAFETY: a valid socket and a c_int option value.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&value as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 4);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 2);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    // Unacknowledged data (frames) also gives up after 10 s.
    set(libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 10_000);
}

fn serve_client(
    stream: TcpStream,
    mut reader: BufReader<SecureReader>,
    writer: SecureWriter,
    displays: &mut Displays,
    control: &mut Control,
    via: &'static str,
) -> Result<()> {
    let mut writer = BufWriter::with_capacity(1 << 20, writer);

    // Room for ~0.3 s of 10 ms sound slices next to video frames.
    let (tx, rx) = mpsc::sync_channel::<HostMsg>(64);
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
    displays.router.send(HostMsg::Order {
        ids: displays.order().to_vec(),
    });
    displays.router.send(HostMsg::MirrorWanted {
        enabled: displays.mirror_wanted(),
    });
    displays.router.send(HostMsg::AudioWanted {
        enabled: displays.audio_wanted(),
    });
    displays.router.send(HostMsg::MicWanted {
        enabled: displays.mic_wanted(),
    });
    displays.set_streaming(true);
    displays.force_keyframes();
    usage::headset_connected();

    // Housekeeping runs on a clock: the headset acknowledges every frame, so the
    // channel is rarely quiet long enough for a receive timeout.
    let mut next_tick = Instant::now();
    // The display under the mouse, for dimming the others in the headset.
    let active_output = focus::ActiveOutput::new();
    let mut focus = None;
    let mut next_focus = Instant::now();
    let result = loop {
        if Instant::now() >= next_focus {
            next_focus = Instant::now() + Duration::from_millis(150);
            let id = active_output
                .get()
                .and_then(|name| displays.id_of_output(&name))
                .unwrap_or(NO_FOCUS);
            if focus != Some(id) {
                focus = Some(id);
                displays.router.send(HostMsg::Focus { id });
            }
        }
        if Instant::now() >= next_tick {
            next_tick = Instant::now() + Duration::from_millis(500);
            if control.poll(Some(displays)) {
                break Ok(());
            }
            control.show(Some(via), Some(displays));
            displays.tick();
            if displays.taken_off_for() >= Some(TAKEN_OFF_GRACE) {
                info!("headset taken off");
                break Ok(());
            }
            if !displays.healthy() {
                break Err(anyhow::anyhow!("virtual monitor or encoder stopped"));
            }
        }
        let wait = next_tick
            .min(next_focus)
            .saturating_duration_since(Instant::now());
        match msg_rx.recv_timeout(wait) {
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
                        info!("headset added Prostir-{}", id + 1);
                        displays.force_keyframe(id);
                        displays.save();
                        displays::sync_wallpapers();
                    }
                    Err(e) => report(displays, format!("{e:#}")),
                }
            }
            Ok(Ok(ClientMsg::RemoveMonitor { id })) => match displays.remove(id) {
                Ok(()) => {
                    info!("headset removed Prostir-{}", id + 1);
                    displays.save();
                }
                Err(e) => report(displays, format!("{e:#}")),
            },
            Ok(Ok(ClientMsg::SetOrder { ids })) => match displays.set_order(&ids) {
                Ok(()) => displays.save(),
                Err(e) => report(displays, format!("{e:#}")),
            },
            Ok(Ok(ClientMsg::SetLayout { rows })) => {
                displays.set_layout(rows);
                displays.save();
            }
            Ok(Ok(ClientMsg::SetMonitors { sizes, primary })) => {
                match displays.set_monitors(&sizes, primary as usize) {
                    Ok(()) => info!("headset switched to {} monitor(s)", sizes.len()),
                    Err(e) => report(displays, format!("{e:#}")),
                }
                for id in displays.order().to_vec() {
                    displays.force_keyframe(id);
                }
                displays.save();
                displays::sync_wallpapers();
            }
            Ok(Ok(ClientMsg::Mic { pcm })) => displays.push_mic(pcm),
            Ok(Ok(ClientMsg::Presence { worn })) => displays.set_worn(worn),
            Ok(Ok(ClientMsg::SetMic { enabled })) => {
                displays.set_mic_wanted(enabled);
                displays.save();
            }
            Ok(Ok(ClientMsg::SetAudio { enabled })) => {
                displays.set_audio_wanted(enabled);
                displays.save();
            }
            Ok(Ok(ClientMsg::SetMirror { enabled })) => {
                displays.set_mirror(enabled);
                displays.save();
            }
            Ok(Ok(ClientMsg::SetPrimary { id })) => {
                info!("headset made Prostir-{} the primary monitor", id + 1);
                displays.set_primary(id);
                displays.save();
                displays::sync_wallpapers();
            }
            Ok(Ok(other)) => warn!("ignoring {other:?}"),
            Ok(Err(e)) if e.kind() == ErrorKind::UnexpectedEof => break Ok(()),
            Ok(Err(e)) => break Err(e.into()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    displays.set_streaming(false);
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
    let beacon = prostir_proto::encode_beacon(port, &hostname());
    std::thread::Builder::new()
        .name("discovery".into())
        .spawn(move || {
            loop {
                // 255.255.255.255 only leaves through the default route, so also send to the
                // broadcast address of every interface (Wi-Fi, Ethernet, hotspot).
                let mut targets = broadcast_addresses();
                targets.push(Ipv4Addr::BROADCAST);
                for ip in targets {
                    let _ = socket.send_to(&beacon, (ip, prostir_proto::DISCOVERY_PORT));
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

fn create_displays(args: &Args, bitrate: u32, sharp: bool) -> Result<Displays> {
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
        bitrate_kbps: bitrate * 1000,
        software: args.software,
        sharp,
    };
    let stored = displays::load_saved(parse_monitor);
    let mirror = stored.as_ref().is_none_or(|s| s.mirror);
    let saved = match stored {
        Some(saved) if args.monitors.is_empty() => saved,
        _ if !args.monitors.is_empty() => SavedMonitors {
            specs: args.monitors.clone(),
            primary: None,
            mirror,
            audio: true,
            mic: true,
            bitrate: None,
            sharp,
            rows: Vec::new(),
        },
        _ => SavedMonitors {
            specs: vec![parse_monitor("2560x1440").expect("valid default")],
            primary: None,
            mirror,
            audio: true,
            mic: true,
            bitrate: None,
            sharp,
            rows: Vec::new(),
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
    info!(
        "Prostir {} waiting for the headset on port {}",
        env!("CARGO_PKG_VERSION"),
        args.port
    );
    if !args.no_discovery {
        spawn_discovery(args.port)?;
    }
    if !args.no_adb {
        spawn_adb_reverse(args.port);
    }

    let bitrate = args
        .bitrate
        .or_else(|| displays::load_saved(parse_monitor).and_then(|s| s.bitrate))
        .unwrap_or(30);
    let sharp = displays::load_saved(parse_monitor).is_some_and(|s| s.sharp);
    let (command_tx, commands) = mpsc::channel();
    let mut control = Control {
        security: pairing::Security::load()?,
        tray: tray::spawn(command_tx, bitrate, sharp),
        commands,
        bitrate,
        sharp,
        quit: false,
        shown: None,
    };
    control.show(None, None);

    let mut displays: Option<Displays> = None;
    let mut idle_since = Instant::now();
    loop {
        if control.quit {
            info!("quitting");
            drop(displays);
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, addr)) => {
                stream.set_nonblocking(false)?;
                let (reader, writer) =
                    match pairing::accept(&stream, &mut control.security, control.tray.as_ref()) {
                        Ok(halves) => halves,
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
                    match create_displays(&args, control.bitrate, control.sharp) {
                        Ok(d) => displays = Some(d),
                        Err(e) => {
                            error!("{e:#}");
                            continue;
                        }
                    }
                }
                set_keepalive(&stream);
                if let Some(d) = displays.as_mut() {
                    d.set_worn(true);
                }
                control.show(Some(via), displays.as_ref());
                if let Err(e) = serve_client(
                    stream,
                    reader,
                    writer,
                    displays.as_mut().unwrap(),
                    &mut control,
                    via,
                ) {
                    warn!("session ended: {e:#}");
                } else {
                    info!("headset disconnected");
                }
                idle_since = Instant::now();
                // Taken off: give the desktop back to the laptop right away (the
                // headset reconnects by itself once it is worn again).
                if displays
                    .as_ref()
                    .is_some_and(|d| d.taken_off_for().is_some())
                {
                    info!("removing virtual monitors until the headset is worn again");
                    displays = None;
                }
                control.show(None, displays.as_ref());
                if args.linger > 0 && displays.as_ref().is_some_and(|d| d.healthy()) {
                    info!("keeping virtual monitors for {} s", args.linger);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                control.poll(displays.as_mut());
                if displays.is_some() && idle_since.elapsed() >= Duration::from_secs(args.linger) {
                    displays = None;
                    control.show(None, None);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.into()),
        }
    }
}
