//! Starting with the session: systemd's `prostir.socket` where the user's systemd
//! runs and the package installed the units (nothing runs until a headset comes), else
//! an XDG autostart entry (`prostir serve`: the small sockets' watcher). Hyprland
//! starts Prostir from its own config (`exec-once = prostir`, which XDG autostart
//! does not reach without uwsm), so it is not set from here there.
//!
//! The package installs the units and enables nothing. Turning autostart on with
//! systemd enables the socket and removes the XDG entry of 0.1, so the two never
//! fight over the port; it takes effect at the next login when Prostir already runs
//! otherwise now (it holds the port). Turning it off disables the socket and removes
//! the entry; Prostir running now runs on until it quits.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use log::{info, warn};

use super::system::{config_home, run};

/// The socket unit.
const SOCKET: &str = "prostir.socket";

/// How Prostir starts with the session on this PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    /// systemd's socket: nothing runs until a headset comes.
    Socket,
    /// An XDG autostart entry.
    Entry,
    /// Not from here (Hyprland's own config).
    None,
}

/// The way for a PC where the user's systemd runs (`systemd`) with the units
/// installed (`units`), on Hyprland or not.
fn pick(systemd: bool, units: bool, hyprland: bool) -> Way {
    match (systemd && units, hyprland) {
        (_, true) => Way::None,
        (true, false) => Way::Socket,
        (false, false) => Way::Entry,
    }
}

fn way() -> Way {
    pick(
        systemd_runs(),
        unit_dirs().iter().any(|d| d.join(SOCKET).is_file()),
        super::hyprland::running(),
    )
}

/// The user's systemd runs: its private socket is there.
fn systemd_runs() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|d| Path::new(&d).join("systemd/private").exists())
}

/// Where the user's systemd looks for units, the user's own first.
fn unit_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.extend(config_home().map(|c| c.join("systemd/user")));
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    dirs.extend(data_home.map(|d| d.join("systemd/user")));
    dirs.push("/etc/systemd/user".into());
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join("systemd/user")),
    );
    dirs.extend(
        ["/usr/local/lib/systemd/user", "/usr/lib/systemd/user"]
            .iter()
            .map(PathBuf::from),
    );
    dirs
}

/// Whether the socket is enabled: a link in a `sockets.target.wants` of the user's
/// systemd (the user's own, or one for every user).
fn socket_enabled() -> bool {
    [
        config_home().map(|c| c.join("systemd/user")),
        Some("/etc/systemd/user".into()),
    ]
    .into_iter()
    .flatten()
    .any(|d| d.join("sockets.target.wants").join(SOCKET).exists())
}

fn entry() -> Option<PathBuf> {
    Some(config_home()?.join("autostart").join("prostir.desktop"))
}

/// Whether this PC can start Prostir with the session from here (Hyprland without
/// uwsm ignores XDG autostart: its own config starts it).
pub fn available() -> bool {
    match way() {
        Way::Socket => true,
        Way::Entry => entry().is_some(),
        Way::None => false,
    }
}

/// Whether Prostir starts with the session.
pub fn enabled() -> bool {
    entry().is_some_and(|p| p.exists()) || (way() == Way::Socket && socket_enabled())
}

/// Turns starting with the session on or off.
pub fn set(on: bool) {
    match way() {
        Way::Socket => {
            // The entry of 0.1 would start a watcher that takes the port first.
            remove_entry();
            let _ = systemctl(&[if on { "enable" } else { "disable" }, SOCKET]);
        }
        Way::Entry if on => write_entry(),
        Way::Entry => remove_entry(),
        Way::None => {}
    }
}

/// On each start: an XDG entry names this install's program again (a Nix store path
/// moves with each upgrade).
pub fn refresh() {
    if way() == Way::Entry && entry().is_some_and(|p| p.exists()) {
        write_entry();
    }
}

/// Asked to quit while systemd's socket started Prostir: the socket stops too, until
/// the next login (without waiting: stopping it stops this very service).
pub fn quit_until_login() {
    let _ = systemctl(&["stop", "--no-block", SOCKET]);
}

/// Starts Prostir through systemd, when its socket waits for the headset: whether it
/// did (`prostir open`, Prostir not running).
pub fn start_service() -> bool {
    socket_waits() && systemctl(&["start", "--no-block", "prostir.service"])
}

/// Whether systemd's socket waits for the headset now (Prostir asleep).
pub fn socket_waits() -> bool {
    systemd_runs()
        && run(
            Command::new("systemctl").args(["--user", "--quiet", "is-active", SOCKET]),
            Duration::from_secs(5),
        )
        .is_ok_and(|out| out.status.success())
}

fn systemctl(args: &[&str]) -> bool {
    let done = run(
        Command::new("systemctl").arg("--user").args(args),
        Duration::from_secs(10),
    );
    match done {
        Ok(out) if out.status.success() => {
            info!("systemctl --user {}", args.join(" "));
            return true;
        }
        Ok(out) => warn!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => warn!("systemctl --user {}: {e}", args.join(" ")),
    }
    false
}

fn write_entry() {
    let Some(path) = entry() else { return };
    // A wrapper (Nix) names the program to start; else this very file.
    let exe = std::env::var("PROSTIR_EXE")
        .ok()
        .filter(|e| !e.is_empty())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| "prostir".into());
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=Prostir\n\
         Comment=Virtual monitors for Meta Quest\nExec={exe} serve\nIcon=prostir\n\
         Terminal=false\nX-KDE-autostart-phase=2\n"
    );
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, text));
    if let Err(e) = written {
        warn!("cannot change autostart ({}): {e}", path.display());
    }
}

fn remove_entry() {
    let Some(path) = entry() else { return };
    match std::fs::remove_file(&path) {
        Ok(()) => info!("autostart entry removed: {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("cannot remove {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The socket where systemd runs with the units, an entry elsewhere, nothing from
    /// here on Hyprland (it keeps `exec-once`).
    #[test]
    fn the_way_to_start_with_the_session() {
        assert_eq!(pick(true, true, false), Way::Socket);
        assert_eq!(pick(true, false, false), Way::Entry, "no units installed");
        assert_eq!(pick(false, true, false), Way::Entry, "no systemd");
        assert_eq!(pick(true, true, true), Way::None);
        assert_eq!(pick(false, false, true), Way::None);
    }
}
