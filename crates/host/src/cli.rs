//! The command line: `prostir`, `prostir serve`, `prostir open` and `prostir ctl`.

use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Virtual monitors for KDE Plasma and Hyprland, streamed to a Meta Quest 3",
    args_conflicts_with_subcommands = true
)]
pub(super) struct Cli {
    /// Print the version (as the log says it: `Prostir 0.2.0`) and exit.
    #[arg(short = 'v', long = "version", global = true)]
    pub(crate) version: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Sub>,
    /// Without a command: as `prostir serve`.
    #[command(flatten)]
    pub(crate) serve: Args,
}

#[derive(Subcommand, Debug)]
pub(super) enum Sub {
    /// Run: wait for the headset and serve it (the same as no command).
    Serve(Args),
    /// Open the settings window, starting Prostir first if it is not running.
    Open,
    /// Ask the running Prostir something (`prostir ctl help` lists the commands).
    #[command(disable_help_flag = true)]
    Ctl {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
}

#[derive(clap::Args, Debug)]
pub(super) struct Args {
    /// Monitor in pixels, optionally with KWin scale: 2560x1440 or 3840x2160@1.5. Repeat for
    /// several monitors. Without it, the set from the last session (changed from the headset
    /// menu) is used, or a single 2560x1440.
    #[arg(short, long = "monitor", value_parser = parse_monitor)]
    pub(crate) monitors: Vec<MonitorSpec>,
    /// Leave KDE's primary monitor alone (by default the middle Quest monitor becomes primary).
    #[arg(long)]
    pub(crate) keep_primary: bool,
    /// Video codec. Without it: the one chosen in the headset menu, else HEVC (sharpest
    /// small text). H.264 needs far less memory on Intel (~9 MB against ~70 MB of driver
    /// memory per monitor) at a slightly softer picture.
    #[arg(long, value_enum)]
    pub(crate) codec: Option<CodecArg>,
    /// Bitrate per monitor, Mbit/s (default: the quality chosen in the tray, or 30).
    #[arg(long)]
    pub(crate) bitrate: Option<u32>,
    /// Encode on the CPU with x264 even when the graphics card can (implies H.264).
    /// Without it Prostir uses VA-API (Intel, AMD), then NVENC (NVIDIA), then x264.
    #[arg(long)]
    pub(crate) software: bool,
    #[arg(long, default_value_t = prostir_proto::TCP_PORT)]
    pub(crate) port: u16,
    /// The UDP port headsets ask for this PC on (0: the system picks one, for tests).
    #[arg(long, default_value_t = prostir_proto::DISCOVERY_PORT, hide = true)]
    pub(crate) discovery_port: u16,
    /// Keep the virtual monitors this many seconds after the headset disconnects.
    #[arg(long, default_value_t = 30)]
    pub(crate) linger: u64,
    /// Do not answer headsets that ask for this PC on the network, and send no beacon.
    #[arg(long)]
    pub(crate) no_discovery: bool,
    /// Send no beacon (headsets that do not ask yet listen for it), but answer.
    #[arg(long, hide = true)]
    pub(crate) no_beacon: bool,
    /// Stay running while no headset needs Prostir, with its sockets opened here,
    /// instead of sleeping in a small watcher until a headset comes.
    #[arg(long)]
    pub(crate) stay: bool,
    /// Started by the sockets' watcher, with its sockets (it decided to start).
    #[arg(long, hide = true)]
    pub(crate) woken: bool,
    /// Open the pairing window once ready (the button of the notice of a new headset).
    #[arg(long, hide = true)]
    pub(crate) pair: bool,
    /// Do not run `adb reverse` for USB connections.
    #[arg(long)]
    pub(crate) no_adb: bool,
    /// Open the settings window once Prostir is ready (`prostir open` starts it so).
    #[arg(long)]
    pub(crate) open_settings: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(super) enum CodecArg {
    H264,
    H265,
}

pub(super) fn parse_monitor(s: &str) -> Result<MonitorSpec, String> {
    s.parse()
}
