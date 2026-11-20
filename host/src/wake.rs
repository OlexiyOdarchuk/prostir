//! Asleep until a headset comes: nothing of the full program runs while no headset
//! needs it, and the sockets wait with whoever holds them.
//!
//! - With systemd, `prostir.socket` holds them and starts `prostir.service` when
//!   something comes. The program then sorts first ([`sort`], the light phase): it
//!   answers discovery queries, and starts the rest (GStreamer, the tray, the local
//!   API) only for a connection, a query from one of its own headsets, a press of the
//!   button of the notice of a new headset, or a start by hand (nothing waiting).
//!   Otherwise it leaves at once, so a stranger's query wakes nothing heavy.
//! - Without systemd, the sockets' watcher ([`watch`], `prostir serve` itself) holds
//!   them: it sleeps in the kernel on the sockets, its signals and a notice, answers
//!   queries the same way, and starts the full program as a child process with the
//!   sockets passed in systemd's way. While the child runs, the watcher does not
//!   touch the sockets; when it ends, it goes back to sleep. A child that crashed is
//!   started again only for something new, after a pause of 2, 4, 8... up to 60 s.
//!
//! The full program leaves once nothing needs it (see `crate::door`): with code 0
//! back to sleep, with [`QUIT_ALL`] when the person asked Prostir to quit (the
//! watcher then quits too).

use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{info, warn};

use crate::door::{self, Door};
use crate::find::{self, Responder};
use crate::i18n::t;
use crate::platform::dialogs::{self, Notice};
use crate::platform::signals;
use crate::platform::sockets::{self, Sockets};

/// The full program's exit code when the person asked Prostir to quit: the watcher
/// quits too, instead of waiting for the headset.
pub const QUIT_ALL: i32 = 10;

/// The longest pause before a full program that crashed is started again.
const LONGEST_PAUSE: Duration = Duration::from_secs(60);
/// A full program that ran this long before it crashed counts as having worked: the
/// pauses start from the shortest again.
const RAN_WELL: Duration = Duration::from_secs(60);
/// How long the watcher waits for the full program to quit before it ends it.
const QUIT_WAIT: Duration = Duration::from_secs(10);

/// Why the full program starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// A connection is waiting.
    Connection,
    /// One of the PC's own headsets asked for it: it connects in a moment.
    Headset,
    /// Started by hand (`prostir open`, `systemctl start`): nothing was waiting.
    Started,
    /// `prostir open` asked the watcher for the settings window (SIGUSR1).
    Window,
    /// The button of the notice of a new headset: the pairing window opens.
    Pair,
}

/// The notice of a new headset nearby, with its button (see `crate::door`).
pub fn new_headset_notice() -> Option<Notice> {
    dialogs::notice(
        t("Новий шолом поруч", "New headset nearby"),
        t("Сполучити?", "Pair it?"),
        t("Сполучити", "Pair"),
        t(
            "Щоб сполучити, виберіть «Сполучити новий шолом» у меню Prostir.",
            "To pair it, choose \"Pair a new headset\" in the Prostir menu.",
        ),
    )
}

/// What answers queries while the full program does not run: the PC's key, port and
/// name, the headsets it trusts (read from their file each time: the full program
/// pairs new ones), and the notice of a new headset.
struct Sorter {
    responder: Responder,
    trust: PathBuf,
    /// The PC's find secret's file (the full program makes a new one when it forgets a
    /// headset).
    secret: PathBuf,
    door: Door,
    notice: Option<(Notice, Instant)>,
}

impl Sorter {
    fn new(port: u16) -> Result<Self> {
        let dir = crate::pairing::config_dir();
        let secret = crate::pairing::find_secret_file(&dir);
        // Made here on the first start, as the full program would.
        crate::pairing::load_find_secret(&secret)?;
        Ok(Self {
            responder: Responder::new(port, crate::platform::hostname()),
            trust: dir.join("paired_headsets"),
            secret,
            door: Door::default(),
            notice: None,
        })
    }

    /// Answers the datagram waiting on `udp`: [`Wake::Headset`] when one of the PC's
    /// own headsets asked. A stranger it does not answer brings the notice.
    fn query(&mut self, udp: &std::net::UdpSocket) -> Option<Wake> {
        let (trust, secret) = (&self.trust, &self.secret);
        let open = || {
            let trusted = prostir_proto::secure::TrustStore::try_load(trust);
            let secret = crate::pairing::load_find_secret(secret);
            match (trusted, secret) {
                (Ok(trusted), Ok(secret)) => find::Open {
                    paired: !trusted.is_empty(),
                    pairing: false,
                    secret,
                },
                // Closed rather than open: an unreadable file must not make every
                // stranger welcome.
                (trusted, secret) => {
                    if let Err(e) = &trusted {
                        warn!("discovery: {e}");
                    }
                    if let Err(e) = &secret {
                        warn!("discovery: {e:#}");
                    }
                    find::Open {
                        paired: true,
                        pairing: false,
                        secret: secret.unwrap_or_else(|_| prostir_proto::discovery::new_secret()),
                    }
                }
            }
        };

        let asker = match find::answer_one(udp, &mut self.responder, open) {
            Ok(Some(asker)) => asker,
            Ok(None) => return None,
            Err(e) => {
                warn!("discovery: {e}");
                return None;
            }
        };
        if asker.ours {
            info!("one of this PC's headsets asked for it at {}", asker.from);
            return Some(Wake::Headset);
        }
        let now = Instant::now();
        if self.door.asked(&asker, now) && self.notice.is_none() {
            info!(
                "a new headset at {} asked: telling the person at the PC",
                asker.from
            );
            self.notice = new_headset_notice().map(|n| (n, now + door::NOTICE_TIME));
        }
        None
    }

    /// The notice's descriptor, while one is up.
    fn notice_fd(&self) -> Option<RawFd> {
        self.notice.as_ref().map(|(n, _)| n.fd())
    }

    /// The notice's descriptor is readable: [`Wake::Pair`] for its button.
    fn notice_said(&mut self) -> Option<Wake> {
        let (notice, _) = self.notice.as_mut()?;
        match notice.check()? {
            true => {
                self.notice = None;
                Some(Wake::Pair)
            }
            false => {
                self.notice = None;
                None
            }
        }
    }

    /// At `now`: the notice given up on at its time. How long until then.
    fn notice_left(&mut self, now: Instant) -> Option<Duration> {
        let until = self.notice.as_ref()?.1;
        if now >= until {
            self.notice = None;
            return None;
        }
        Some(until - now)
    }
}

/// The light phase under systemd: answers what is waiting on `sockets`, and says
/// whether the full program is to start (and why). `None`: nothing for it, the
/// program leaves.
pub fn sort(sockets: &Sockets) -> Result<Option<Wake>> {
    let mut sorter = Sorter::new(sockets.port())?;
    let tcp = sockets.tcp.as_raw_fd();
    let mut first = true;
    loop {
        let wait = sorter.notice_left(Instant::now()).unwrap_or(Duration::ZERO);
        let mut fds = vec![tcp];
        fds.extend(sockets.udp.as_ref().map(AsRawFd::as_raw_fd));
        fds.extend(sorter.notice_fd());
        let ready = sockets::readable(&fds, Some(wait))?;
        let at = |fd: RawFd| fds.iter().position(|f| *f == fd).is_some_and(|i| ready[i]);
        if !ready.contains(&true) {
            if first {
                return Ok(Some(Wake::Started));
            }
            if sorter.notice.is_none() {
                return Ok(None);
            }
            continue;
        }
        first = false;
        if at(tcp) {
            return Ok(Some(Wake::Connection));
        }
        if let Some(udp) = &sockets.udp
            && at(udp.as_raw_fd())
            && let Some(wake) = sorter.query(udp)
        {
            return Ok(Some(wake));
        }
        if let Some(fd) = sorter.notice_fd()
            && at(fd)
            && let Some(wake) = sorter.notice_said()
        {
            return Ok(Some(wake));
        }
    }
}

/// How to start the full program.
pub struct Start {
    pub exe: PathBuf,
    /// Its arguments (`serve` and this run's), without what says why it starts.
    pub args: Vec<String>,
}

impl Start {
    /// This program again, with this run's arguments: a wrapper's name for it (Nix,
    /// `PROSTIR_EXE`), else this very file (looked up now, while it is surely there).
    pub fn this_program() -> Result<Self> {
        let exe = match std::env::var_os("PROSTIR_EXE").filter(|e| !e.is_empty()) {
            Some(exe) => exe.into(),
            None => std::env::current_exe()?,
        };
        let mut args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().map(String::as_str) != Some("serve") {
            args.insert(0, "serve".into());
        }
        // The watcher's own: it starts the program for the window once.
        args.retain(|a| a != "--open-settings");
        Ok(Self { exe, args })
    }

    fn args_for(&self, wake: Wake) -> Vec<String> {
        let mut args = self.args.clone();
        args.push("--woken".into());
        match wake {
            Wake::Pair => args.push("--pair".into()),
            Wake::Window => args.push("--open-settings".into()),
            Wake::Connection | Wake::Headset | Wake::Started => {}
        }
        args
    }
}

/// The full program running as the watcher's child.
struct Child {
    pid: libc::pid_t,
    started: Instant,
}

/// How a child ended, from `waitpid`'s status.
fn ended(status: libc::c_int) -> Option<i32> {
    libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status))
}

/// The pause before the next start after `fails` crashes in a row.
fn pause_after(fails: u32) -> Duration {
    let secs = 1u64 << fails.clamp(1, 6);
    Duration::from_secs(secs).min(LONGEST_PAUSE)
}

/// The sockets' watcher: holds `sockets` and starts the full program with `start`
/// when a headset comes, until asked to quit. `window`: start it at once for the
/// settings window (`prostir open` when nothing ran). Its exit code.
pub fn watch(sockets: Sockets, start: Start, window: bool) -> Result<i32> {
    let pipe = signals::install(
        &[
            signals::QUIT_SIGNALS.as_slice(),
            &[libc::SIGUSR1, libc::SIGCHLD],
        ]
        .concat(),
    )?;
    let mut sorter = Sorter::new(sockets.port())?;
    let tcp = sockets.tcp.as_raw_fd();
    let udp = sockets.udp.as_ref().map(AsRawFd::as_raw_fd);
    let mut child: Option<Child> = None;
    let mut fails = 0u32;
    let mut not_before: Option<Instant> = None;
    let launch = |wake: Wake, child: &mut Option<Child>| {
        info!("starting Prostir: {wake:?}");
        match sockets::start_with(&start.exe, &start.args_for(wake), &sockets) {
            Ok(pid) => {
                *child = Some(Child {
                    pid,
                    started: Instant::now(),
                })
            }
            Err(e) => warn!("cannot start Prostir ({}): {e}", start.exe.display()),
        }
    };
    if let Some(udp) = &sockets.udp {
        info!(
            "answering headsets that ask for this PC: discovery on UDP port {}",
            udp.local_addr()?.port()
        );
    }
    info!(
        "asleep until a headset comes: waiting for the headset on port {}",
        sockets.port()
    );
    if window {
        launch(Wake::Window, &mut child);
    }
    loop {
        // A child that ended (its SIGCHLD may have been one of several at once).
        if let Some(c) = &child {
            let mut status = 0;
            // SAFETY: waits without blocking for this process's own child.
            let pid = unsafe { libc::waitpid(c.pid, &mut status, libc::WNOHANG) };
            if pid == c.pid {
                let ran = c.started.elapsed();
                child = None;
                match ended(status) {
                    Some(0) => {
                        fails = 0;
                        info!("Prostir went to sleep: waiting for the headset");
                    }
                    Some(QUIT_ALL) => {
                        info!("Prostir was asked to quit: the watcher quits too");
                        return Ok(0);
                    }
                    how => {
                        fails = if ran >= RAN_WELL { 1 } else { fails + 1 };
                        let pause = pause_after(fails);
                        warn!(
                            "Prostir ended ({}): started again for the next headset, in {} s at the soonest",
                            how.map_or_else(|| "killed".to_owned(), |c| format!("code {c}")),
                            pause.as_secs()
                        );
                        not_before = Some(Instant::now() + pause);
                    }
                }
            }
        }
        let now = Instant::now();
        let paused = not_before.filter(|t| *t > now);
        let mut fds = vec![pipe.as_raw_fd()];
        if child.is_none() {
            if paused.is_none() {
                fds.push(tcp);
            }
            fds.extend(udp);
        }
        fds.extend(sorter.notice_fd());
        let wait = crate::brain::earliest([paused, sorter.notice_left(now).map(|left| now + left)])
            .map(|at| at.saturating_duration_since(Instant::now()));
        let ready = sockets::readable(&fds, wait)?;
        let at = |fd: RawFd| fds.iter().position(|f| *f == fd).is_some_and(|i| ready[i]);
        if at(pipe.as_raw_fd()) {
            match signals::wait_for_signal(&pipe) {
                Some(libc::SIGCHLD) => continue,
                Some(libc::SIGUSR1) => {
                    if child.is_none() {
                        launch(Wake::Window, &mut child);
                    }
                    continue;
                }
                Some(signal) => {
                    info!("{} received: quitting", signals::name(signal));
                    if let Some(c) = child.take() {
                        stop(c.pid);
                    }
                    return Ok(0);
                }
                None => anyhow::bail!("reading signals failed"),
            }
        }
        if child.is_some() {
            if let Some(fd) = sorter.notice_fd()
                && at(fd)
                && sorter.notice_said() == Some(Wake::Pair)
            {
                // The program runs already: it opens pairing on the bus.
                open_pairing_on_the_bus();
            }
            continue;
        }
        let mut wake = None;
        if at(tcp) {
            wake = Some(Wake::Connection);
        } else if let (Some(fd), Some(socket)) = (udp, &sockets.udp)
            && at(fd)
        {
            wake = sorter.query(socket).filter(|_| paused.is_none());
        }
        if wake.is_none()
            && let Some(fd) = sorter.notice_fd()
            && at(fd)
        {
            wake = sorter.notice_said();
        }
        if let Some(wake) = wake {
            launch(wake, &mut child);
        }
    }
}

/// Asks the full program to quit, and waits for it (ending it if it takes too long).
fn stop(pid: libc::pid_t) {
    // SAFETY: signals and waits for this process's own child.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
        let until = Instant::now() + QUIT_WAIT;
        let mut status = 0;
        while libc::waitpid(pid, &mut status, libc::WNOHANG) == 0 {
            if Instant::now() >= until {
                warn!("Prostir did not quit in time: ending it");
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The notice's button while the full program runs: pairing opens through its local
/// API.
fn open_pairing_on_the_bus() {
    let opened = crate::platform::local_api::Client::connect(Duration::from_secs(5))
        .map_err(|e| format!("{e:?}"))
        .and_then(|client| {
            use crate::ctl::CtlClient;
            client
                .call(&prostir_api::Request::PairNewHeadset)
                .map_err(|e| format!("{e:?}"))
        });
    if let Err(e) = opened {
        warn!("cannot open pairing: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After crashes in a row the pauses double, up to a minute.
    #[test]
    fn pauses_after_crashes_grow_to_a_minute() {
        let secs: Vec<u64> = (1..=8).map(|n| pause_after(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 60, 60, 60]);
    }

    /// The full program starts with this run's arguments and what says why.
    #[test]
    fn the_program_starts_with_why() {
        let start = Start {
            exe: "/usr/bin/prostir".into(),
            args: vec!["serve".into(), "--linger".into(), "5".into()],
        };
        assert_eq!(
            start.args_for(Wake::Pair),
            ["serve", "--linger", "5", "--woken", "--pair"]
        );
        assert_eq!(
            start.args_for(Wake::Window),
            ["serve", "--linger", "5", "--woken", "--open-settings"]
        );
        assert_eq!(
            start.args_for(Wake::Headset),
            ["serve", "--linger", "5", "--woken"]
        );
    }
}
