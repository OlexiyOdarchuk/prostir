//! Counting how many PCs use Prostir, without identifying any of them.
//!
//! Once a day, when a headset connects, the PC sends one request to GoatCounter with
//! the path `/day/YYYY-MM-DD`; the number of requests for a day is the number of PCs
//! that used Prostir on it. The first time ever it also sends `/install`. Nothing else
//! goes along: no id, no host name, no settings. GoatCounter keeps no IP addresses.
//!
//! On by default, off with the tray item or `PROSTIR_NO_COUNT=1`. The first start says
//! so in a notification, and nothing is sent before the next day, so there is time to
//! turn it off before anything leaves the PC. Only official builds
//! count: the key is set at build time (`PROSTIR_COUNT_TOKEN`), so builds from source
//! and forks never send anything.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{info, warn};

use crate::i18n::t;
use crate::pairing::config_dir;

const TOKEN: Option<&str> = option_env!("PROSTIR_COUNT_TOKEN");
const SITE: &str = match option_env!("PROSTIR_COUNT_SITE") {
    Some(s) => s,
    None => "ishawyha",
};

fn switch_file() -> Option<PathBuf> {
    Some(config_dir().join("count"))
}

fn state_file() -> Option<PathBuf> {
    Some(config_dir().join("counted"))
}

/// The day the user was told about the counter (`YYYY-MM-DD`).
fn notice_file() -> Option<PathBuf> {
    Some(config_dir().join("count-notice"))
}

/// On the first start: says that Prostir counts its users and how to turn it off.
/// Counting starts the day after this.
pub fn announce_once() {
    if !available() || !enabled() {
        return;
    }
    let Some(notice) = notice_file() else { return };
    if notice.exists() {
        return;
    }
    // Remembered only once the notification was really shown: without a notification
    // daemon nothing is counted, since the user was never told.
    if notify_first_time() {
        let _ = std::fs::create_dir_all(notice.parent().unwrap_or(&notice));
        let _ = std::fs::write(&notice, format!("{}\n", today()));
    }
}

/// Whether this build counts at all.
pub fn available() -> bool {
    TOKEN.is_some_and(|t| !t.is_empty())
}

/// The user's choice (on unless turned off).
pub fn enabled() -> bool {
    if std::env::var_os("PROSTIR_NO_COUNT").is_some_and(|v| v != "0") {
        return false;
    }
    switch_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_none_or(|s| s.trim() != "off")
}

pub fn set_enabled(on: bool) {
    if let Some(p) = switch_file() {
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
        if let Err(e) = std::fs::write(&p, if on { "on\n" } else { "off\n" }) {
            warn!("cannot save the counting choice: {e}");
        }
    }
    info!("counting this PC among users: {on}");
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

/// A headset connected: count this PC for today, at most once a day.
pub fn headset_connected() {
    if !available() || !enabled() {
        return;
    }
    let Some(state) = state_file() else { return };
    let date = today();
    // Not before the day after the user was told (and could turn it off).
    let told = notice_file().and_then(|p| std::fs::read_to_string(p).ok());
    match told.as_deref().map(str::trim) {
        None => {
            announce_once();
            return;
        }
        Some(day) if day >= date.as_str() => return,
        Some(_) => {}
    }
    let last = std::fs::read_to_string(&state).ok();
    if last.as_deref().map(str::trim) == Some(date.as_str()) {
        return;
    }
    let first = last.is_none();
    let _ = std::thread::Builder::new()
        .name("usage".into())
        .spawn(move || {
            let mut paths = vec![format!("/day/{date}")];
            if first {
                paths.push("/install".into());
            }
            if send(&paths) {
                let _ = std::fs::write(&state, format!("{date}\n"));
            }
        });
}

/// One POST through curl (always present on desktops), with the key passed on stdin
/// so it never shows up in the process list.
fn send(paths: &[String]) -> bool {
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

/// Shows the notice; `true` when the notification was shown.
fn notify_first_time() -> bool {
    crate::platform::dialogs::notify_and_wait(
        t("Prostir рахує користувачів", "Prostir counts its users"),
        t(
            "Починаючи із завтра, раз на день ПК надсилатиме анонімний запит без даних про вас, щоб було видно, скільки людей користується Prostir. Вимкнути: пункт трею «Рахувати цей ПК серед користувачів» або PROSTIR_NO_COUNT=1.",
            "From tomorrow, once a day the PC sends an anonymous request with nothing about you, to show how many people use Prostir. To turn it off: the tray item \"Count this PC among users\" or PROSTIR_NO_COUNT=1.",
        ),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn date_has_the_right_shape() {
        let d = super::today();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }
}
