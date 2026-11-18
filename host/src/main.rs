// Every unsafe block says why it is sound.
#![deny(clippy::undocumented_unsafe_blocks)]
mod audio;
mod desktop;
mod displays;
mod encoder;
mod focus;
mod hyprland;
mod i18n;
mod notifications;
mod outbox;
mod pairing;
mod screencopy;
mod sound_source;
mod tray;
mod usage;
mod virtual_output;
mod windows;

use std::io::{BufReader, BufWriter, ErrorKind, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use log::{debug, error, info, warn};
use prostir_proto::secure::{SecureReader, SecureWriter};
use prostir_proto::{ClientMsg, Codec, HostMsg, NO_FOCUS};

use displays::{Displays, SavedMonitors};
use encoder::EncoderConfig;
use virtual_output::MonitorSpec;

#[derive(Parser, Debug)]
#[command(about = "Virtual monitors for KDE Plasma and Hyprland, streamed to a Meta Quest 3")]
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
    /// Video codec. Without it: the one chosen in the headset menu, else HEVC (sharpest
    /// small text). H.264 needs far less memory on Intel (~9 MB against ~70 MB of driver
    /// memory per monitor) at a slightly softer picture.
    #[arg(long, value_enum)]
    codec: Option<CodecArg>,
    /// Bitrate per monitor, Mbit/s (default: the quality chosen in the tray, or 30).
    #[arg(long)]
    bitrate: Option<u32>,
    /// Encode on the CPU with x264 even when the graphics card can (implies H.264).
    /// Without it Prostir uses VA-API (Intel, AMD), then NVENC (NVIDIA), then x264.
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
        if QUIT.load(Ordering::Relaxed) {
            self.quit = true;
        }
        let mut displays = displays;
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                tray::Command::SetBitrate(mbps) => {
                    info!("video quality: {mbps} Mbit/s per monitor");
                    self.bitrate = mbps;
                    if let Some(d) = displays.as_deref_mut() {
                        d.set_bitrate(mbps);
                        d.save();
                        d.announce_pc_settings();
                    }
                }
                tray::Command::SetSharp(sharp) => {
                    info!("clearer text: {sharp}");
                    self.sharp = sharp;
                    if let Some(d) = displays.as_deref_mut() {
                        d.set_sharp(sharp);
                        d.save();
                        d.announce_pc_settings();
                    }
                }
                tray::Command::SetCounting(on) => usage::set_enabled(on),
                tray::Command::Recenter => {
                    if let Some(d) = displays.as_deref_mut() {
                        d.router.send(HostMsg::Recenter);
                    }
                }
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
    // Keep little unsent data in the kernel (Linux would let it grow to megabytes):
    // frames wait in the outbox instead, where late ones can still be dropped.
    set(libc::IPPROTO_TCP, libc::TCP_NOTSENT_LOWAT, 256 * 1024);
}

fn serve_client(
    stream: TcpStream,
    mut reader: BufReader<SecureReader>,
    writer: SecureWriter,
    displays: &mut Displays,
    control: &mut Control,
    via: &'static str,
) -> Result<()> {
    // A small buffer: anything waiting belongs in the outbox, where old video frames
    // can still be dropped, not in a buffer where they can only be sent late.
    let mut writer = BufWriter::with_capacity(64 * 1024, writer);
    // A short Wi-Fi stall drops frames (see the outbox) instead of ending the session;
    // TCP_USER_TIMEOUT notices a headset that is really gone.
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let outbox = Arc::new(outbox::Outbox::default());
    let writer_thread = {
        let stream = stream.try_clone()?;
        let outbox = outbox.clone();
        std::thread::Builder::new()
            .name("net-writer".into())
            .spawn(move || {
                let result = (|| -> std::io::Result<()> {
                    while let Some(msg) = outbox.next() {
                        msg.write(&mut writer)?;
                        if outbox.is_idle() {
                            writer.flush()?;
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    info!("connection closed while sending: {e}");
                }
                outbox.close();
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

    displays.router.attach(outbox.clone());
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
    displays.announce_pc_settings();
    displays.set_streaming(true);
    displays.force_keyframes();
    usage::headset_connected();

    // Housekeeping runs on a clock: the headset acknowledges every frame, so the
    // channel is rarely quiet long enough for a receive timeout.
    let mut next_tick = Instant::now();
    // The display under the mouse, for dimming the others in the headset.
    let active_output = focus::ActiveOutput::new();
    let mut battery_warned = false;
    // PC notifications passed to the headset, while it asks for them.
    let mut notifications: Option<notifications::Watcher> = None;
    // Which monitor the sound comes from, while the headset wants to know.
    let mut sound_source: Option<sound_source::Watcher> = None;
    let mut sound_from = None;
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
            if let Some(w) = &sound_source {
                let id = w
                    .output()
                    .and_then(|name| displays.id_of_output(&name))
                    .unwrap_or(NO_FOCUS);
                if sound_from != Some(id) {
                    sound_from = Some(id);
                    info!("sound comes from monitor {id}");
                    displays.router.send(HostMsg::SoundFrom { id });
                }
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
            Ok(Ok(ClientMsg::RemoveMonitor { id })) if prostir_proto::is_window_panel(id) => {
                displays.close_window_panel(id);
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
            Ok(Ok(ClientMsg::DetachWindow)) => {
                if let Err(e) = displays.detach_window() {
                    report(displays, format!("{e:#}"));
                }
            }
            Ok(Ok(ClientMsg::WindowHere { id })) => {
                if let Err(e) = displays.window_here(id) {
                    report(displays, format!("{e:#}"));
                }
            }
            Ok(Ok(ClientMsg::Screenshot { id })) => match displays.screenshot(id) {
                Ok(path) => displays.router.send(HostMsg::Notice {
                    text: format!(
                        "{} {}",
                        i18n::t("Знімок збережено:", "Screenshot saved:"),
                        home_relative(&path)
                    ),
                }),
                Err(e) => report(displays, format!("{e:#}")),
            },
            Ok(Ok(ClientMsg::SetNotifications { enabled })) => {
                notifications = if enabled {
                    notifications.or_else(|| notifications::watch(displays.router.clone()))
                } else {
                    None
                };
            }
            Ok(Ok(ClientMsg::SetSoundFromScreen { enabled })) => {
                sound_source = enabled.then(|| {
                    sound_source
                        .take()
                        .unwrap_or_else(sound_source::Watcher::start)
                });
                sound_from = None;
            }
            Ok(Ok(ClientMsg::FindMouse)) => {
                if let Some((id, x, y)) = displays.find_mouse() {
                    displays.router.send(HostMsg::MouseAt { id, x, y });
                }
            }
            Ok(Ok(ClientMsg::MouseTo { id, x, y })) => displays.mouse_to(id, x, y),
            Ok(Ok(ClientMsg::Battery { percent, charging })) => {
                tray::set_battery(control.tray.as_ref(), Some((percent, charging)));
                // Once per session, when it gets low.
                if percent <= 15 && !charging && !battery_warned {
                    battery_warned = true;
                    notify_battery(percent);
                }
            }
            Ok(Ok(ClientMsg::SetPcSettings { settings })) => {
                match displays.set_pc_settings(settings) {
                    Ok(()) => {
                        control.bitrate = settings.quality.clamp(5, 100);
                        control.sharp = settings.sharp;
                        tray::set_encoding(control.tray.as_ref(), control.bitrate, control.sharp);
                    }
                    Err(e) => report(displays, format!("{e:#}")),
                }
            }
            Ok(Ok(ClientMsg::Unknown { tag })) => {
                debug!("skipping message {tag} from a newer headset")
            }
            Ok(Ok(other)) => warn!("ignoring {other:?}"),
            Ok(Err(e)) if e.kind() == ErrorKind::UnexpectedEof => break Ok(()),
            Ok(Err(e)) => break Err(e.into()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    displays.set_streaming(false);
    tray::set_battery(control.tray.as_ref(), None);
    displays.router.detach();
    outbox.close();
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
    let stored = displays::load_saved(parse_monitor);
    // The command line wins, then the headset menu's choice, then HEVC.
    let prefer = match args.codec {
        Some(CodecArg::H264) => Codec::H264,
        Some(CodecArg::H265) => Codec::H265,
        None => stored.as_ref().and_then(|s| s.codec).unwrap_or(Codec::H265),
    };
    let software = args.software || stored.as_ref().is_some_and(|s| s.software);
    let (backend, codec) = encoder::pick(prefer, software)?;
    info!("encoding with {backend:?} {codec:?}");
    let cfg = EncoderConfig {
        codec,
        bitrate_kbps: bitrate * 1000,
        backend,
        sharp,
    };
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
            codec: None,
            software,
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
            codec: None,
            software,
            rows: Vec::new(),
        },
    };
    let displays = Displays::create(&saved, cfg, !args.keep_primary)?;
    displays.save();
    Ok(displays)
}

/// `~/Pictures/…` rather than the full path, for showing.
fn home_relative(path: &std::path::Path) -> String {
    let shown = path.to_string_lossy().into_owned();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
    }
}

/// The headset's battery is low: say so on the PC, where the user is looking.
fn notify_battery(percent: u8) {
    let body = format!(
        "{} {percent}%.",
        i18n::t("Заряд шолома:", "Headset battery:")
    );
    if let Ok(mut child) = std::process::Command::new("notify-send")
        .args([
            "--app-name=Prostir",
            "--icon=prostir",
            i18n::t("Шолом скоро розрядиться", "The headset battery is low"),
            &body,
        ])
        .spawn()
    {
        let _ = std::thread::Builder::new()
            .name("notify".into())
            .spawn(move || child.wait());
    }
}

/// After a session the freed buffers would stay with the process: hand them back to
/// the system, so Prostir waiting in the background stays small.
fn give_memory_back() {
    // SAFETY: plain glibc call; only releases memory that is already free.
    unsafe { libc::malloc_trim(0) };
}

/// Set by SIGTERM, SIGINT and SIGHUP; the main loop then quits normally, so monitors,
/// the sound output and the microphone are put back by the usual clean-up.
static QUIT: AtomicBool = AtomicBool::new(false);

/// Lets signals end the program cleanly on every desktop. If the normal path does not
/// finish within a few seconds (stuck in a dialog, say), removes Hyprland's monitors
/// directly: KWin drops its outputs when the program ends, Hyprland keeps them.
fn quit_on_signals() {
    extern "C" fn on_signal(_: libc::c_int) {
        // Only an atomic store: safe inside a signal handler.
        QUIT.store(true, Ordering::Relaxed);
    }
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: installs a handler that only stores an atomic. A handler (unlike a
        // blocked signal mask) is not inherited by child processes such as pw-loopback.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
    let spawned = std::thread::Builder::new()
        .name("signals".into())
        .spawn(|| {
            while !QUIT.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            std::thread::sleep(Duration::from_secs(5));
            warn!("did not quit within 5 s: removing monitors and leaving");
            if hyprland::running() {
                hyprland::remove_leftovers();
            }
            audio::restore_after_crash();
            usage::announce_once();
            std::process::exit(1);
        });
    if let Err(e) = spawned {
        warn!("cannot watch for signals: {e}");
    }
}

/// One Prostir per desktop session: a second one would fight over the monitors (and
/// on Hyprland, its start-up clean-up would remove the first one's). The lock file is
/// held for the life of the process.
fn single_instance() -> Result<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let path = dir.join(format!("prostir-{display}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a file descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!("Prostir is already running on this desktop");
    }
    Ok(file)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let _instance = single_instance()?;
    quit_on_signals();
    // Monitors from a run that crashed: without the headset nobody sees their windows.
    if hyprland::running() {
        hyprland::remove_leftovers();
    }
    audio::restore_after_crash();
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
                if let Err(e) = stream.set_nonblocking(false) {
                    warn!("connection from {addr}: {e}");
                    continue;
                }
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
                            // Tell the headset why, instead of just hanging up.
                            let mut writer = writer;
                            let _ = HostMsg::Error {
                                text: format!("{e:#}"),
                            }
                            .write(&mut writer);
                            let _ = writer.flush();
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
                    give_memory_back();
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
                    give_memory_back();
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            // Pending network errors (ENETDOWN, EHOSTUNREACH…) or running out of file
            // descriptors: not a reason to quit.
            Err(e) => {
                warn!("accepting a connection failed: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}
