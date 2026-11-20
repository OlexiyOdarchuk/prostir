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

    /// A word's form for a count of `n`, by the plural rules Fluent follows (CLDR):
    /// Ukrainian `uk` is one (1, 21, 101), few (2 to 4, 22 to 24) and many (0, 5 to
    /// 20, 25); English `en` is one and other. "1 монітор", "2 монітори", "5 моніторів".
    pub fn count<'a>(self, n: u64, uk: [&'a str; 3], en: [&'a str; 2]) -> &'a str {
        match self {
            Lang::Uk => match (n % 10, n % 100) {
                (1, r) if r != 11 => uk[0],
                (2..=4, r) if !(12..=14).contains(&r) => uk[1],
                _ => uk[2],
            },
            Lang::En if n == 1 => en[0],
            Lang::En => en[1],
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

    /// The forms for counts are the ones Fluent picks, for every count up to a
    /// thousand, in both languages.
    #[test]
    fn counts_take_the_forms_fluent_picks() {
        const UK: &str = "n = { $n ->\n [one] 0\n [few] 1\n *[many] 2\n}\n";
        const EN: &str = "n = { $n ->\n [one] 0\n *[other] 1\n}\n";
        let catalog = prostir_lang::Catalog::with_dirs(&[("en", EN), ("uk", UK)], Vec::new());
        for (lang, code) in [(Lang::Uk, "uk"), (Lang::En, "en")] {
            let fluent = catalog.load(code);
            for n in 0..1000u64 {
                let ours = lang.count(n, ["0", "1", "2"], ["0", "1"]);
                assert_eq!(ours, fluent.f("n", &[("n", n.into())]), "{code} {n}");
            }
        }
        assert_eq!(
            [1, 2, 5, 11, 21, 22, 112].map(|n| Lang::Uk.count(
                n,
                ["екран", "екрани", "екранів"],
                ["", ""]
            )),
            [
                "екран",
                "екрани",
                "екранів",
                "екранів",
                "екран",
                "екрани",
                "екранів"
            ]
        );
    }
}
