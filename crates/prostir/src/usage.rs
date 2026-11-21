//! Counting how many PCs use Prostir, without identifying any of them.
//!
//! Once a day, when a headset connects, the PC sends one request to GoatCounter with
//! the path `/day/YYYY-MM-DD`; the number of requests for a day is the number of PCs
//! that used Prostir on it. The first time ever it also sends `/install`. Nothing else
//! goes along: no id, no host name, no settings. GoatCounter keeps no IP addresses.
//!
//! Only with the person's yes: the settings window asks once ("Count this PC among
//! users?"), and the answer is kept in the settings (`count unasked|yes|no`), changed
//! later in the window, the tray or `prostir ctl count`. Until the answer is yes
//! nothing is sent, so a PC where nobody opened the window (only `prostir serve`, a
//! server, CI) never counts. `PROSTIR_NO_COUNT=1` turns it off whatever the answer.
//! Every build may count, as it asks first. The key, GoatCounter's, which can only add
//! counts, is in the source (`crates/prostir/count.key`, read when the program is
//! built; empty: that build never counts); `PROSTIR_COUNT_TOKEN` at build time takes
//! its place when set. A build without the `count` feature (a distribution that does
//! not want it), a debug build and the tests never count, and never ask.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prostir_api::Counting;

use crate::pairing::config_dir;

/// The day this PC was last counted (`YYYY-MM-DD`).
fn state_file() -> PathBuf {
    config_dir().join("counted")
}

/// Whether this build counts at all.
pub fn available() -> bool {
    sender::available()
}

/// Whether this run may count: the build counts and `PROSTIR_NO_COUNT` does not say
/// no. Otherwise there is nothing to ask the person either.
pub fn possible() -> bool {
    available() && std::env::var_os("PROSTIR_NO_COUNT").is_none_or(|v| v == "0")
}

/// What to send now, if anything: `Some(first)` (`first`: `/install` too) only for a
/// yes on a run that may count, and only once a day (`last`: the day counted last,
/// none before the first time).
fn to_send(count: Counting, possible: bool, last: Option<&str>, today: &str) -> Option<bool> {
    if count != Counting::Yes || !possible {
        return None;
    }
    match last.map(str::trim) {
        Some(day) if day == today => None,
        last => Some(last.is_none()),
    }
}

/// Today's date in UTC as `YYYY-MM-DD` (days since the epoch to a civil date).
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// A headset connected: count this PC for today, at most once a day, if the person
/// said yes (`count`, from the settings).
pub fn headset_connected(count: Counting) {
    if count != Counting::Yes || !possible() {
        return;
    }
    let state = state_file();
    let date = today();
    let last = std::fs::read_to_string(&state).ok();
    let Some(first) = to_send(count, true, last.as_deref(), &date) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("usage".into())
        .spawn(move || {
            let mut paths = vec![format!("/day/{date}")];
            if first {
                paths.push("/install".into());
            }
            if sender::send(&paths) {
                let _ = std::fs::write(&state, format!("{date}\n"));
            }
        });
}

/// The real counter: GoatCounter, with the `count` feature; it counts only in a
/// release build, never in a debug build or the tests.
#[cfg(all(feature = "count", not(feature = "fake-desktop")))]
mod sender {
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// The key: `PROSTIR_COUNT_TOKEN` at build time, else `count.key`; none when
    /// empty.
    const TOKEN: Option<&str> = {
        let key = match option_env!("PROSTIR_COUNT_TOKEN") {
            Some(t) if !t.trim_ascii().is_empty() => t,
            _ => include_str!("../count.key"),
        };
        let key = key.trim_ascii();
        if key.is_empty() { None } else { Some(key) }
    };
    const SITE: &str = match option_env!("PROSTIR_COUNT_SITE") {
        Some(s) => s,
        None => "ishawyha",
    };

    pub fn available() -> bool {
        cfg!(not(any(debug_assertions, test))) && TOKEN.is_some()
    }

    /// One POST through curl (always present on desktops), with the key passed on
    /// stdin so it never shows up in the process list.
    pub fn send(paths: &[String]) -> bool {
        let Some(token) = TOKEN else { return false };
        let hits: Vec<String> = paths
            .iter()
            .map(|p| format!(r#"{{"path":"{p}"}}"#))
            .collect();
        let body = format!(r#"{{"no_sessions":true,"hits":[{}]}}"#, hits.join(","));
        let config = format!(
            "url = \"https://{SITE}.goatcounter.com/api/v0/count\"\n\
             request = \"POST\"\n\
             header = \"Authorization: Bearer {token}\"\n\
             header = \"Content-Type: application/json\"\n\
             data = \"{}\"\n\
             silent\nfail\nmax-time = 15\noutput = \"/dev/null\"\n",
            body.replace('\\', "\\\\").replace('"', "\\\"")
        );
        // -q first: the user's ~/.curlrc must not log the request or send it elsewhere.
        let child = Command::new("curl")
            .args(["-q", "--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return false };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(config.as_bytes());
        }
        child.wait().is_ok_and(|s| s.success())
    }
}

/// The end-to-end tests' counter: a build with the fake desktop counts only when
/// `PROSTIR_FAKE_COUNT` names a file, and writes each path it would send there, a
/// line each, instead of sending anything.
#[cfg(feature = "fake-desktop")]
mod sender {
    use std::io::Write;

    fn file() -> Option<std::path::PathBuf> {
        std::env::var_os("PROSTIR_FAKE_COUNT")
            .filter(|f| !f.is_empty())
            .map(Into::into)
    }

    pub fn available() -> bool {
        file().is_some()
    }

    pub fn send(paths: &[String]) -> bool {
        let Some(file) = file() else { return false };
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
        else {
            return false;
        };
        paths.iter().all(|p| writeln!(f, "{p}").is_ok())
    }
}

/// A build without the `count` feature: it never counts.
#[cfg(all(not(feature = "count"), not(feature = "fake-desktop")))]
mod sender {
    pub fn available() -> bool {
        false
    }

    pub fn send(_paths: &[String]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_has_the_right_shape() {
        let d = today();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }

    /// Nothing is sent without a yes: not while unasked, not after a no, not for an
    /// answer this build does not know, and not where the run may not count.
    #[test]
    fn nothing_is_sent_without_a_yes() {
        let today = "2026-11-21";
        for count in [Counting::Unasked, Counting::No, Counting::Other] {
            for last in [None, Some("2026-11-20\n")] {
                assert_eq!(to_send(count, true, last, today), None, "{count:?}");
            }
        }
        assert_eq!(to_send(Counting::Yes, false, None, today), None);
    }

    /// A yes counts once a day, the first time ever with `/install`.
    #[test]
    fn a_yes_counts_once_a_day() {
        let today = "2026-11-21";
        assert_eq!(to_send(Counting::Yes, true, None, today), Some(true));
        assert_eq!(
            to_send(Counting::Yes, true, Some("2026-11-20\n"), today),
            Some(false)
        );
        assert_eq!(
            to_send(Counting::Yes, true, Some("2026-11-21\n"), today),
            None
        );
    }

    /// A run that may count only in a build that counts; the tests never do (but for
    /// the fake desktop's file, `PROSTIR_FAKE_COUNT`).
    #[test]
    fn only_a_build_that_counts_may() {
        assert!(!possible() || available());
        #[cfg(not(feature = "fake-desktop"))]
        assert!(!available() && !possible());
    }
}
