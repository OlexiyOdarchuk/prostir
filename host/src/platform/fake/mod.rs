//! A desktop that exists only inside this process, for the end-to-end tests
//! (`host/tests/e2e`): built instead of the real platform with the `fake-desktop`
//! feature, never in a release. The core runs as it is (monitors, router, encoders,
//! the session loop, pairing) without KDE, Hyprland, PipeWire or a tray.
//!
//! What it does differently from a real PC:
//! - Monitors are pictures made here and streamed to the encoder like KWin streams
//!   its outputs (see `screen.rs`): the current picture when the encoder starts, then
//!   only when the picture changes (`PROSTIR_FAKE_FPS` times a second; 0, the
//!   default, is a still screen). `PROSTIR_FAKE_PICTURE=noise` makes the picture
//!   random pixels instead of grey bands, so frames are as big as on a busy screen.
//!   `PROSTIR_FAKE_SLOW_MONITORS_MS` makes each monitor take that long to make, and
//!   `PROSTIR_FAKE_BROKEN_MONITORS=2,3` makes those monitors (by number, from 1)
//!   impossible to make.
//! - Sound is silence, 10 ms at a time, each slice starting with the time it was
//!   made (see [`audio`]), so a fake headset can tell how late it arrives; the
//!   microphone goes nowhere.
//! - No windows, no mouse, no screenshots, no notifications, and no outside programs
//!   are run ([`run`] says none is installed). The tray is a file a test writes menu
//!   items to (see [`tray`]).
//! - Another program can resize a monitor, as sharing it in Discord does on KDE: a
//!   test writes `resize Fake-2 320x240` to `$XDG_RUNTIME_DIR/fake-desktop` (see
//!   `desktop.rs`).
//! - The network is the real one: the connection to the headset is set up as on any
//!   Linux PC (`platform/net.rs`), so the tests measure what a user gets.
//! - It runs only when asked to with `PROSTIR_FAKE_DESKTOP=1`, and its settings live
//!   in `$XDG_CONFIG_HOME` only, which must be set: a fake build started by mistake
//!   never falls back to the user's own `~/.config`.
//! - The person at the PC types the pairing code in `PROSTIR_FAKE_TYPED_CODE`, if
//!   set; otherwise there is no dialog to type it in.
//!
//! It provides what the core uses of the platform contract (see `platform/mod.rs`);
//! helpers only the Linux platform calls itself (`windows::all_windows`,
//! `audio::restore_after_crash`) are left out.

mod desktop;
mod screen;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

pub use desktop::{FakeDesktop as Desktop, FakeOutput as Output};

/// The fake desktop was asked for: `PROSTIR_FAKE_DESKTOP=1`, as the tests set it.
fn asked_for() -> bool {
    std::env::var_os("PROSTIR_FAKE_DESKTOP").is_some_and(|v| v == "1")
}

/// Where per-user settings go: `$XDG_CONFIG_HOME` and nowhere else, and only when the
/// fake desktop was asked for.
pub fn config_home() -> Option<PathBuf> {
    if !asked_for() {
        return None;
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// One fake PC per settings directory; refuses to start unless asked for and given
/// one, so neither a test nor a fake build started by mistake touches the real settings.
pub fn single_instance() -> Result<std::fs::File> {
    anyhow::ensure!(
        asked_for(),
        "this build has a fake desktop for tests; it runs only with PROSTIR_FAKE_DESKTOP=1"
    );
    let dir = config_home().context(
        "the fake desktop needs XDG_CONFIG_HOME set to a directory of its own (a test's)",
    )?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("prostir-fake.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a file descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!("a fake Prostir already runs with {}", dir.display());
    }
    log::warn!("fake desktop (a test build): settings in {}", dir.display());
    Ok(file)
}

/// Set by SIGTERM and SIGINT: a test stops the PC app the way a user would.
static QUIT: AtomicBool = AtomicBool::new(false);

pub fn quit_requested() -> bool {
    QUIT.load(Ordering::Relaxed)
}

pub fn quit_on_signals() {
    extern "C" fn on_signal(_: libc::c_int) {
        // Only an atomic store: safe inside a signal handler.
        QUIT.store(true, Ordering::Relaxed);
    }
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: installs a handler that only stores an atomic.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
}

/// Nothing outlives a fake run.
pub fn clean_up_after_crash() {}

pub fn give_memory_back() {}

pub fn hostname() -> String {
    "fake-pc".into()
}

pub fn broadcast_addresses() -> Vec<Ipv4Addr> {
    Vec::new()
}

/// The fake PC has no programs installed.
pub fn run(cmd: &mut Command, _timeout: Duration) -> std::io::Result<std::process::Output> {
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "the fake desktop runs no programs ({})",
            cmd.get_program().to_string_lossy()
        ),
    ))
}

/// No windows and no mouse.
pub mod windows {
    use std::path::PathBuf;

    use anyhow::{Result, bail};

    use crate::platform::ActiveWindow;

    pub fn active() -> Option<ActiveWindow> {
        None
    }

    pub fn cursor() -> Option<(i32, i32)> {
        None
    }

    pub fn can_move_cursor() -> bool {
        false
    }

    pub fn move_cursor(_x: i32, _y: i32) -> bool {
        false
    }

    pub fn move_to(_w: &ActiveWindow, _output: &str, _fill: bool) -> Result<()> {
        bail!("the fake desktop has no windows")
    }

    pub fn put_back(_w: &ActiveWindow, _output: &str) -> Result<()> {
        bail!("the fake desktop has no windows")
    }

    pub fn forget_panel(_output: &str) {}

    pub fn vacated(_panels: &[String]) -> Vec<String> {
        Vec::new()
    }

    pub fn screenshot(_output: Option<&str>) -> Result<PathBuf> {
        bail!("the fake desktop takes no screenshots")
    }

    /// In the settings directory (the fake PC has no Pictures folder).
    pub fn screenshot_path() -> Result<PathBuf> {
        let dir = super::config_home()
            .ok_or_else(|| anyhow::anyhow!("no settings directory"))?
            .join("screenshots");
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join("screenshot.png"))
    }
}

pub mod focus {
    /// The mouse is on no output.
    pub struct ActiveOutput;

    impl ActiveOutput {
        pub fn new() -> Self {
            Self
        }

        pub fn get(&self) -> Option<String> {
            None
        }
    }
}

/// Silence to the headset; the headset microphone goes nowhere.
///
/// Every slice of sound starts with the `CLOCK_MONOTONIC` time it was made, in
/// nanoseconds (`u64`, little-endian): a fake headset on the same machine reads how
/// long each slice took to arrive (see `prostir-testkit`).
pub mod audio {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use prostir_proto::{AUDIO_CHANNELS, AUDIO_RATE};

    /// 10 ms of 16-bit sound, as the real capture sends it.
    const SLICE_BYTES: usize = (AUDIO_RATE / 100 * AUDIO_CHANNELS * 2) as usize;

    pub struct AudioOut {
        stop: Arc<AtomicBool>,
    }

    impl AudioOut {
        pub fn start(on_sound: impl Fn(Vec<u8>) + Send + Sync + 'static) -> Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            std::thread::Builder::new()
                .name("fake-sound".into())
                .spawn(move || {
                    let mut next = Instant::now();
                    while !stopped.load(Ordering::Relaxed) {
                        let mut pcm = vec![0; SLICE_BYTES];
                        pcm[..8].copy_from_slice(&monotonic_ns().to_le_bytes());
                        on_sound(pcm);
                        next += Duration::from_millis(10);
                        std::thread::sleep(next.saturating_duration_since(Instant::now()));
                    }
                })?;
            Ok(Self { stop })
        }
    }

    /// `CLOCK_MONOTONIC` now, in nanoseconds.
    fn monotonic_ns() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime writes into the timespec it is given.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    impl Drop for AudioOut {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// The headset's microphone as the fake PC's: what comes is counted and said in
    /// the log now and then; a program records while a test says so (`record on`).
    pub struct MicIn {
        got: std::sync::atomic::AtomicUsize,
    }

    /// A program of the fake PC records from the headset's microphone.
    pub static RECORDING: AtomicBool = AtomicBool::new(false);

    impl MicIn {
        pub fn start() -> Result<Self> {
            Ok(Self {
                got: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        pub fn push(&self, pcm: Vec<u8>) {
            let before = self.got.fetch_add(pcm.len(), Ordering::Relaxed);
            // About once a second of mono sound.
            if before / 96_000 != (before + pcm.len()) / 96_000 {
                log::info!(
                    "fake desktop: the microphone got {} bytes",
                    before + pcm.len()
                );
            }
        }

        pub fn recording(&self) -> bool {
            RECORDING.load(Ordering::Relaxed)
        }
    }

    /// The fake sound device for the headset: its level is what a test sets (`volume
    /// 40`, `volume 40 muted`, `drag 0 100 1000`: the slider from 0 to 100 in a
    /// second), and what the PC sets goes into the log.
    pub struct SinkVolume {
        changes: std::sync::mpsc::Receiver<(u8, bool, Instant)>,
    }

    /// Where the fake desktop's commands send the device's changes.
    #[allow(clippy::type_complexity)]
    pub static VOLUME: std::sync::Mutex<Option<std::sync::mpsc::Sender<(u8, bool, Instant)>>> =
        std::sync::Mutex::new(None);

    impl SinkVolume {
        pub fn open() -> Option<Self> {
            let (tx, changes) = std::sync::mpsc::channel();
            *VOLUME.lock().unwrap() = Some(tx);
            Some(Self { changes })
        }

        pub fn set(&self, level: u8, muted: bool) {
            log::info!(
                "fake desktop: volume set to {level}{}",
                if muted { " muted" } else { "" }
            );
            // The device says it changed, as a real one does.
            if let Some(tx) = VOLUME.lock().unwrap().as_ref() {
                let _ = tx.send((level, muted, Instant::now()));
            }
        }

        pub fn changed(&self) -> Option<(u8, bool, Instant)> {
            self.changes.try_recv().ok()
        }
    }
}

pub mod sound_source {
    /// No app plays sound on the fake PC.
    pub struct Watcher;

    impl Watcher {
        pub fn start() -> Self {
            Self
        }

        pub fn output(&self) -> Option<String> {
            None
        }
    }
}

pub mod notifications {
    pub struct Watcher;

    /// The fake PC shows no notifications.
    pub fn watch(_show: impl Fn(String) + Send + 'static) -> Option<Watcher> {
        None
    }
}

/// A tray without an icon: a test "clicks" its items by writing their names, one a
/// line, to `$XDG_RUNTIME_DIR/fake-tray` (`forget`, `recenter`, `quit`, `sharp on`,
/// `sharp off`, `quality 15`, `quality 50`), which is read and removed a few times a
/// second.
pub mod tray {
    use std::path::PathBuf;
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    use crate::platform::{TrayCommand, TrayStatus};

    pub struct Handle;

    pub fn spawn(commands: Sender<TrayCommand>) -> Option<Handle> {
        let path = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("fake-tray");
        let watched = std::thread::Builder::new()
            .name("fake-tray".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    let _ = std::fs::remove_file(&path);
                    for item in text.lines() {
                        let command = match item.trim() {
                            "forget" => TrayCommand::ForgetHeadsets,
                            "recenter" => TrayCommand::Headset {
                                asked: prostir_proto::request::Asked::Known(
                                    prostir_proto::request::Command::Recenter,
                                ),
                                reply: None,
                            },
                            "quit" => TrayCommand::Quit,
                            "sharp on" => TrayCommand::SetSharp(true),
                            "sharp off" => TrayCommand::SetSharp(false),
                            "quality 15" => TrayCommand::SetBitrate(15),
                            "quality 50" => TrayCommand::SetBitrate(50),
                            other => {
                                log::warn!("fake tray: no item {other:?}");
                                continue;
                            }
                        };
                        log::info!("fake tray: {item}");
                        if commands.send(command).is_err() {
                            return;
                        }
                    }
                }
            });
        watched.ok().map(|_| Handle)
    }

    pub fn set_status(_handle: Option<&Handle>, _status: TrayStatus) {}

    pub fn set_battery(_handle: Option<&Handle>, _battery: Option<(u8, bool)>) {}

    pub fn set_settings(_handle: Option<&Handle>, _bitrate: u32, _sharp: bool, _counting: bool) {}

    pub fn set_pairing(_handle: Option<&Handle>, _code: Option<String>) {}
}

pub mod dialogs {
    use crate::platform::NoCode;

    /// The code the test's person at the PC types (`PROSTIR_FAKE_TYPED_CODE`).
    pub fn ask_code() -> Result<String, NoCode> {
        std::env::var("PROSTIR_FAKE_TYPED_CODE").map_err(|_| NoCode::NoDialog)
    }

    /// Into the log instead of onto a screen.
    pub fn notify(title: &str, body: &str) {
        log::info!("notification: {title}: {body}");
    }

    pub fn notify_and_wait(title: &str, body: &str) -> bool {
        notify(title, body);
        false
    }
}
