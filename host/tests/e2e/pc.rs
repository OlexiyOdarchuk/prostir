//! The PC app under test: the `prostir` binary built with the fake desktop, started in
//! a directory of its own (settings, keys, runtime files), on a port the system picks.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prostir_proto::secure::{Identity, TrustStore};
use prostir_testkit::Keys;

/// A directory for one test under Cargo's temporary directory. Removed when the test
/// passes; kept (with the PC app's log) when it fails, to look at.
pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The PC app's settings directory (its `XDG_CONFIG_HOME`).
    fn config(&self) -> PathBuf {
        self.0.join("config")
    }

    /// The fake headset's keys.
    pub fn headset_keys(&self) -> Keys {
        Keys::load(&self.0.join("headset")).unwrap()
    }

    /// Makes the PC and the headset trust each other, as after pairing.
    pub fn pair(&self) -> Keys {
        let pc = Identity::load_or_create(&self.config().join("prostir/identity")).unwrap();
        let mut keys = self.headset_keys();
        keys.trusted.add(pc.public, "fake PC").unwrap();
        TrustStore::load(&self.config().join("prostir/paired_headsets"))
            .add(keys.identity.public, "fake headset")
            .unwrap();
        keys
    }

    /// Makes the PC forget every headset it paired with (the tray's "Forget paired
    /// headsets").
    pub fn forget_headsets(&self) {
        TrustStore::load(&self.config().join("prostir/paired_headsets"))
            .clear()
            .unwrap();
    }

    /// The monitor set the PC app starts with, as 0.1 saved it (`monitors.txt`, moved
    /// over on the first start).
    pub fn save_monitors(&self, text: &str) {
        let path = self.config().join("prostir/monitors.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A file the PC app keeps in its settings directory (`screens.txt`, say).
    pub fn saved(&self, name: &str) -> String {
        std::fs::read_to_string(self.config().join("prostir").join(name)).unwrap_or_default()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("kept {} for a look", self.0.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// How to start the PC app.
pub struct Options {
    /// Picture changes per second on the fake monitors (0: a still screen).
    pub fps: u32,
    /// Random pixels instead of grey bands: frames as big as the bitrate allows.
    pub noise: bool,
    /// `--bitrate`: Mbit/s per monitor.
    pub bitrate: u32,
    /// How long making each fake monitor takes.
    pub slow_monitors: Duration,
    /// `--linger`: seconds the monitors stay after the headset leaves.
    pub linger: u32,
    /// The pairing code the person at the PC types, if asked.
    pub typed_code: Option<String>,
    /// Monitors (by number, from 1) the fake desktop cannot make.
    pub broken_monitors: String,
    /// The fake PC's own displays (`PROSTIR_FAKE_PHYSICAL`: `eDP-1=EDID@1920x1080;…`).
    pub physical: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            fps: 0,
            noise: false,
            bitrate: 8,
            slow_monitors: Duration::ZERO,
            linger: 30,
            typed_code: None,
            broken_monitors: String::new(),
            physical: String::new(),
        }
    }
}

/// A running PC app. Stopped (killed if it has to be) when dropped.
pub struct Pc {
    child: Child,
    pub addr: SocketAddr,
    log: Arc<Mutex<Vec<String>>>,
    /// Its `XDG_RUNTIME_DIR`.
    run_dir: PathBuf,
}

impl Pc {
    pub fn start(dir: &TestDir, options: &Options) -> Self {
        let root = dir.path();
        for sub in ["config", "run", "home"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_prostir"));
        cmd.args(["--port", "0", "--no-discovery", "--no-adb", "--software"])
            .args(["--bitrate", &options.bitrate.to_string()])
            .args(["--linger", &options.linger.to_string()])
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("HOME", root.join("home"))
            .env("PROSTIR_FAKE_DESKTOP", "1")
            .env("PROSTIR_NO_COUNT", "1")
            .env("PROSTIR_FAKE_FPS", options.fps.to_string())
            .env(
                "PROSTIR_FAKE_SLOW_MONITORS_MS",
                options.slow_monitors.as_millis().to_string(),
            )
            .env("PROSTIR_FAKE_BROKEN_MONITORS", &options.broken_monitors)
            .env("PROSTIR_FAKE_PHYSICAL", &options.physical)
            .env(
                "PROSTIR_FAKE_PICTURE",
                if options.noise { "noise" } else { "bands" },
            )
            .env("LANGUAGE", "en")
            .env("RUST_LOG", "info")
            // One plugin registry for all tests, so GStreamer scans its plugins once.
            .env(
                "GST_REGISTRY",
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("gst-registry.bin"),
            )
            // Nothing of the real desktop session, should anything look.
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match &options.typed_code {
            Some(code) => cmd.env("PROSTIR_FAKE_TYPED_CODE", code),
            None => cmd.env_remove("PROSTIR_FAKE_TYPED_CODE"),
        };
        let mut child = cmd.spawn().expect("start the PC app");
        let log = Arc::new(Mutex::new(Vec::new()));
        let stderr = child.stderr.take().unwrap();
        let lines = log.clone();
        let file = root.join("pc.log");
        std::thread::spawn(move || {
            let mut copy = std::fs::File::create(file).ok();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(f) = &mut copy {
                    use std::io::Write;
                    let _ = writeln!(f, "{line}");
                }
                lines.lock().unwrap().push(line);
            }
        });
        let mut pc = Self {
            child,
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            log,
            run_dir: root.join("run"),
        };
        let line = pc.wait_log(Duration::from_secs(30), "waiting for the headset on port");
        let port = line.rsplit(' ').next().unwrap().parse().unwrap();
        pc.addr.set_port(port);
        pc
    }

    /// The first log line containing `needle`, waiting up to `timeout` for it.
    pub fn wait_log(&mut self, timeout: Duration, needle: &str) -> String {
        self.wait_log_after(timeout, needle, 0)
    }

    /// The log line containing `needle` after the first `skip` such lines (taken with
    /// [`Pc::count_log`] before something happened, to wait for a line logged after
    /// it), waiting up to `timeout` for it.
    pub fn wait_log_after(&mut self, timeout: Duration, needle: &str, skip: usize) -> String {
        let until = Instant::now() + timeout;
        loop {
            let found = self
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.contains(needle))
                .nth(skip)
                .cloned();
            if let Some(line) = found {
                return line;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("the PC app ended ({status}) before logging {needle:?}");
            }
            assert!(
                Instant::now() < until,
                "the PC app did not log {needle:?} within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Every log line containing `needle`, in order.
    pub fn lines(&self, needle: &str) -> Vec<String> {
        let log = self.log.lock().unwrap();
        log.iter().filter(|l| l.contains(needle)).cloned().collect()
    }

    /// How many log lines contain `needle`.
    pub fn count_log(&self, needle: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(needle))
            .count()
    }

    /// How many times the picture source of each fake monitor started so far, by
    /// output name: each start sends the current picture.
    pub fn source_starts(&self) -> BTreeMap<String, usize> {
        let mut starts = BTreeMap::new();
        for line in self.log.lock().unwrap().iter() {
            if let Some(name) = line
                .split_once("fake screen ")
                .and_then(|(_, rest)| rest.strip_suffix(": started"))
            {
                *starts.entry(name.to_owned()).or_default() += 1;
            }
        }
        starts
    }

    /// Clicks `item` in the PC app's tray (see the fake tray for the items).
    pub fn tray(&self, item: &str) {
        std::fs::write(self.run_dir.join("fake-tray"), format!("{item}\n")).unwrap();
    }

    /// Has another program do `command` on the fake desktop (see the fake desktop
    /// for the commands, e.g. `resize Fake-2 320x240`).
    pub fn desktop(&self, command: &str) {
        std::fs::write(self.run_dir.join("fake-desktop"), format!("{command}\n")).unwrap();
    }

    /// Kills the PC app (`SIGKILL`), as a crash or a power cut would.
    pub fn kill(mut self) {
        self.signal(libc::SIGKILL);
        let _ = self.child.wait();
    }

    /// Stops the PC app dead (`SIGSTOP`), as a PC going to sleep does.
    pub fn freeze(&self) {
        self.signal(libc::SIGSTOP);
    }

    /// Lets a frozen PC app run on (`SIGCONT`), as after waking up.
    pub fn thaw(&self) {
        self.signal(libc::SIGCONT);
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: a signal to our own child process.
        unsafe { libc::kill(self.child.id() as libc::pid_t, signal) };
    }

    /// Asks the PC app to quit as a user would (SIGTERM) and checks that it does.
    pub fn quit(mut self) {
        self.signal(libc::SIGTERM);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "the PC app quit with {status}");
                return;
            }
            assert!(
                Instant::now() < until,
                "the PC app did not quit within 10 s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Pc {
    fn drop(&mut self) {
        // A frozen one does not die of SIGKILL before it runs again.
        self.thaw();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
