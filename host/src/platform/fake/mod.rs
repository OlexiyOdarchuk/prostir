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
//! - No windows, no mouse, no screenshots (its desktop has no [`crate::platform::Windows`]
//!   and no [`crate::platform::Pointer`]), no notifications, no USB, and no outside
//!   programs are run. The tray is a file a test writes menu
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
//!   set, taking `PROSTIR_FAKE_TYPED_CODE_DELAY_MS` to do it; otherwise there is no
//!   dialog to type it in.
//!
//! It provides what the core uses of the platform contract (see `platform/mod.rs`);
//! helpers only the Linux platform calls itself (`audio::restore_after_crash`) are
//! left out.

mod desktop;
mod screen;

use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::{Context, Result};

pub use desktop::connect;

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

/// Nothing outlives a fake run.
pub fn clean_up_after_crash() {}

pub fn give_memory_back() {}

pub fn hostname() -> String {
    "fake-pc".into()
}

pub fn broadcast_addresses() -> Vec<Ipv4Addr> {
    Vec::new()
}

/// Calls `on_text` with what a test writes to `$XDG_RUNTIME_DIR/NAME`, on a thread
/// that sleeps in the kernel until a file there is written (inotify), and removes the
/// file once read. `on_text` returns `false` to stop. Whether the thread started.
fn watch_file(name: &'static str, on_text: impl Fn(String) -> bool + Send + 'static) -> bool {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
        return false;
    };
    let path = dir.join(name);
    let Ok(dir_c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a plain system call; the descriptor it returns is owned right below.
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        log::warn!("fake desktop: no inotify for {name}");
        return false;
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    let inotify = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: a valid descriptor and a NUL-terminated path.
    let watch = unsafe {
        libc::inotify_add_watch(
            inotify.as_raw_fd(),
            dir_c.as_ptr(),
            libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO,
        )
    };
    if watch < 0 {
        log::warn!("fake desktop: cannot watch {}", dir.display());
        return false;
    }
    let take = move |path: &std::path::Path| {
        let text = std::fs::read_to_string(path).ok()?;
        let _ = std::fs::remove_file(path);
        Some(text)
    };
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            // Written before the watch began.
            if let Some(text) = take(&path)
                && !on_text(text)
            {
                return;
            }
            let mut buf = [0u8; 4096];
            loop {
                // SAFETY: reads into a buffer of the length given.
                let n =
                    unsafe { libc::read(inotify.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                }
                // Each event: wd (4 bytes), mask (4), cookie (4), len (4), then a name
                // of `len` bytes padded with NULs.
                let mut named = false;
                let mut at = 0;
                let n = n as usize;
                while at + 16 <= n {
                    let len =
                        u32::from_ne_bytes(buf[at + 12..at + 16].try_into().unwrap()) as usize;
                    let raw = &buf[at + 16..(at + 16 + len).min(n)];
                    let event_name = raw.split(|b| *b == 0).next().unwrap_or_default();
                    named |= event_name == name.as_bytes();
                    at += 16 + len;
                }
                if named
                    && let Some(text) = take(&path)
                    && !on_text(text)
                {
                    return;
                }
            }
        })
        .is_ok()
}

/// No headset is ever plugged in by USB.
pub fn keep_adb_reverse(_port: u16) {}

/// In the settings directory (the fake PC has no Pictures folder).
pub fn screenshot_path() -> Result<PathBuf> {
    let dir = config_home()
        .ok_or_else(|| anyhow::anyhow!("no settings directory"))?
        .join("screenshots");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("screenshot.png"))
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

        /// As slow as asking the real sound system (`pactl` takes a process start),
        /// so a test notices when it is asked too often.
        pub fn recording(&self) -> bool {
            std::thread::sleep(std::time::Duration::from_millis(20));
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

    /// Where the fake device is now, once something set or moved it.
    pub static LEVEL: std::sync::Mutex<Option<(u8, bool)>> = std::sync::Mutex::new(None);

    impl SinkVolume {
        pub fn open() -> Option<Self> {
            let (tx, changes) = std::sync::mpsc::channel();
            *VOLUME.lock().unwrap() = Some(tx);
            Some(Self { changes })
        }

        pub fn set(&self, level: u8, muted: bool) {
            *LEVEL.lock().unwrap() = Some((level, muted));
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

        /// Where the device is now, if known.
        pub fn level(&self) -> Option<(u8, bool)> {
            *LEVEL.lock().unwrap()
        }
    }
}

pub mod sound_source {
    use std::sync::Arc;

    use crate::platform::{OutputKey, WindowList};

    /// No app plays sound on the fake PC.
    pub struct Watcher;

    impl Watcher {
        pub fn start(_windows: Option<Arc<dyn WindowList>>) -> Self {
            Self
        }

        pub fn output(&self) -> Option<OutputKey> {
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
/// `sharp off`, `quality 15`, `quality 50`), which is read and removed once written.
pub mod tray {
    use crate::events::EventSink;
    use crate::platform::{TrayCommand, TrayStatus};

    #[derive(Clone)]
    pub struct Handle;

    pub fn spawn(events: EventSink) -> Option<Handle> {
        super::watch_file("fake-tray", move |text| {
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
                if !events.tray(command) {
                    return false;
                }
            }
            true
        })
        .then_some(Handle)
    }

    /// What the tray would show, in the log for the tests.
    pub fn set_status(_handle: Option<&Handle>, status: TrayStatus) {
        log::info!(
            "fake tray: {} other headset(s), microphone denied: {}",
            status.others,
            status.mic_denied
        );
    }

    pub fn set_battery(_handle: Option<&Handle>, _battery: Option<(u8, bool)>) {}

    pub fn set_settings(_handle: Option<&Handle>, _bitrate: u32, _sharp: bool, _counting: bool) {}

    pub fn set_pairing(_handle: Option<&Handle>, _code: Option<String>) {}
}

pub mod dialogs {
    use crate::platform::NoCode;

    /// The code the test's person at the PC types (`PROSTIR_FAKE_TYPED_CODE`), after
    /// thinking for `PROSTIR_FAKE_TYPED_CODE_DELAY_MS`.
    pub fn ask_code() -> Result<String, NoCode> {
        let code = std::env::var("PROSTIR_FAKE_TYPED_CODE").map_err(|_| NoCode::NoDialog)?;
        let delay: u64 = std::env::var("PROSTIR_FAKE_TYPED_CODE_DELAY_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .unwrap_or(0);
        log::info!("fake desktop: the person at the PC types the code in {delay} ms");
        std::thread::sleep(std::time::Duration::from_millis(delay));
        Ok(code)
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
