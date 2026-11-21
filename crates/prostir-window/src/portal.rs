//! The desktop's light or dark preference: `color-scheme` in the
//! `org.freedesktop.appearance` namespace of the XDG desktop portal, read once and
//! followed through `SettingChanged`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MessageType;
use zbus::zvariant::{OwnedValue, Value};

use crate::theme::Scheme;

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

const UNKNOWN: u8 = 0;
const DARK: u8 = 1;
const LIGHT: u8 = 2;

/// What the desktop prefers, as last heard; shared with the thread that listens.
#[derive(Clone, Default)]
pub struct SystemScheme(Arc<AtomicU8>);

impl SystemScheme {
    /// None: the desktop has no preference, or no portal answers.
    pub fn get(&self) -> Option<Scheme> {
        match self.0.load(Ordering::Relaxed) {
            DARK => Some(Scheme::Dark),
            LIGHT => Some(Scheme::Light),
            _ => None,
        }
    }

    pub fn set(&self, s: Option<Scheme>) {
        let v = match s {
            Some(Scheme::Dark) => DARK,
            Some(Scheme::Light) => LIGHT,
            None => UNKNOWN,
        };
        self.0.store(v, Ordering::Relaxed);
    }

    /// Reads the preference now and follows it on a thread; `wake` after each change.
    pub fn follow(&self, conn: Connection, wake: impl Fn() + Send + 'static) {
        let me = self.clone();
        let started = std::thread::Builder::new()
            .name("portal".into())
            .spawn(move || {
                // Subscribe first, so a change between the read and the subscription
                // is not lost.
                let changes = changes_rule()
                    .and_then(|rule| MessageIterator::for_match_rule(rule, &conn, Some(16)));
                me.set(read(&conn));
                wake();
                let Ok(changes) = changes else {
                    log::info!("the desktop portal sends no setting changes");
                    return;
                };
                let mut owner = portal_owner(&conn);
                for msg in changes.flatten() {
                    // Only the portal's own: the owner is looked up again when a signal
                    // comes from someone else (the portal may have started again).
                    let header = msg.header();
                    let from = header.sender().map(|s| s.as_str().to_owned());
                    if from != owner {
                        owner = portal_owner(&conn);
                        if from != owner {
                            continue;
                        }
                    }
                    if let Ok((ns, key, value)) =
                        msg.body().deserialize::<(String, String, OwnedValue)>()
                        && ns == NAMESPACE
                        && key == KEY
                    {
                        me.set(scheme(&value));
                        wake();
                    }
                }
            });
        if let Err(e) = started {
            log::warn!("no thread for the desktop portal: {e}");
        }
    }
}

fn changes_rule() -> zbus::Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(IFACE)?
        .member("SettingChanged")?
        .path(PATH)?
        .arg(0, NAMESPACE)?
        .build())
}

fn portal_owner(conn: &Connection) -> Option<String> {
    let bus = zbus::blocking::fdo::DBusProxy::new(conn).ok()?;
    let owner = bus.get_name_owner(DEST.try_into().ok()?).ok()?;
    Some(owner.as_str().to_owned())
}

fn read(conn: &Connection) -> Option<Scheme> {
    let proxy = zbus::blocking::Proxy::new(conn, DEST, PATH, IFACE).ok()?;
    // ReadOne is version 2 of the interface; older portals have only Read, which
    // wraps the value in one more variant.
    let value: OwnedValue = proxy
        .call("ReadOne", &(NAMESPACE, KEY))
        .or_else(|_| proxy.call("Read", &(NAMESPACE, KEY)))
        .ok()?;
    scheme(&value)
}

/// 1 dark, 2 light, 0 (or anything else) no preference.
fn scheme(v: &Value<'_>) -> Option<Scheme> {
    match v {
        Value::U32(1) => Some(Scheme::Dark),
        Value::U32(2) => Some(Scheme::Light),
        Value::Value(inner) => scheme(inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_value_and_its_older_wrapping() {
        assert_eq!(scheme(&Value::U32(1)), Some(Scheme::Dark));
        assert_eq!(scheme(&Value::U32(2)), Some(Scheme::Light));
        assert_eq!(scheme(&Value::U32(0)), None);
        assert_eq!(
            scheme(&Value::Value(Box::new(Value::U32(2)))),
            Some(Scheme::Light)
        );
        assert_eq!(scheme(&Value::Str("dark".into())), None);
    }
}
