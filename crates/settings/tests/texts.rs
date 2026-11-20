//! The window's texts: every language has every message, no text uses a long dash,
//! and every message the code names exists (and every message is named somewhere).

use std::collections::BTreeSet;

const EN: &str = include_str!("../lang/en.ftl");
const UK: &str = include_str!("../lang/uk.ftl");

/// The code that names messages.
const SOURCES: &[(&str, &str)] = &[
    ("app.rs", include_str!("../src/app.rs")),
    ("pages.rs", include_str!("../src/pages.rs")),
    ("map.rs", include_str!("../src/map.rs")),
    ("widgets.rs", include_str!("../src/widgets.rs")),
];

/// All the code, for the dash check.
const ALL_CODE: &[(&str, &str)] = &[
    ("api.rs", include_str!("../src/api.rs")),
    ("dbus.rs", include_str!("../src/dbus.rs")),
    ("fonts.rs", include_str!("../src/fonts.rs")),
    ("instance.rs", include_str!("../src/instance.rs")),
    ("launch.rs", include_str!("../src/launch.rs")),
    ("lib.rs", include_str!("../src/lib.rs")),
    ("link.rs", include_str!("../src/link.rs")),
    ("main.rs", include_str!("../src/main.rs")),
    ("mock.rs", include_str!("../src/mock.rs")),
    ("portal.rs", include_str!("../src/portal.rs")),
    ("snap.rs", include_str!("../src/snap.rs")),
    ("theme.rs", include_str!("../src/theme.rs")),
];

/// Quoted strings in the code that look like ids but are not messages.
const NOT_MESSAGES: &[&str] = &["xdg-open"];

#[test]
fn every_language_has_every_message() {
    for (name, file) in [("en", EN), ("uk", UK)] {
        let r = prostir_lang::check(EN, file);
        assert!(r.is_clean(), "{name}.ftl: {r:?}");
    }
}

#[test]
fn no_long_dashes_anywhere() {
    let files = [("en.ftl", EN), ("uk.ftl", UK)];
    let themes = [
        ("ink.toml", prostir_settings::theme::INK),
        ("graphite.toml", prostir_settings::theme::GRAPHITE),
    ];
    for (name, text) in files.iter().chain(&themes).chain(SOURCES).chain(ALL_CODE) {
        for (n, line) in text.lines().enumerate() {
            assert!(
                !line.contains('\u{2014}') && !line.contains('\u{2013}'),
                "{name}:{}: a long dash: {line}",
                n + 1
            );
        }
    }
}

/// Every `"word-word"` literal in the code, and every `t("word")` and `f("word", ...)`.
fn named_ids() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (_, src) in SOURCES {
        let pieces: Vec<&str> = src.split('"').collect();
        for (i, piece) in pieces.iter().enumerate() {
            let called =
                i > 0 && (pieces[i - 1].ends_with(".t(") || pieces[i - 1].ends_with(".f("));
            let is_id = i % 2 == 1
                && (piece.contains('-') || called)
                && !piece.is_empty()
                && piece
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && piece.starts_with(|c: char| c.is_ascii_lowercase());
            if is_id && !NOT_MESSAGES.contains(piece) {
                out.insert((*piece).to_owned());
            }
        }
    }
    out
}

#[test]
fn the_code_and_the_messages_match() {
    let (messages, errors) = prostir_lang::messages(EN);
    assert!(errors.is_empty(), "{errors:?}");
    let named = named_ids();
    let missing: Vec<_> = named
        .iter()
        .filter(|id| !messages.contains_key(*id))
        .collect();
    assert!(
        missing.is_empty(),
        "named in the code, not in en.ftl: {missing:?}"
    );
    let unused: Vec<_> = messages
        .keys()
        .filter(|id| !named.contains(*id) && *id != prostir_lang::NAME_ID)
        .collect();
    assert!(unused.is_empty(), "in en.ftl, never shown: {unused:?}");
}

#[test]
fn placeholders_are_the_same_in_every_language() {
    let vars = |text: &str| -> BTreeSet<String> {
        text.split("{ $")
            .skip(1)
            .filter_map(|s| s.split([' ', '}']).next())
            .map(str::to_owned)
            .collect()
    };
    let per_message = |file: &str| -> Vec<(String, BTreeSet<String>)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for line in file.lines() {
            match line.split_once(" = ").or_else(|| line.split_once(" =")) {
                Some((id, rest)) if !line.starts_with([' ', '#']) => {
                    out.push((id.trim().to_owned(), rest.to_owned()));
                }
                _ => {
                    if let Some(last) = out.last_mut() {
                        last.1.push_str(line);
                    }
                }
            }
        }
        out.into_iter().map(|(id, t)| (id, vars(&t))).collect()
    };
    let en: std::collections::BTreeMap<_, _> = per_message(EN).into_iter().collect();
    for (id, v) in per_message(UK) {
        assert_eq!(Some(&v), en.get(&id), "uk.ftl {id}: placeholders differ");
    }
}
