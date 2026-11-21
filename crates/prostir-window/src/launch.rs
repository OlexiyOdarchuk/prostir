//! Programs the window starts: Prostir's service ("Start Prostir"), and the file manager
//! for the theme's folder. Every child is waited for on a thread of its own, so none
//! lingers as a zombie of the window.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// How starting Prostir went, as far as the window can tell at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
    /// It was started; it shows up on the bus when it is ready.
    Launched,
    /// Another Prostir program runs (an older version, another install) and does not
    /// answer this window: a second copy would only fight it for the network port.
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

/// This program (`prostir`), to start the service with: as a wrapper (Nix) names it,
/// else this very file.
fn this_program() -> io::Result<PathBuf> {
    match std::env::var_os("PROSTIR_EXE").filter(|e| !e.is_empty()) {
        Some(exe) => Ok(exe.into()),
        None => std::env::current_exe(),
    }
}

/// Whether another Prostir program runs for this user: a process of this program's
/// name from another file (an older version still running after an update, another
/// install), from /proc. This program's own processes (the service asleep or awake,
/// another window) are not counted: `prostir` handles each of them.
fn other_program_running() -> bool {
    let (Ok(name), Ok(exe)) = (
        std::fs::read_to_string("/proc/self/comm"),
        std::fs::read_link("/proc/self/exe"),
    ) else {
        return false;
    };
    running_in(Path::new("/proc"), name.trim_end(), &exe)
}

/// Whether a process of this user called `name` runs from a file other than `exe`.
fn running_in(proc: &Path, name: &str, exe: &Path) -> bool {
    let Ok(uid) = std::fs::metadata(proc.join("self")).map(|m| m.uid()) else {
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
            && std::fs::metadata(&pid_dir).is_ok_and(|m| m.uid() == uid)
            && std::fs::read_to_string(pid_dir.join("comm")).is_ok_and(|c| c.trim_end() == name)
            && std::fs::read_link(pid_dir.join("exe")).is_ok_and(|other| other != exe)
    })
}

/// Starts Prostir unless another Prostir program runs: `prostir`, as the menu entry
/// does (it wakes or starts the service, which asks for the settings window: this one
/// comes forward).
pub fn start_prostir() -> io::Result<Started> {
    if other_program_running() {
        return Ok(Started::AlreadyRunning);
    }
    spawn_reaped(&mut Command::new(this_program()?))?;
    Ok(Started::Launched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_process_by_its_name_from_another_file() {
        let me = std::fs::read_to_string("/proc/self/comm").unwrap();
        let exe = std::fs::read_link("/proc/self/exe").unwrap();
        let proc = Path::new("/proc");
        assert!(running_in(
            proc,
            me.trim_end(),
            Path::new("/elsewhere/prostir")
        ));
        assert!(!running_in(proc, me.trim_end(), &exe));
        assert!(!running_in(
            proc,
            "no-such-program-here",
            Path::new("/elsewhere")
        ));
    }
}
