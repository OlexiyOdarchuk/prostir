//! Interface language. Strings live next to the code that shows them, as
//! `lang.t("український", "English")` pairs.

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// The language in use, for code far from the menu (network thread, errors).
pub fn current() -> Lang {
    if ENGLISH.load(Ordering::Relaxed) {
        Lang::En
    } else {
        Lang::Uk
    }
}

pub fn set_current(lang: Lang) {
    ENGLISH.store(lang == Lang::En, Ordering::Relaxed);
}

/// The headset's system language (Quest settings), e.g. `en-US`.
pub fn system() -> Option<Lang> {
    let out = std::process::Command::new("getprop")
        .arg("persist.sys.locale")
        .output()
        .ok()?;
    Lang::from_code(&String::from_utf8_lossy(&out.stdout))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Uk,
    En,
}

impl Lang {
    /// From a language code such as `uk`, `uk_UA.UTF-8` or `en-US`.
    pub fn from_code(code: &str) -> Option<Self> {
        let code = code.trim().to_ascii_lowercase();
        if code.is_empty() {
            None
        } else if code.starts_with("uk") {
            Some(Lang::Uk)
        } else {
            Some(Lang::En)
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::Uk => "uk",
            Lang::En => "en",
        }
    }

    pub fn t<'a>(self, uk: &'a str, en: &'a str) -> &'a str {
        match self {
            Lang::Uk => uk,
            Lang::En => en,
        }
    }
}

/// The language chosen in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    /// The connected PC's language (the last one seen, before connecting).
    #[default]
    LikePc,
    Fixed(Lang),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(Lang::from_code("uk_UA.UTF-8"), Some(Lang::Uk));
        assert_eq!(Lang::from_code("en-US"), Some(Lang::En));
        assert_eq!(Lang::from_code("de"), Some(Lang::En));
        assert_eq!(Lang::from_code(""), None);
    }
}
