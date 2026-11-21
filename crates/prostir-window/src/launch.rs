//! Programs the window starts: Prostir's service ("Start Prostir"), and the file manager
//! for the theme's folder. Every child is waited for on a thread of its own, so none
//! lingers as a zombie of the window.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

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

/// This program (`prostir`), to start again (the service, the window): as a wrapper
/// (Nix) names it, else this very file. The file is looked up once (`prostir` does it
/// first thing), and without the " (deleted)" Linux adds once a package update
/// replaced it while it runs: the update's file is at the same path.
pub fn this_program() -> io::Result<PathBuf> {
    static EXE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    if let Some(exe) = std::env::var_os("PROSTIR_EXE").filter(|e| !e.is_empty()) {
        return Ok(exe.into());
    }
    EXE.get_or_init(|| {
        std::env::current_exe()
            .map(|exe| undeleted(&exe))
            .map_err(|e| e.to_string())
    })
    .clone()
    .map_err(io::Error::other)
}

/// `path` without the " (deleted)" that Linux adds to the file of a running program
/// once that file was replaced or removed (`/proc/<pid>/exe`, `current_exe`).
pub fn undeleted(path: &Path) -> PathBuf {
    match path.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        Some(rest) => PathBuf::from(OsStr::from_bytes(rest)),
        None => path.to_path_buf(),
    }
}

/// Whether another Prostir program runs for this user: a process of this program's
/// name from another file (an older version still running after an update, another
/// install), from /proc. This program's own processes (the service asleep or awake,
/// another window) are not counted: `prostir` handles each of them; nor is one that
/// still runs the file a package update replaced (its file reads " (deleted)").
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
            && std::fs::read_link(pid_dir.join("exe"))
                .is_ok_and(|other| undeleted(&other) != undeleted(exe))
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

    #[test]
    fn a_replaced_file_is_named_without_deleted() {
        let same = |p: &str| undeleted(Path::new(p));
        assert_eq!(
            same("/usr/bin/prostir (deleted)"),
            Path::new("/usr/bin/prostir")
        );
        assert_eq!(same("/usr/bin/prostir"), Path::new("/usr/bin/prostir"));
        assert_eq!(
            same("/a (deleted)/prostir"),
            Path::new("/a (deleted)/prostir")
        );
    }

    /// A program whose file a package update replaced while it runs (a new file
    /// renamed over it, as dpkg and rpm do): /proc names its file with " (deleted)",
    /// and it is the same program as the file at that path now, not another one.
    #[test]
    fn a_program_updated_while_it_runs_is_the_same_program() {
        let dir = std::env::temp_dir().join(format!("prostir-updated-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prostir-upd");
        std::fs::copy("/bin/sleep", &path).unwrap();
        // A test on another thread that starts a program while the copy is open for
        // writing passes that on to its child for a moment: "text file busy" until then.
        let mut tries = 0;
        let mut child = loop {
            match Command::new(&path).arg("30").spawn() {
                Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && tries < 100 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                started => break started.unwrap(),
            }
        };
        let pid_dir = Path::new("/proc").join(child.id().to_string());
        // Until the child runs the program (no longer the test's own file after fork).
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_link(pid_dir.join("exe")).ok().as_deref() != Some(path.as_path()) {
            assert!(
                std::time::Instant::now() < until,
                "the program never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::copy("/bin/sleep", dir.join("new")).unwrap();
        std::fs::rename(dir.join("new"), &path).unwrap();
        let replaced = std::fs::read_link(pid_dir.join("exe")).unwrap();
        let proc = Path::new("/proc");
        let same = running_in(proc, "prostir-upd", &path);
        let other = running_in(proc, "prostir-upd", Path::new("/elsewhere/prostir-upd"));
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            replaced.as_os_str().as_bytes().ends_with(b" (deleted)"),
            "{replaced:?}"
        );
        assert_eq!(undeleted(&replaced), path);
        assert!(!same, "the same program, updated: not another one");
        assert!(other, "a program from another file");
    }
}
