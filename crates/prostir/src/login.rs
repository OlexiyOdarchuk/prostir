//! A bare `prostir` started with the session. Up to 0.1 that started the background
//! part (an XDG autostart entry with `Exec=prostir`, `exec-once = prostir` on
//! Hyprland); since 0.2 it opens the window. Started so after an update, it runs as
//! `prostir serve` instead, without the window (which still opens at the very first
//! start, and for a Prostir already running, as `serve` does).

use std::ffi::OsString;

/// How long after Hyprland started a bare `prostir` counts as its `exec-once`, in
/// seconds.
const HYPRLAND_START: u64 = 30;

/// Who started this program with the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    /// An XDG autostart entry: the desktop's session manager, or systemd's unit for it.
    Autostart,
    /// Hyprland's config (`exec-once`), right after the compositor started.
    Hyprland,
}

/// What tells how this program was started, read before anything else runs.
#[derive(Debug, Default)]
pub struct Signs {
    /// `DESKTOP_AUTOSTART_ID`, which session managers give what they autostart.
    pub autostart_id: Option<OsString>,
    /// `/proc/self/cgroup`.
    pub cgroup: String,
    /// The parent process's name (`/proc/<parent>/comm`).
    pub parent: String,
    /// `HYPRLAND_INSTANCE_SIGNATURE`: `<hash>_<unix time it started>_<random>`.
    pub hyprland: Option<String>,
    /// Seconds since 1970, now.
    pub now: u64,
}

impl Signs {
    /// This process's own. `DESKTOP_AUTOSTART_ID` is taken off the environment, so the
    /// programs Prostir starts (its window, a `prostir` of the window's "Start
    /// Prostir") are not taken for autostarted ones.
    ///
    /// Called first thing in main, before any thread starts: it changes the environment.
    pub fn take() -> Self {
        let autostart_id = std::env::var_os("DESKTOP_AUTOSTART_ID");
        if autostart_id.is_some() {
            // SAFETY: at the start of main, before any thread exists, so nothing reads
            // the environment meanwhile.
            unsafe { std::env::remove_var("DESKTOP_AUTOSTART_ID") };
        }
        // SAFETY: getppid has no preconditions and cannot fail.
        let parent = unsafe { libc::getppid() };
        Self {
            autostart_id,
            cgroup: std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default(),
            parent: std::fs::read_to_string(format!("/proc/{parent}/comm"))
                .map(|c| c.trim_end().to_owned())
                .unwrap_or_default(),
            hyprland: std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok(),
            now: crate::unix_now(),
        }
    }

    /// Whether, and by whom, this program was started with the session.
    pub fn login(&self) -> Option<Login> {
        let session_manager = self.autostart_id.as_ref().is_some_and(|id| !id.is_empty());
        // systemd starts the unit's program itself; a program that one starts (the
        // window's "Start Prostir") stays in the unit but has another parent.
        let autostart_unit = self.parent == "systemd" && self.cgroup.lines().any(autostart_unit);
        if session_manager || autostart_unit {
            return Some(Login::Autostart);
        }
        let started = self.hyprland.as_deref().and_then(hyprland_started)?;
        (self.now.saturating_sub(started) <= HYPRLAND_START).then_some(Login::Hyprland)
    }
}

/// Whether a line of `/proc/self/cgroup` puts this process in a unit of systemd's XDG
/// autostart generator (`app-<entry>@autostart.service`).
fn autostart_unit(line: &str) -> bool {
    let unit = line.rsplit('/').next().unwrap_or_default();
    unit.starts_with("app-") && unit.ends_with("@autostart.service")
}

/// When the Hyprland of `signature` started (seconds since 1970).
fn hyprland_started(signature: &str) -> Option<u64> {
    let mut parts = signature.split('_');
    let (_hash, time, _random) = (parts.next()?, parts.next()?, parts.next()?);
    time.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn signs() -> Signs {
        Signs {
            cgroup: "0::/user.slice/user-1000.slice/user@1000.service/app.slice/kitty-1.scope\n"
                .into(),
            parent: "fish".into(),
            now: NOW,
            ..Signs::default()
        }
    }

    /// From a terminal or the menu: not at login.
    #[test]
    fn a_start_by_hand_is_not_at_login() {
        assert_eq!(signs().login(), None);
        let empty_id = Signs {
            autostart_id: Some(OsString::new()),
            ..signs()
        };
        assert_eq!(empty_id.login(), None);
    }

    /// A session manager's autostart (KDE's, GNOME's, Xfce's) gives an id.
    #[test]
    fn the_session_managers_autostart() {
        let s = Signs {
            autostart_id: Some("10d8f2c1a3".into()),
            ..signs()
        };
        assert_eq!(s.login(), Some(Login::Autostart));
    }

    /// systemd's unit for an XDG autostart entry (Plasma and GNOME on systemd): the
    /// program systemd started itself, not one the program started.
    #[test]
    fn systemds_unit_for_an_autostart_entry() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-prostir@autostart.service\n";
        let s = Signs {
            cgroup: cgroup.into(),
            parent: "systemd".into(),
            ..signs()
        };
        assert_eq!(s.login(), Some(Login::Autostart));
        let child = Signs {
            cgroup: cgroup.into(),
            parent: "prostir".into(),
            ..signs()
        };
        assert_eq!(child.login(), None, "started by the program in that unit");
        let other_unit = Signs {
            cgroup: "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.kde.konsole-123.scope\n".into(),
            parent: "systemd".into(),
            ..signs()
        };
        assert_eq!(other_unit.login(), None);
    }

    /// Hyprland's `exec-once`: within 30 s of the compositor's start, from the time in
    /// its instance signature.
    #[test]
    fn hyprlands_exec_once() {
        let hyprland = |started: u64| Signs {
            hyprland: Some(format!(
                "4520b30d498daca8079365bdb909a8dea38e8d55_{started}_1981813508"
            )),
            ..signs()
        };
        assert_eq!(hyprland(NOW - 2).login(), Some(Login::Hyprland));
        assert_eq!(hyprland(NOW - 30).login(), Some(Login::Hyprland));
        assert_eq!(hyprland(NOW - 31).login(), None, "later: by hand");
        let odd = Signs {
            hyprland: Some("not-a-signature".into()),
            ..signs()
        };
        assert_eq!(odd.login(), None);
    }
}
