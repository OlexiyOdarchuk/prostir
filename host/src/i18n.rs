//! Interface language of the PC side, from the desktop's locale: Ukrainian for `uk*`,
//! English otherwise. Strings are written as `t("український", "English")` pairs.

use std::sync::OnceLock;

fn ukrainian() -> bool {
    static UK: OnceLock<bool> = OnceLock::new();
    *UK.get_or_init(|| {
        // The first set variable wins, as in gettext.
        ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
            .is_some_and(|s| s.starts_with("uk"))
    })
}

pub fn t<'a>(uk: &'a str, en: &'a str) -> &'a str {
    if ukrainian() { uk } else { en }
}

/// `uk` or `en`, sent to the headset.
pub fn code() -> &'static str {
    t("uk", "en")
}
