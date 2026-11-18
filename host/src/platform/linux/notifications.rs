//! The PC's desktop notifications, shown in the headset too (when the user turns it
//! on there): the session bus is watched for `org.freedesktop.Notifications.Notify`
//! calls, the way `dbus-monitor` does, and each one's title and text are passed on.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use log::{info, warn};
use zbus::zvariant::OwnedValue;

/// Longest text passed on (a notification body can be long).
const MAX_CHARS: usize = 140;

pub struct Watcher {
    on: Arc<AtomicBool>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The thread stops at the next notification (it waits for one).
        self.on.store(false, Ordering::Relaxed);
    }
}

/// Starts passing notifications to the headset: `show` gets the text of each one.
pub fn watch(show: impl Fn(String) + Send + 'static) -> Option<Watcher> {
    let conn = zbus::blocking::Connection::session()
        .inspect_err(|e| warn!("notifications: no session bus: {e}"))
        .ok()?;
    let rules =
        vec!["type='method_call',interface='org.freedesktop.Notifications',member='Notify'"];
    if let Err(e) = conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus.Monitoring"),
        "BecomeMonitor",
        &(rules, 0u32),
    ) {
        warn!("notifications: cannot watch the session bus: {e}");
        return None;
    }
    let on = Arc::new(AtomicBool::new(true));
    let running = on.clone();
    std::thread::Builder::new()
        .name("notifications".into())
        .spawn(move || {
            info!("passing PC notifications to the headset");
            for msg in zbus::blocking::MessageIterator::from(&conn) {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(msg) = msg else { continue };
                type Notify = (
                    String,
                    u32,
                    String,
                    String,
                    String,
                    Vec<String>,
                    HashMap<String, OwnedValue>,
                    i32,
                );
                let Ok((app, _, _, summary, body, ..)) = msg.body().deserialize::<Notify>() else {
                    continue;
                };
                if let Some(text) = shown_text(&app, &summary, &body) {
                    show(text);
                }
            }
            info!("PC notifications no longer passed to the headset");
        })
        .ok()?;
    Some(Watcher { on })
}

/// "App: title. Text", shortened; `None` for Prostir's own notifications.
fn shown_text(app: &str, summary: &str, body: &str) -> Option<String> {
    if app.eq_ignore_ascii_case("prostir") {
        return None;
    }
    // Bodies may carry simple markup.
    let plain = |s: &str| {
        let mut out = String::new();
        let mut in_tag = false;
        for c in s.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                '\n' | '\r' | '\t' => out.push(' '),
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let (summary, body) = (plain(summary), plain(body));
    let mut text = match (app.is_empty(), summary.is_empty()) {
        (false, false) => format!("{app}: {summary}"),
        (true, false) => summary,
        (false, true) => app.to_owned(),
        (true, true) => return None,
    };
    if !body.is_empty() {
        text = format!("{text}. {body}");
    }
    if text.chars().count() > MAX_CHARS {
        text = text.chars().take(MAX_CHARS - 1).collect::<String>() + "…";
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_plain_and_short() {
        assert_eq!(
            shown_text("Telegram", "Anna", "<b>Hi</b>\nthere").as_deref(),
            Some("Telegram: Anna. Hi there")
        );
        assert_eq!(shown_text("Prostir", "Paired", ""), None);
        let long = shown_text("A", &"x".repeat(300), "").unwrap();
        assert_eq!(long.chars().count(), MAX_CHARS);
    }
}
