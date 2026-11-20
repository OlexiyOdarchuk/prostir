//! The PC app under test: the `prostir` binary built with the fake desktop, started in
//! a directory of its own (settings, keys, runtime files), on a port the system picks,
//! with a session bus of its own for the local API (`dbus-daemon --session`, where
//! it is installed).

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
        self.keys_of("headset")
    }

    /// The keys of another fake headset, `name`.
    pub fn keys_of(&self, name: &str) -> Keys {
        Keys::load(&self.0.join(name)).unwrap()
    }

    /// Makes the PC and the headset trust each other, as after pairing.
    pub fn pair(&self) -> Keys {
        self.pair_headset("headset")
    }

    /// Makes the PC and another fake headset, `name`, trust each other.
    pub fn pair_headset(&self, name: &str) -> Keys {
        let pc = Identity::load_or_create(&self.config().join("prostir/identity")).unwrap();
        let mut keys = self.keys_of(name);
        keys.trusted.add(pc.public, "fake PC").unwrap();
        TrustStore::load(&self.config().join("prostir/paired_headsets"))
            .add(keys.identity.public, name)
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

    /// The set of screens the PC app starts with, as this version saves it.
    pub fn save_screens(&self, text: &str) {
        self.save("screens.txt", text);
    }

    /// The settings the PC app starts with, as this version saves them.
    pub fn save_settings(&self, text: &str) {
        self.save("settings.txt", text);
    }

    fn save(&self, name: &str, text: &str) {
        let path = self.config().join("prostir").join(name);
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
    /// How long connecting to the fake desktop takes.
    pub slow_connect: Duration,
    /// How long a step of the desktop's worker takes before the desktop counts as not
    /// responding, and before the PC app gives up and ends, in seconds (none: 15 s
    /// and 2 min, as in a release).
    pub desk_limits: Option<(u64, u64)>,
    /// How long a request waits for the desktop's answer, in seconds (none: 20 s).
    pub desk_answer: Option<u64>,
    /// `--linger`: seconds the monitors stay after the headset leaves.
    pub linger: u64,
    /// The pairing code the person at the PC types, if asked.
    pub typed_code: Option<String>,
    /// How long the person at the PC takes to type it.
    pub typed_code_delay: Duration,
    /// Monitors (by number, from 1) the fake desktop cannot make.
    pub broken_monitors: String,
    /// The fake PC's own displays (`PROSTIR_FAKE_PHYSICAL`: `eDP-1=EDID@1920x1080;…`).
    pub physical: String,
    /// `--software`: encode on the CPU whatever the settings say (the default, the same
    /// everywhere); without it, as `settings.txt` says.
    pub software: bool,
    /// How many headsets may have a session at once (`PROSTIR_TEST_MAX_HEADSETS`, a
    /// build for these tests only); 0: as a release allows.
    pub headsets: usize,
    /// A fake hardware encoder (`PROSTIR_FAKE_ENCODER`: `works`, `sessions=N`,
    /// `refuses=N`, `broken`, `hangs`, `slow`), tried before every other; none: the PC's own
    /// encoders only.
    pub encoder: Option<&'static str>,
    /// More of the PC app's environment (`PROSTIR_FAKE_NO_SOUND`, `PATH`).
    pub env: Vec<(String, String)>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            fps: 0,
            noise: false,
            bitrate: 8,
            slow_monitors: Duration::ZERO,
            slow_connect: Duration::ZERO,
            desk_limits: None,
            desk_answer: None,
            linger: 30,
            typed_code: None,
            typed_code_delay: Duration::ZERO,
            broken_monitors: String::new(),
            physical: String::new(),
            software: true,
            headsets: 0,
            encoder: None,
            env: Vec::new(),
        }
    }
}

/// A session bus of a test's own: a `dbus-daemon` stopped when dropped.
pub struct Bus {
    daemon: Child,
    pub address: String,
}

impl Bus {
    /// A new bus with its socket in `dir`; `None` (saying so) without `dbus-daemon`.
    pub fn start(dir: &Path) -> Option<Self> {
        let socket = dir.join("bus");
        let _ = std::fs::remove_file(&socket);
        let spawned = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .arg(format!("--address=unix:path={}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut daemon = match spawned {
            Ok(d) => d,
            // CI installs it: there the tests of the local API must run.
            Err(e) if std::env::var_os("CI").is_some() => {
                panic!("no dbus-daemon ({e}), which CI installs for the local API's tests")
            }
            Err(e) => {
                eprintln!(
                    "\n*** no dbus-daemon ({e}): the tests of the local API are SKIPPED ***\n"
                );
                return None;
            }
        };
        let mut line = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Some(Self {
            daemon,
            address: line.trim().to_owned(),
        })
    }

    /// A connection to it, as a client of the PC app.
    pub fn connect(&self) -> zbus::blocking::Connection {
        zbus::blocking::connection::Builder::address(self.address.as_str())
            .unwrap()
            .method_timeout(Duration::from_secs(40))
            .build()
            .unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// What `prostir ctl` did: its exit code and what it wrote.
#[derive(Debug)]
pub struct Ctl {
    pub code: i32,
    pub out: String,
    pub err: String,
}

impl Ctl {
    /// Its output as JSON (it was run with `--json`).
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(self.out.trim())
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.out))
    }
}

/// Runs `prostir ctl ARGS` on the session bus at `address`.
pub fn ctl_on(address: &str, args: &[&str]) -> Ctl {
    let out = Command::new(env!("CARGO_BIN_EXE_prostir"))
        .arg("ctl")
        .args(args)
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        .env("LANGUAGE", "en")
        .stdin(Stdio::null())
        .output()
        .expect("run prostir ctl");
    Ctl {
        code: out.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A running PC app. Stopped (killed if it has to be) when dropped.
pub struct Pc {
    child: Child,
    pub addr: SocketAddr,
    log: Arc<Mutex<Vec<String>>>,
    /// Its `XDG_RUNTIME_DIR`.
    run_dir: PathBuf,
    /// Its session bus, where `dbus-daemon` is installed.
    pub bus: Option<Bus>,
}

impl Pc {
    pub fn start(dir: &TestDir, options: &Options) -> Self {
        let root = dir.path();
        for sub in ["config", "run", "home", "cache"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let bus = Bus::start(&root.join("run"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_prostir"));
        cmd.args(["--port", "0", "--no-discovery", "--no-adb"])
            .args(options.software.then_some("--software"))
            .args(["--bitrate", &options.bitrate.to_string()])
            .args(["--linger", &options.linger.to_string()])
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            // What the encoders' probes found, kept for the next start of this test.
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("HOME", root.join("home"))
            .env("PROSTIR_FAKE_DESKTOP", "1")
            .env("PROSTIR_NO_COUNT", "1")
            .env("PROSTIR_FAKE_FPS", options.fps.to_string())
            .env(
                "PROSTIR_FAKE_SLOW_MONITORS_MS",
                options.slow_monitors.as_millis().to_string(),
            )
            .env(
                "PROSTIR_FAKE_SLOW_CONNECT_MS",
                options.slow_connect.as_millis().to_string(),
            )
            .env("PROSTIR_FAKE_BROKEN_MONITORS", &options.broken_monitors)
            .env("PROSTIR_FAKE_PHYSICAL", &options.physical)
            .env(
                "PROSTIR_FAKE_TYPED_CODE_DELAY_MS",
                options.typed_code_delay.as_millis().to_string(),
            )
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
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match &bus {
            Some(bus) => cmd.env("DBUS_SESSION_BUS_ADDRESS", &bus.address),
            None => cmd.env_remove("DBUS_SESSION_BUS_ADDRESS"),
        };
        match options.headsets {
            0 => cmd.env_remove("PROSTIR_TEST_MAX_HEADSETS"),
            n => cmd.env("PROSTIR_TEST_MAX_HEADSETS", n.to_string()),
        };
        match options.encoder {
            Some(mode) => cmd.env("PROSTIR_FAKE_ENCODER", mode),
            None => cmd.env_remove("PROSTIR_FAKE_ENCODER"),
        };
        match (options.desk_limits, options.desk_answer) {
            (None, None) => cmd.env_remove("PROSTIR_TEST_DESK_LIMITS"),
            (limits, answer) => {
                let (stuck, give_up) = limits.unwrap_or((15, 120));
                let answer = answer.unwrap_or(20);
                cmd.env(
                    "PROSTIR_TEST_DESK_LIMITS",
                    format!("{stuck},{give_up},{answer}"),
                )
            }
        };
        match &options.typed_code {
            Some(code) => cmd.env("PROSTIR_FAKE_TYPED_CODE", code),
            None => cmd.env_remove("PROSTIR_FAKE_TYPED_CODE"),
        };
        cmd.envs(options.env.iter().map(|(k, v)| (k, v)));
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
            bus,
        };
        let line = pc.wait_log(Duration::from_secs(30), "waiting for the headset on port");
        let port = line.rsplit(' ').next().unwrap().parse().unwrap();
        pc.addr.set_port(port);
        if pc.bus.is_some() {
            pc.wait_log(Duration::from_secs(10), "local API on D-Bus as");
        }
        pc
    }

    /// Stops its session bus and starts another at the same address, as a session
    /// bus restarted would be; `false` without one.
    pub fn restart_bus(&mut self) -> bool {
        if self.bus.take().is_none() {
            return false;
        }
        self.bus = Bus::start(&self.run_dir);
        self.bus.is_some()
    }

    /// Runs `prostir ctl ARGS` against this PC app; `None` without a session bus
    /// (no `dbus-daemon`: the test is skipped then, saying so).
    pub fn ctl(&self, args: &[&str]) -> Option<Ctl> {
        let Some(bus) = &self.bus else {
            eprintln!("no session bus: `prostir ctl {}` skipped", args.join(" "));
            return None;
        };
        Some(ctl_on(&bus.address, args))
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
        self.order("fake-tray", item);
    }

    /// Writes `text` to the file `name` the fake desktop watches, whole at once: made
    /// under another name and moved there, so the PC app never reads it half written
    /// (it would take an empty file and remove it, and the command would be lost).
    fn order(&self, name: &str, text: &str) {
        let path = self.run_dir.join(name);
        let new = self.run_dir.join(format!("{name}.new"));
        std::fs::write(&new, format!("{text}\n")).unwrap();
        std::fs::rename(&new, &path).unwrap();
    }

    /// Has another program do `command` on the fake desktop (see the fake desktop
    /// for the commands, e.g. `resize Fake-2 320x240`).
    pub fn desktop(&self, command: &str) {
        self.order("fake-desktop", command);
    }

    /// How many times each thread of the PC app went to sleep and woke since it
    /// started (`voluntary_ctxt_switches`), by thread id, with the thread's name.
    pub fn wakeups(&self) -> BTreeMap<u32, (String, u64)> {
        let mut threads = BTreeMap::new();
        let tasks = format!("/proc/{}/task", self.child.id());
        for task in std::fs::read_dir(tasks).into_iter().flatten().flatten() {
            let Ok(tid) = task.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let name = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
            let status = std::fs::read_to_string(task.path().join("status")).unwrap_or_default();
            let woke = status
                .lines()
                .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
                .and_then(|n| n.trim().parse().ok());
            if let Some(woke) = woke {
                threads.insert(tid, (name.trim().to_owned(), woke));
            }
        }
        threads
    }

    /// The files the PC app has open now that are graphics devices (`/dev/dri/...`):
    /// one per card it encodes on, however many monitors it streams.
    pub fn gpu_files(&self) -> Vec<String> {
        let fds = format!("/proc/{}/fd", self.child.id());
        std::fs::read_dir(fds)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .map(|target| target.to_string_lossy().into_owned())
            .filter(|target| target.starts_with("/dev/dri/"))
            .collect()
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

    /// Waits up to `timeout` for the PC app to end by itself: how it ended.
    pub fn wait_end(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let until = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < until,
                "the PC app did not end within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
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
