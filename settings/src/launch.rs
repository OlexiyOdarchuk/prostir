//! Programs the window starts: Prostir itself ("Start Prostir"), and the file manager
//! for the theme's folder. Every child is waited for on a thread of its own, so none
//! lingers as a zombie of the window.

use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How starting Prostir went, as far as the window can tell at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
    /// It was started; it shows up on the bus when it is ready.
    Launched,
    /// It runs already (and does not answer this window): a second copy would only
    /// fight the first for the network port.
    AlreadyRunning,
}

/// Starts the service ("Start Prostir"); an error means it could not even be run.
pub type Starter = Box<dyn FnMut() -> io::Result<Started>>;

/// `cmd` detached from the window (its own process group, no terminal), reaped
/// when it ends.
pub fn spawn_reaped(cmd: &mut Command) -> io::Result<()> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    reap(child);
    Ok(())
}

fn reap(mut child: Child) {
    std::thread::spawn(move || child.wait());
}

/// The first `name` on `PATH` that is a file.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Whether a process called `name` runs for this user, from /proc.
pub fn running(name: &str) -> bool {
    running_in(Path::new("/proc"), name)
}

fn running_in(proc: &Path, name: &str) -> bool {
    let Ok(me) = std::fs::metadata(proc.join("self")).map(|m| m.uid()) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(proc) else {
        return false;
    };
    entries.flatten().any(|e| {
        let pid_dir = e.path();
        let is_pid = e
            .file_name()
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
        is_pid
            && std::fs::metadata(&pid_dir).is_ok_and(|m| m.uid() == me)
            && std::fs::read_to_string(pid_dir.join("comm")).is_ok_and(|c| c.trim_end() == name)
    })
}

/// Starts Prostir unless it runs already: `prostir open`, as the menu entry does
/// (Prostir starts and asks for the settings window, which is this one: it comes
/// forward), and the program itself where an older Prostir refuses the word at once.
pub fn start_prostir() -> io::Result<Started> {
    if running("prostir") {
        return Ok(Started::AlreadyRunning);
    }
    let exe = find_in_path("prostir")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "prostir is not on PATH"))?;
    std::thread::Builder::new()
        .name("start-prostir".into())
        .spawn(move || {
            if !open_known(&exe, ["open"])
                && let Err(e) = spawn_reaped(&mut Command::new(&exe))
            {
                log::warn!("could not start {}: {e}", exe.display());
            }
        })?;
    Ok(Started::Launched)
}

/// Runs `exe args`; false when it fails within a few seconds (it does not know
/// them). Still running after that, it is left to run and reaped.
fn open_known(exe: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> bool {
    let child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    reap(child);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_this_process_by_its_name() {
        let me = std::fs::read_to_string("/proc/self/comm").unwrap();
        assert!(running(me.trim_end()));
        assert!(!running("no-such-program-here"));
    }

    #[test]
    fn a_program_that_refuses_the_word_is_told_apart() {
        assert!(!open_known(Path::new("/bin/sh"), ["-c", "exit 2"]));
        assert!(open_known(Path::new("/bin/sh"), ["-c", "exit 0"]));
        assert!(!open_known(Path::new("/nonexistent/prostir"), ["open"]));
    }
}
