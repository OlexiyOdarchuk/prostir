//! `adb reverse` for a headset on USB, so it reaches the PC through 127.0.0.1, with
//! nothing running while no headset is plugged in.
//!
//! - The kernel says when a USB device comes or goes (its uevents, over netlink: a
//!   thread sleeps on the socket). Only Meta's devices (vendor 2833) start anything;
//!   whether one is plugged in is read from `/sys/bus/usb/devices`.
//! - While one is, `adb track-devices` runs: the adb server tells it every change of a
//!   device's state, so a headset whose owner allows USB debugging minutes later (from
//!   `unauthorized` to `device`) gets its `adb reverse` then, with nothing asked again
//!   and again. It ends when the adb server ends (another tool restarted it, say): it is
//!   started again, and the reverse set up again on the new server.
//! - An `adb reverse` that fails is tried again after 5, 10, 20… up to 60 s while the
//!   device stays ready.
//! - Without the kernel's events (no netlink), `/sys` is looked at every 10 s instead.
//!   Without adb, nothing runs at all.

use std::collections::BTreeSet;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use log::{info, warn};

use super::system::run;

/// Meta's USB vendor id (the Quest's), as the kernel writes it.
const META: &str = "2833";

/// The kernel's own uevents (udev passes them on in group 2, which needs udev).
const KERNEL_EVENTS: u32 = 1;

/// Without the kernel's events, how often `/sys` is looked at.
const POLL_WITHOUT_EVENTS: Duration = Duration::from_secs(10);

/// The longest wait before a failed `adb reverse` is tried again, and before a
/// `track-devices` that ended is started again.
const LONGEST_RETRY: Duration = Duration::from_secs(60);
const LONGEST_RESTART: Duration = Duration::from_secs(30);

/// What the adb thread hears.
enum Note {
    /// A USB device came or went.
    Usb,
    /// The devices `track-devices` number `n` lists now: serial and state.
    Devices(u64, Vec<(String, String)>),
    /// `track-devices` number `n` ended.
    TrackEnded(u64),
    /// Stop (tests only).
    #[cfg(test)]
    Stop,
}

/// Whether a uevent is about a USB device that matters: one of Meta's plugged in or
/// ready (a driver bound, its configuration changed), or any that went (to look again
/// whether a Meta one is left). Its interfaces' events say nothing more.
fn usb_event_matters(event: &[u8]) -> bool {
    let (mut action, mut subsystem, mut devtype, mut product) = (None, None, None, None);
    // The header (`add@/devices/…`) first, then KEY=value fields.
    for field in event.split(|b| *b == 0).skip(1) {
        if let Some(v) = field.strip_prefix(b"ACTION=") {
            action = Some(v);
        } else if let Some(v) = field.strip_prefix(b"SUBSYSTEM=") {
            subsystem = Some(v);
        } else if let Some(v) = field.strip_prefix(b"DEVTYPE=") {
            devtype = Some(v);
        } else if let Some(v) = field.strip_prefix(b"PRODUCT=") {
            product = Some(v);
        }
    }
    // PRODUCT is vendor/product/version in hex without leading zeros: 2833/186/ffff.
    let meta = product.is_some_and(|p| p.split(|b| *b == b'/').next() == Some(META.as_bytes()));
    subsystem == Some(b"usb")
        && devtype == Some(b"usb_device")
        && match action {
            Some(b"add" | b"bind" | b"change") => meta,
            Some(b"remove") => true,
            _ => false,
        }
}

/// Sends [`Note::Usb`] on a thread of its own each time a USB device that matters
/// comes or goes. An error when the kernel's events cannot be read.
fn follow_usb(notes: Sender<Note>) -> std::io::Result<()> {
    // SAFETY: a plain socket call; the descriptor is owned right below.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: an all-zero sockaddr_nl is valid; its family and group are set below.
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = KERNEL_EVENTS;
    // SAFETY: binds the socket to an address of the size given.
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if bound != 0 {
        return Err(std::io::Error::last_os_error());
    }
    std::thread::Builder::new()
        .name("usb".into())
        .spawn(move || {
            let mut buf = vec![0u8; 8192];
            loop {
                // SAFETY: receives into a buffer of the length given.
                let n = unsafe {
                    libc::recv(socket.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0)
                };
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    match e.raw_os_error() {
                        Some(libc::EINTR) => continue,
                        // Events were lost while the queue was full: one may matter.
                        Some(libc::ENOBUFS) => {}
                        _ => {
                            warn!("USB devices are no longer followed: {e}");
                            return;
                        }
                    }
                } else if !usb_event_matters(&buf[..n as usize]) {
                    continue;
                }
                if notes.send(Note::Usb).is_err() {
                    return;
                }
            }
        })?;
    Ok(())
}

/// Whether one of Meta's devices is plugged in, as `/sys` lists them (`root`).
fn meta_plugged_in(root: &Path) -> bool {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .any(|d| std::fs::read_to_string(d.path().join("idVendor")).is_ok_and(|v| v.trim() == META))
}

/// Reads what `adb track-devices` writes: each change a list, four hex digits of
/// length then `serial\tstate` lines. Calls `on_list` with each list, and returns at
/// its end.
fn read_lists(mut out: impl Read, mut on_list: impl FnMut(Vec<(String, String)>) -> bool) {
    let mut len = [0u8; 4];
    loop {
        if out.read_exact(&mut len).is_err() {
            return;
        }
        let Some(n) = std::str::from_utf8(&len)
            .ok()
            .and_then(|s| usize::from_str_radix(s, 16).ok())
        else {
            return;
        };
        let mut text = vec![0u8; n];
        if out.read_exact(&mut text).is_err() {
            return;
        }
        let list = String::from_utf8_lossy(&text)
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(serial, state)| (serial.to_owned(), state.trim().to_owned()))
            .collect();
        if !on_list(list) {
            return;
        }
    }
}

/// Where to find adb and the devices, and the port to reverse.
struct Follow {
    adb: PathBuf,
    /// `/sys/bus/usb/devices`, or a test's.
    devices: PathBuf,
    spec: String,
    /// Whether the kernel's events come (otherwise `/sys` is looked at now and then).
    events: bool,
}

/// `adb track-devices` running, number `n`.
struct Tracker {
    n: u64,
    child: Child,
}

impl Drop for Tracker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The time to wait after `wait`, doubled up to `longest`.
fn longer(wait: Duration, longest: Duration) -> Duration {
    (wait * 2).min(longest)
}

impl Follow {
    /// Starts `adb track-devices`, its lists read on a thread of its own; an error of
    /// kind `NotFound` when adb is not installed.
    fn track(&self, n: u64, notes: &Sender<Note>) -> std::io::Result<Tracker> {
        let mut child = Command::new(&self.adb)
            .arg("track-devices")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let out = child.stdout.take().expect("piped above");
        let notes = notes.clone();
        let tracker = Tracker { n, child };
        std::thread::Builder::new()
            .name("adb-devices".into())
            .spawn(move || {
                read_lists(out, |list| notes.send(Note::Devices(n, list)).is_ok());
                let _ = notes.send(Note::TrackEnded(n));
            })?;
        Ok(tracker)
    }

    /// Sets `adb reverse` up for `serial`.
    fn reverse(&self, serial: &str) -> bool {
        run(
            Command::new(&self.adb).args(["-s", serial, "reverse", &self.spec, &self.spec]),
            Duration::from_secs(5),
        )
        .is_ok_and(|out| out.status.success())
    }

    /// Follows the headsets on USB until told to stop (never, but in tests).
    fn run(&self, notes: &Receiver<Note>, sender: &Sender<Note>) {
        let mut tracker: Option<Tracker> = None;
        let mut last = 0;
        // Ready devices (`device`) from the last list, and those reversed.
        let mut ready: Vec<String> = Vec::new();
        let mut reversed: BTreeSet<String> = BTreeSet::new();
        let mut retry: Option<(Instant, Duration)> = None;
        let mut restart: Option<(Instant, Duration)> = None;
        loop {
            let now = Instant::now();
            let plugged = meta_plugged_in(&self.devices);
            if !plugged && tracker.is_some() {
                info!("no headset on USB any more: adb is left alone");
                (tracker, retry, restart) = (None, None, None);
                ready.clear();
                reversed.clear();
            }
            let may_start = restart.is_none_or(|(at, _)| now >= at);
            if plugged && tracker.is_none() && may_start {
                last += 1;
                match self.track(last, sender) {
                    Ok(t) => tracker = Some(t),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        info!("a headset is on USB, but adb is not installed: no adb reverse");
                        return;
                    }
                    Err(e) => {
                        warn!("cannot follow the devices adb sees: {e}");
                        let wait = restart
                            .map_or(Duration::from_secs(1), |(_, w)| longer(w, LONGEST_RESTART));
                        restart = Some((now + wait, wait));
                    }
                }
            }
            if tracker.is_some() && retry.is_none_or(|(at, _)| now >= at) {
                let mut failed = false;
                let waiting: Vec<String> = ready
                    .iter()
                    .filter(|s| !reversed.contains(*s))
                    .cloned()
                    .collect();
                for serial in &waiting {
                    if self.reverse(serial) {
                        info!("USB headset {serial}: adb reverse {} active", self.spec);
                        reversed.insert(serial.clone());
                    } else {
                        failed = true;
                    }
                }
                retry = failed.then(|| {
                    let wait =
                        retry.map_or(Duration::from_secs(5), |(_, w)| longer(w, LONGEST_RETRY));
                    (Instant::now() + wait, wait)
                });
            }
            let deadline = [
                retry.map(|(at, _)| at),
                restart.filter(|_| plugged).map(|(at, _)| at),
                (!self.events).then(|| Instant::now() + POLL_WITHOUT_EVENTS),
            ]
            .into_iter()
            .flatten()
            .min();
            let note = match deadline {
                None => notes.recv().ok(),
                Some(at) => notes
                    .recv_timeout(at.saturating_duration_since(Instant::now()))
                    .ok(),
            };
            match note {
                Some(Note::Devices(n, list)) if tracker.as_ref().is_some_and(|t| t.n == n) => {
                    ready = list
                        .into_iter()
                        .filter(|(serial, state)| state == "device" && !serial.contains(':'))
                        .map(|(serial, _)| serial)
                        .collect();
                    // Gone or no longer ready: set up again when it is back.
                    reversed.retain(|s| ready.contains(s));
                    retry = None;
                    // The server answers: a later end waits from the start again.
                    restart = None;
                }
                Some(Note::TrackEnded(n)) if tracker.as_ref().is_some_and(|t| t.n == n) => {
                    // The adb server ended (restarted by another tool, say): its
                    // reverses went with it.
                    info!("the adb server ended: following the devices again");
                    tracker = None;
                    ready.clear();
                    reversed.clear();
                    let wait =
                        restart.map_or(Duration::from_secs(1), |(_, w)| longer(w, LONGEST_RESTART));
                    restart = Some((Instant::now() + wait, wait));
                }
                #[cfg(test)]
                Some(Note::Stop) => return,
                // A device came or went, a timer, or news of an earlier tracker.
                _ => {}
            }
        }
    }
}

/// Keeps `adb reverse tcp:PORT` set up for every Meta headset on USB, on a thread of
/// its own (see the top of this file).
pub fn keep_adb_reverse(port: u16) {
    let (sender, notes) = mpsc::channel();
    // Before anything else: a headset plugged in from now on is not missed.
    let events = match follow_usb(sender.clone()) {
        Ok(()) => true,
        Err(e) => {
            warn!(
                "the kernel's USB events cannot be read ({e}): looking for a headset every {} s",
                POLL_WITHOUT_EVENTS.as_secs()
            );
            false
        }
    };
    let follow = Follow {
        adb: "adb".into(),
        devices: "/sys/bus/usb/devices".into(),
        spec: format!("tcp:{port}"),
        events,
    };
    let spawned = std::thread::Builder::new()
        .name("adb-reverse".into())
        .spawn(move || follow.run(&notes, &sender));
    if let Err(e) = spawned {
        warn!("no adb reverse for a headset on USB: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(fields: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in fields {
            out.extend_from_slice(f.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn only_a_meta_device_coming_or_any_going_matters() {
        let quest = event(&[
            "add@/devices/pci0000:00/0000:00:14.0/usb1/1-2",
            "ACTION=add",
            "DEVPATH=/devices/pci0000:00/0000:00:14.0/usb1/1-2",
            "SUBSYSTEM=usb",
            "DEVNAME=bus/usb/001/005",
            "DEVTYPE=usb_device",
            "PRODUCT=2833/186/ffff",
            "SEQNUM=4510",
        ]);
        assert!(usb_event_matters(&quest));
        let bound = event(&[
            "bind@/x",
            "ACTION=bind",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_device",
            "PRODUCT=2833/183/1",
        ]);
        assert!(usb_event_matters(&bound));
        let mouse = event(&[
            "add@/x",
            "ACTION=add",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_device",
            "PRODUCT=46d/c52b/1211",
        ]);
        assert!(!usb_event_matters(&mouse), "not Meta's");
        let removed = event(&[
            "remove@/x",
            "ACTION=remove",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_device",
            "PRODUCT=46d/c52b/1211",
        ]);
        assert!(usb_event_matters(&removed), "whatever went: look again");
        let interface = event(&[
            "add@/x/1-2:1.0",
            "ACTION=add",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_interface",
            "PRODUCT=2833/186/ffff",
        ]);
        assert!(!usb_event_matters(&interface));
        let block = event(&["add@/x", "ACTION=add", "SUBSYSTEM=block", "DEVTYPE=disk"]);
        assert!(!usb_event_matters(&block));
        assert!(!usb_event_matters(b"add@/devices/usb1/1-2"));
        assert!(!usb_event_matters(&[0xff, 0, 0xfe]));
    }

    #[test]
    fn the_lists_of_track_devices_are_read() {
        let one = "R3Q\tunauthorized\n";
        let two = "R3Q\tdevice\n192.168.1.5:5555\tdevice\n";
        let text = format!("0000{:04x}{one}{:04x}{two}", one.len(), two.len());
        let mut lists = Vec::new();
        read_lists(text.as_bytes(), |l| {
            lists.push(l);
            true
        });
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            lists,
            [
                vec![],
                vec![pair("R3Q", "unauthorized")],
                vec![pair("R3Q", "device"), pair("192.168.1.5:5555", "device")],
            ]
        );
        // A cut-off list ends the reading.
        let mut n = 0;
        read_lists("0010R3Q".as_bytes(), |_| {
            n += 1;
            true
        });
        assert_eq!(n, 0);
    }

    /// A fake adb: `track-devices` says what `track-N` holds (N counting the starts)
    /// and stays as long as the file says, `reverse` is written to `reversed`.
    fn fake_adb(dir: &Path) -> PathBuf {
        let adb = dir.join("adb");
        let script = format!(
            r#"#!/bin/sh
D={dir}
if [ "$1" = track-devices ]; then
    n=$(($(cat $D/starts 2>/dev/null || echo 0) + 1)); echo $n > $D/starts
    exec sh $D/track-$n
fi
if [ "$3" = reverse ]; then echo "$2" >> $D/reversed; exit 0; fi
exit 1
"#,
            dir = dir.display()
        );
        std::fs::write(&adb, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
        adb
    }

    /// A list as `track-devices` prints it, for a shell `printf`.
    fn list(lines: &str) -> String {
        // The text goes in the format, where printf reads `\t` and `\n`.
        format!(
            "printf '%04x' {}; printf '{}'",
            lines.len(),
            lines.replace('\t', "\\t").replace('\n', "\\n")
        )
    }

    fn wait_for(what: &str, until: Duration, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < until, "{what} within {until:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A Meta headset not allowed yet, then allowed: reversed once it is ready, with
    /// nothing asked meanwhile. The adb server restarted by another tool: reversed
    /// again on the new one. Unplugged: adb is left alone, no `track-devices` runs.
    #[test]
    fn a_headset_is_reversed_when_ready_and_again_after_an_adb_restart() {
        let dir = std::env::temp_dir().join(format!("prostir-adb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let usb = dir.join("usb");
        std::fs::create_dir_all(usb.join("1-2")).unwrap();
        std::fs::write(usb.join("1-2/idVendor"), "2833\n").unwrap();
        // The first server: unauthorized, allowed a moment later, then it ends.
        std::fs::write(
            dir.join("track-1"),
            format!(
                "{}; sleep 0.3; {}; sleep 0.5\n",
                list("R3Q\tunauthorized\n"),
                list("R3Q\tdevice\n")
            ),
        )
        .unwrap();
        // The second one lists it ready at once and stays.
        std::fs::write(
            dir.join("track-2"),
            format!("{}; exec sleep 30\n", list("R3Q\tdevice\n")),
        )
        .unwrap();
        let follow = Follow {
            adb: fake_adb(&dir),
            devices: usb.clone(),
            spec: "tcp:47830".into(),
            events: true,
        };
        let (sender, notes) = mpsc::channel();
        let stopper = sender.clone();
        let reversed = || {
            std::fs::read_to_string(dir.join("reversed"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let starts = || {
            std::fs::read_to_string(dir.join("starts"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0)
        };
        /// Stops the follower however the test ends, so a failure does not hang it.
        struct Stop(Sender<Note>);
        impl Drop for Stop {
            fn drop(&mut self) {
                let _ = self.0.send(Note::Stop);
            }
        }
        std::thread::scope(|s| {
            s.spawn(move || follow.run(&notes, &sender));
            let _stop = Stop(stopper.clone());
            wait_for("the reverse once ready", Duration::from_secs(5), || {
                reversed() == 1
            });
            assert_eq!(starts(), 1);
            wait_for(
                "the reverse on the new server",
                Duration::from_secs(5),
                || reversed() == 2,
            );
            assert_eq!(starts(), 2);
            // Unplugged: the tracker goes, and nothing starts again.
            std::fs::remove_dir_all(usb.join("1-2")).unwrap();
            stopper.send(Note::Usb).unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(starts(), 2);
            assert_eq!(reversed(), 2);
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
