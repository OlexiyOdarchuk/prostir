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

    /// The monitor set the PC app starts with (its saved `monitors.txt`).
    pub fn save_monitors(&self, text: &str) {
        let path = self.config().join("prostir/monitors.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
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
    /// `--linger`: seconds the monitors stay after the headset leaves.
    pub linger: u32,
    /// The pairing code the person at the PC types, if asked.
    pub typed_code: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            fps: 0,
            linger: 30,
            typed_code: None,
        }
    }
}

/// A running PC app. Stopped (killed if it has to be) when dropped.
pub struct Pc {
    child: Child,
    pub addr: SocketAddr,
    log: Arc<Mutex<Vec<String>>>,
}

impl Pc {
    pub fn start(dir: &TestDir, options: &Options) -> Self {
        let root = dir.path();
        for sub in ["config", "run", "home"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_prostir"));
        cmd.args(["--port", "0", "--no-discovery", "--no-adb", "--software"])
            .args(["--bitrate", "8", "--linger", &options.linger.to_string()])
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("HOME", root.join("home"))
            .env("PROSTIR_FAKE_DESKTOP", "1")
            .env("PROSTIR_NO_COUNT", "1")
            .env("PROSTIR_FAKE_FPS", options.fps.to_string())
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
        };
        let line = pc.wait_log(Duration::from_secs(30), "waiting for the headset on port");
        let port = line.rsplit(' ').next().unwrap().parse().unwrap();
        pc.addr.set_port(port);
        pc
    }

    /// The first log line containing `needle`, waiting up to `timeout` for it.
    pub fn wait_log(&mut self, timeout: Duration, needle: &str) -> String {
        let until = Instant::now() + timeout;
        loop {
            if let Some(line) = self.log.lock().unwrap().iter().find(|l| l.contains(needle)) {
                return line.clone();
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

    /// Asks the PC app to quit as a user would (SIGTERM) and checks that it does.
    pub fn quit(mut self) {
        // SAFETY: a signal to our own child process.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
