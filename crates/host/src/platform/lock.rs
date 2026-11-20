//! The lock file of the one Prostir of a user: held (`flock`) for the life of the
//! process that took it, and saying who that is (`watcher 1234`), so `prostir open`
//! can wake the sockets' watcher. The same on every Linux desktop and the fake one.

use std::io::{Seek, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Another process of the user holds the lock: Prostir runs already. Says who.
#[derive(Debug)]
pub struct Held(pub String);

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "held by {}", self.0)
    }
}

impl std::error::Error for Held {}

/// Whether `e` says Prostir runs already (its lock is held by another process).
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "the method of a trait object reads worse as a path"
)]
pub fn is_held(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Held>())
}

/// Takes the lock at `path` for `role`: the open file, held until it is dropped.
pub fn take(path: &Path, role: &str) -> Result<std::fs::File> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    // SAFETY: flock on a file descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let held = std::fs::read_to_string(path).unwrap_or_default();
        bail!(Held(held.trim().to_owned()));
    }
    file.set_len(0)?;
    file.rewind()?;
    write!(file, "{role} {}", std::process::id())?;
    Ok(file)
}

/// The process id of whoever holds the lock at `path` as `role`, if anyone does.
pub fn holder(path: &Path, role: &str) -> Option<u32> {
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: flock on a file descriptor this function owns; taken only to see
    // whether it can be, and let go at once.
    let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0;
    if free {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let (who, pid) = text.trim().split_once(' ')?;
    (who == role).then(|| pid.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One holder at a time, which says who it is; free again once dropped.
    #[test]
    fn one_holder_who_says_who_it_is() {
        let dir = std::env::temp_dir().join(format!("prostir-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prostir.lock");
        assert_eq!(holder(&path, "watcher"), None);
        let held = take(&path, "watcher").unwrap();
        assert_eq!(holder(&path, "watcher"), Some(std::process::id()));
        assert_eq!(holder(&path, "serve"), None, "another role");
        let again = take(&path, "serve").unwrap_err();
        assert!(format!("{again:#}").contains("watcher"), "{again:#}");
        assert!(is_held(&again.context("Prostir is already running")));
        drop(held);
        // A program another test starts at this moment shares the lock until its
        // `exec` closes it: a moment.
        let started = std::time::Instant::now();
        while holder(&path, "watcher").is_some() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "let go"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let serve = take(&path, "serve").unwrap();
        assert_eq!(holder(&path, "serve"), Some(std::process::id()));
        drop(serve);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
