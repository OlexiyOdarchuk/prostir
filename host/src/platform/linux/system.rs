//! Linux system bits that do not depend on the desktop: files, the single instance,
//! memory, programs run with a time limit.

use std::io::{ErrorKind, Read};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::warn;

use crate::platform::audio;
use crate::platform::linux::hyprland;

/// Whether the laptop lid is open (true on machines without a lid).
pub fn lid_open() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc/acpi/button/lid") else {
        return true;
    };
    entries
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("state")).ok())
        .all(|state| !state.contains("closed"))
}

/// A directory only this user can use: `$XDG_RUNTIME_DIR`, or `~/.cache/prostir`
/// without it (never the shared `/tmp`, where another user could plant files).
pub fn runtime_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    let dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".cache/prostir");
    let _ = std::fs::create_dir_all(&dir);
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    dir
}

/// Where per-user settings go: `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// One Prostir per desktop session: a second one would fight over the monitors (and
/// on Hyprland, its start-up clean-up would remove the first one's). The lock file is
/// held for the life of the process.
pub fn single_instance() -> Result<std::fs::File> {
    let dir = runtime_dir();
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

/// Removes what a run that crashed left behind: Hyprland's monitors (without the
/// headset nobody sees their windows) and the sound devices.
pub fn clean_up_after_crash() {
    if hyprland::running() {
        hyprland::remove_leftovers();
    }
    audio::restore_after_crash();
}

/// After a session the freed buffers would stay with the process: hand them back to
/// the system, so Prostir waiting in the background stays small.
pub fn give_memory_back() {
    // SAFETY: plain glibc call; only releases memory that is already free.
    unsafe { libc::malloc_trim(0) };
}

/// The PC's name, for the headset's list of PCs.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "linux".into())
}

/// IPv4 broadcast addresses of all interfaces that are up.
pub fn broadcast_addresses() -> Vec<Ipv4Addr> {
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

/// Runs a program to the end like `Command::output` (no input, its output and errors
/// captured), but stops it after `timeout`: a tool that hangs must not hang Prostir.
pub fn run(cmd: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Read while it runs: a program that writes a lot would block on a full pipe.
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let mut pause = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let program = cmd.get_program().to_string_lossy();
            warn!("{program} did not finish within {timeout:?}: stopped it");
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                format!("{program} did not finish within {timeout:?}"),
            ));
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    };
    // Something it started may keep the pipes open: wait only until the deadline.
    let collect = |rx: Receiver<Vec<u8>>| {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    };
    Ok(Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

/// All that comes out of `pipe`, read on a thread of its own.
fn read_in_background(pipe: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        let _ = std::thread::Builder::new()
            .name("child-output".into())
            .spawn(move || {
                let mut data = Vec::new();
                let _ = pipe.read_to_end(&mut data);
                let _ = tx.send(data);
            });
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_returns_the_output() {
        let out = run(
            Command::new("sh").args(["-c", "echo hi; echo oops >&2; exit 3"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.stderr, b"oops\n");
        assert_eq!(out.status.code(), Some(3));
        // More than a pipe holds.
        let out = run(
            Command::new("sh").args(["-c", "head -c 1000000 /dev/zero"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout.len(), 1_000_000);
    }

    #[test]
    fn run_stops_a_program_that_hangs() {
        let started = Instant::now();
        let e = run(Command::new("sleep").arg("10"), Duration::from_millis(200)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        let missing = run(
            &mut Command::new("prostir-no-such-tool"),
            Duration::from_secs(1),
        );
        assert_eq!(missing.unwrap_err().kind(), ErrorKind::NotFound);
    }
}
