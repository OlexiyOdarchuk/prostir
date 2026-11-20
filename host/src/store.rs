//! What the PC keeps between runs, with one owner: the settings (`settings.txt`), the
//! set of screens with the rows the headset shows them in (`screens.txt`) and when
//! each paired headset was last seen (`headsets.txt`), all plain `key value` text in
//! `~/.config/prostir/`.
//!
//! - What is kept is what the person wants, not what came out of it: the codec chosen
//!   stays chosen when the graphics card made another one.
//! - Command-line options hold for this run only and are never written (see [`Run`]);
//!   a change made while running is.
//! - Lines this version does not know are kept and written back as they were, so a
//!   newer version run before loses nothing.
//! - A change only marks the file; [`Store::flush`], once a pass of the main loop,
//!   hands the text to a thread of its own that writes it (the latest of each file)
//!   atomically and durably. A crash loses at most that one pass; a half-written file
//!   is a `*.tmp` beside the old one, removed on the next start.
//! - The first start of this version moves the settings of 0.1 over from
//!   `monitors.txt` (and the counter switch from `count`), which stay as they are for
//!   a downgrade to read.
//! - Screens are numbered from a counter that never goes back ([`Store::new_id`]):
//!   before the first number of each block of [`ID_BLOCK`] is given out, the end of the
//!   block is written and on disk, so a crash can leave numbers unused but never give
//!   one out twice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use log::{info, warn};
use prostir_layout::desk::Entry;
use prostir_proto::Codec;
use prostir_proto::state::{Access, perms, role};

use crate::displays::MONITORS_NO_LIMITS;
use crate::platform::MonitorSpec;
use crate::screen::ScreenId;

/// The file format this version writes.
const VERSION: u32 = 2;
/// Numbers of screens are reserved this many at a time (see [`Store::new_id`]).
pub const ID_BLOCK: u32 = 16;
/// The first number the counter gives: above every number earlier builds gave out
/// (monitors from 0, window panels 10 to 12, the laptop screen 100), so a new screen
/// never takes a place the headset kept for an old one.
const FIRST_ID: u32 = 101;
/// How long quitting waits for the files to be written.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// The PC's settings, as the person chose them.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Mbit/s per monitor.
    pub bitrate: u32,
    /// Clearer-text encoder preset.
    pub sharp: bool,
    pub codec: Codec,
    /// Encode on the CPU.
    pub software: bool,
    /// PC sound to the headset.
    pub audio: bool,
    /// The laptop screen in the headset.
    pub mirror: bool,
    /// Count this PC among users (see `usage`).
    pub count: bool,
    /// Starting with the session, as the person chose it (`autostart on|off`); none
    /// yet before the first start, which turns it on (see `platform::autostart`).
    /// Once kept, an update never changes it.
    pub autostart: Option<bool>,
    /// The settings window's look (`window-theme`, `window-palette`,
    /// `window-language`), written only when not the default.
    pub window: prostir_api::WindowLook,
    /// Each headset's role and rights, by its key (hex): `access KEY main input mic`.
    /// A headset without a line is the main one, with every right.
    pub access: BTreeMap<String, Access>,
}

impl Settings {
    /// The role and rights of the headset with key `key`.
    pub fn access_of(&self, key: &[u8; 32]) -> Access {
        self.access.get(&hex(key)).copied().unwrap_or(Access::MAIN)
    }
}

/// A key as `access` lines name it.
fn hex(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

/// The words of rights in `access` lines.
const PERMS: [(&str, u32); 6] = [
    ("input", perms::INPUT),
    ("mic", perms::MIC),
    ("camera", perms::CAMERA),
    ("clipboard", perms::CLIPBOARD),
    ("changes", perms::CHANGES),
    ("private", perms::PRIVATE),
];

/// The words of the rights in `perms`, as `access` lines write them.
pub fn perm_words(perms: u32) -> Vec<String> {
    PERMS
        .iter()
        .filter(|(_, bit)| perms & bit != 0)
        .map(|(word, _)| (*word).to_owned())
        .collect()
}

/// No right at all: a headset whose `access` line has a word that is no role or right.
const NO_RIGHTS: Access = Access {
    role: role::GUEST,
    perms: 0,
};

/// `KEY main input mic …`: a headset's role and rights, and whether the line was
/// mistyped. A line with a key and a word that is no role or right (a typo) makes that
/// headset a guest with no rights, never one with every right as a headset without a
/// line is; `None` for a line without a key.
fn parse_access(value: &str) -> Option<(String, Access, bool)> {
    let mut words = value.split_whitespace();
    let key = words.next()?;
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let key = key.to_ascii_lowercase();
    let role = match words.next() {
        Some("main") => role::MAIN,
        Some("guest") => role::GUEST,
        _ => return Some((key, NO_RIGHTS, true)),
    };
    let mut perms = 0;
    for w in words {
        match PERMS.iter().find(|(name, _)| *name == w) {
            Some((_, bit)) => perms |= bit,
            None => return Some((key, NO_RIGHTS, true)),
        }
    }
    Some((key, Access { role, perms }, false))
}

fn access_text(key: &str, a: &Access) -> String {
    let role = if a.is_main() { "main" } else { "guest" };
    let mut text = format!("access {key} {role}");
    for (name, bit) in PERMS {
        if a.may(bit) {
            text.push(' ');
            text.push_str(name);
        }
    }
    text
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bitrate: 30,
            sharp: false,
            codec: Codec::H265,
            software: false,
            audio: true,
            mirror: true,
            count: true,
            autostart: None,
            window: prostir_api::WindowLook::default(),
            access: BTreeMap::new(),
        }
    }
}

/// How a display of the PC is told from others: by what it says it is (its EDID:
/// maker, model, serial number), or where that says nothing or is not unique, by the
/// connector it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalKey {
    pub edid: Option<String>,
    pub connector: String,
}

/// A display of the PC itself, as kept: how it is known, whether it is the laptop's
/// own panel, and whether the headset shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Physical {
    pub key: PhysicalKey,
    pub builtin: bool,
    pub shown: bool,
    /// When it was last connected, in seconds since 1970 (0: not known): of those
    /// not connected, the least recently seen are forgotten first.
    pub seen: u64,
}

/// The set of virtual monitors, the PC's own displays it knows, and how the headset
/// shows them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SavedScreens {
    pub specs: BTreeMap<ScreenId, MonitorSpec>,
    /// The PC's own displays seen so far, connected now or not.
    pub physical: BTreeMap<ScreenId, Physical>,
    /// Rows from top to bottom, left to right, with where each is seen; the PC's own
    /// displays too, where the headset put them.
    pub rows: Vec<Vec<Entry<ScreenId>>>,
    pub primary: Option<ScreenId>,
}

impl SavedScreens {
    /// One 2560x1440 monitor, for a first start.
    pub fn first() -> Self {
        let id = ScreenId::saved(0);
        Self {
            specs: BTreeMap::from([(id, "2560x1440".parse().expect("a valid size"))]),
            rows: vec![vec![Entry::new(id)]],
            ..Self::default()
        }
    }

    /// `specs` side by side in one row, numbered from 0, where nothing is known.
    pub fn of(specs: &[MonitorSpec]) -> Self {
        let ids: Vec<ScreenId> = (0..specs.len() as u32).map(ScreenId::saved).collect();
        Self {
            specs: ids.iter().copied().zip(specs.iter().cloned()).collect(),
            rows: vec![ids.iter().map(|&id| Entry::new(id)).collect()],
            ..Self::default()
        }
    }

    /// The highest number among the screens and in the rows.
    fn highest(&self) -> Option<u32> {
        let rows = self.rows.iter().flatten().map(|e| e.id);
        self.specs
            .keys()
            .chain(self.physical.keys())
            .copied()
            .chain(rows)
            .map(ScreenId::number)
            .max()
    }
}

/// What holds for this run only, from the command line: never written.
#[derive(Debug, Clone, Default)]
pub struct Run {
    pub bitrate: Option<u32>,
    pub codec: Option<Codec>,
    pub software: Option<bool>,
    /// Monitors from `-m`: the set until the person changes it in the headset.
    pub screens: Option<SavedScreens>,
}

/// The monitors to start with: those from the command line if any (for this run),
/// else the saved ones, else one.
pub fn startup_set(saved: &SavedScreens, args: &[MonitorSpec]) -> Option<SavedScreens> {
    if !args.is_empty() {
        let mut set = SavedScreens::of(&args[..args.len().min(MONITORS_NO_LIMITS)]);
        // The PC's own displays stay as they were, where they were seen.
        set.physical = saved.physical.clone();
        let places: Vec<Entry<ScreenId>> = saved
            .rows
            .iter()
            .flatten()
            .filter(|e| !saved.specs.contains_key(&e.id))
            .copied()
            .collect();
        if !places.is_empty() {
            set.rows.push(places);
        }
        Some(set)
    } else if saved.specs.is_empty() {
        Some(SavedScreens::first())
    } else {
        None
    }
}

/// A file's version and the lines this version does not know.
#[derive(Debug, Clone, Default, PartialEq)]
struct Kept {
    version: u32,
    unknown: Vec<String>,
    /// Headsets with a mistyped `access` line: their line stays as it was, among the
    /// unknown ones, for the user to put right, and they have no rights meanwhile,
    /// whatever other line they have.
    mistyped: std::collections::BTreeSet<String>,
    /// The well-formed `access` lines of those headsets, written back as they were.
    beside_mistyped: BTreeMap<String, Access>,
}

impl Kept {
    /// The head of the file: the version (never lower than one that wrote it) and the
    /// unknown lines.
    fn head(&self) -> String {
        let mut text = format!("version {}\n", self.version.max(VERSION));
        for line in &self.unknown {
            text.push_str(line);
            text.push('\n');
        }
        text
    }
}

fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::H265 => "h265",
        Codec::Av1 => "av1",
    }
}

/// A palette's or a language's name: one short word of letters, digits and `-`.
pub fn is_word(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn parse_settings(text: &str) -> (Settings, Kept) {
    let mut s = Settings::default();
    let mut kept = Kept::default();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let value = value.trim();
        let flag = match value {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        };
        let known = match (key, flag) {
            ("version", _) => value.parse().map(|v| kept.version = v).is_ok(),
            ("bitrate", _) => value.parse().map(|v| s.bitrate = v).is_ok(),
            ("codec", _) => match value {
                "h264" => Some(Codec::H264),
                "h265" => Some(Codec::H265),
                "av1" => Some(Codec::Av1),
                _ => None,
            }
            .map(|c| s.codec = c)
            .is_some(),
            ("sharp", Some(v)) => {
                s.sharp = v;
                true
            }
            ("software", Some(v)) => {
                s.software = v;
                true
            }
            ("audio", Some(v)) => {
                s.audio = v;
                true
            }
            ("mirror", Some(v)) => {
                s.mirror = v;
                true
            }
            ("count", Some(v)) => {
                s.count = v;
                true
            }
            ("autostart", Some(v)) => {
                s.autostart = Some(v);
                true
            }
            ("window-theme", _) => match value {
                "system" => Some(prostir_api::ThemeMode::System),
                "light" => Some(prostir_api::ThemeMode::Light),
                "dark" => Some(prostir_api::ThemeMode::Dark),
                _ => None,
            }
            .map(|theme| s.window.theme = theme)
            .is_some(),
            ("window-palette", _) if is_word(value) => {
                s.window.palette = value.to_owned();
                true
            }
            ("window-language", _) if is_word(value) => {
                s.window.language = Some(value.to_owned());
                true
            }
            ("access", _) => match parse_access(value) {
                // A mistyped line for the same headset wins, before or after.
                Some((key, a, false)) if kept.mistyped.contains(&key) => {
                    kept.beside_mistyped.insert(key, a);
                    true
                }
                Some((key, a, false)) => {
                    s.access.insert(key, a);
                    true
                }
                Some((key, a, true)) => {
                    warn!(
                        "settings.txt: \"{line}\" has a word that is no role or right: \
                         that headset gets no rights until it is put right"
                    );
                    if let Some(good) = s.access.insert(key.clone(), a)
                        && !kept.mistyped.contains(&key)
                    {
                        kept.beside_mistyped.insert(key.clone(), good);
                    }
                    kept.mistyped.insert(key);
                    false
                }
                None => false,
            },
            _ => false,
        };
        if !known {
            kept.unknown.push(line.to_owned());
        }
    }
    (s, kept)
}

fn settings_text(s: &Settings, kept: &Kept) -> String {
    let mut text = format!(
        "{}bitrate {}\nsharp {}\ncodec {}\nsoftware {}\naudio {}\nmirror {}\ncount {}\n",
        kept.head(),
        s.bitrate,
        on_off(s.sharp),
        codec_name(s.codec),
        on_off(s.software),
        on_off(s.audio),
        on_off(s.mirror),
        on_off(s.count),
    );
    if let Some(on) = s.autostart {
        text.push_str(&format!("autostart {}\n", on_off(on)));
    }
    let window = prostir_api::WindowLook::default();
    if s.window.theme != window.theme {
        text.push_str(&format!("window-theme {}\n", s.window.theme.name()));
    }
    if s.window.palette != window.palette {
        text.push_str(&format!("window-palette {}\n", s.window.palette));
    }
    if let Some(language) = &s.window.language {
        text.push_str(&format!("window-language {language}\n"));
    }
    for (key, a) in &s.access {
        // A headset with a mistyped line keeps its other line as it was.
        let a = if kept.mistyped.contains(key) {
            kept.beside_mistyped.get(key)
        } else {
            Some(a)
        };
        if let Some(a) = a {
            text.push_str(&access_text(key, a));
            text.push('\n');
        }
    }
    text
}

/// `at CENTER MIDDLE`, either `next` (unknown centre) or missing (unknown middle).
fn parse_seen<'a>(mut words: impl Iterator<Item = &'a str>) -> (Option<i32>, Option<i32>) {
    let center = words.next().and_then(|w| w.parse().ok());
    let middle = words.next().and_then(|w| w.parse().ok());
    (center, middle)
}

fn seen_text(e: &Entry<ScreenId>) -> String {
    if e.center.is_none() && e.middle.is_none() {
        return String::new();
    }
    let center = e.center.map_or("next".to_owned(), |c| c.to_string());
    match e.middle {
        Some(m) => format!(" at {center} {m}"),
        None => format!(" at {center}"),
    }
}

/// The laptop screen's number in version 1 of `screens.txt` (its `mirror` lines).
pub const LEGACY_LAPTOP: u32 = 100;

/// `ID [edid "TEXT"] connector NAME [builtin] [shown] [seen SECONDS]`: a display of
/// the PC itself.
fn parse_physical(rest: &str) -> Option<(ScreenId, Physical)> {
    let (id, rest) = rest.split_once(' ')?;
    let id = ScreenId::saved(id.parse().ok()?);
    let (edid, rest) = match rest.strip_prefix("edid \"") {
        Some(r) => {
            let (edid, r) = r.split_once('"')?;
            (Some(edid.to_owned()), r.trim_start())
        }
        None => (None, rest),
    };
    let mut words = rest.split_whitespace();
    if words.next()? != "connector" {
        return None;
    }
    let connector = words.next()?.to_owned();
    let (mut builtin, mut shown, mut seen) = (false, false, 0);
    while let Some(w) = words.next() {
        match w {
            "builtin" => builtin = true,
            "shown" => shown = true,
            "seen" => seen = words.next()?.parse().ok()?,
            _ => return None,
        }
    }
    Some((
        id,
        Physical {
            key: PhysicalKey { edid, connector },
            builtin,
            shown,
            seen,
        },
    ))
}

fn physical_text(id: ScreenId, p: &Physical) -> String {
    let mut text = format!("physical {}", id.number());
    if let Some(edid) = &p.key.edid {
        text.push_str(&format!(" edid \"{}\"", edid.replace('"', "'")));
    }
    text.push_str(&format!(" connector {}", p.key.connector));
    if p.builtin {
        text.push_str(" builtin");
    }
    if p.shown {
        text.push_str(" shown");
    }
    if p.seen > 0 {
        text.push_str(&format!(" seen {}", p.seen));
    }
    text.push('\n');
    text
}

/// The file `screens.txt`: `next N` (the counter, see [`Store::new_id`]), `physical …`
/// for each display of the PC itself, then `row` and in each row `screen N WxH[@scale]
/// [at C M] [primary]` for a monitor and `place N [at C M] [primary]` for a display of
/// the PC (version 1 had `mirror [at C M]` for the laptop screen, number 100). The
/// counter read, if there is one.
fn parse_screens(text: &str) -> (SavedScreens, Kept, Option<u32>) {
    let mut set = SavedScreens::default();
    let mut kept = Kept::default();
    let mut next = None;
    let place =
        |set: &mut SavedScreens, id: ScreenId, seen: (Option<i32>, Option<i32>), primary: bool| {
            if set.rows.iter().flatten().any(|e| e.id == id) {
                return false;
            }
            if primary {
                set.primary = Some(id);
            }
            if set.rows.is_empty() {
                set.rows.push(Vec::new());
            }
            if let Some(row) = set.rows.last_mut() {
                row.push(Entry {
                    id,
                    center: seen.0,
                    middle: seen.1,
                });
            }
            true
        };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut words = line.split_whitespace();
        let known = match words.next() {
            Some("version") => words
                .next()
                .and_then(|v| v.parse().ok())
                .map(|v| kept.version = v)
                .is_some(),
            Some("next") => words
                .next()
                .and_then(|v| v.parse().ok())
                .map(|v| next = Some(v))
                .is_some(),
            Some("physical") => match line.strip_prefix("physical ").and_then(parse_physical) {
                Some((id, p))
                    if !set.physical.contains_key(&id) && !set.specs.contains_key(&id) =>
                {
                    set.physical.insert(id, p);
                    true
                }
                _ => false,
            },
            Some("row") => {
                set.rows.push(Vec::new());
                true
            }
            Some("screen") => (|| {
                let id = ScreenId::saved(words.next()?.parse().ok()?);
                let spec: MonitorSpec = words.next()?.parse().ok()?;
                let rest: Vec<&str> = words.collect();
                let primary = rest.last() == Some(&"primary");
                let seen = match rest.first() {
                    Some(&"at") => parse_seen(rest[1..].iter().copied()),
                    _ => (None, None),
                };
                if set.specs.contains_key(&id) || set.physical.contains_key(&id) {
                    return None;
                }
                place(&mut set, id, seen, primary).then(|| set.specs.insert(id, spec))
            })()
            .is_some(),
            Some(word @ ("place" | "mirror")) => (|| {
                let id = if word == "mirror" {
                    ScreenId::saved(LEGACY_LAPTOP)
                } else {
                    ScreenId::saved(words.next()?.parse().ok()?)
                };
                let rest: Vec<&str> = words.collect();
                let primary = rest.last() == Some(&"primary");
                let seen = match rest.first() {
                    Some(&"at") => parse_seen(rest[1..].iter().copied()),
                    _ => (None, None),
                };
                place(&mut set, id, seen, primary).then_some(())
            })()
            .is_some(),
            _ => false,
        };
        if !known {
            kept.unknown.push(line.to_owned());
        }
    }
    set.rows.retain(|r| !r.is_empty());
    (set, kept, next)
}

fn screens_text(set: &SavedScreens, kept: &Kept, next: u32) -> String {
    let mut text = kept.head();
    text.push_str(&format!("next {next}\n"));
    for (id, p) in &set.physical {
        text.push_str(&physical_text(*id, p));
    }
    for row in &set.rows {
        text.push_str("row\n");
        for e in row {
            let primary = if set.primary == Some(e.id) {
                " primary"
            } else {
                ""
            };
            match set.specs.get(&e.id) {
                Some(spec) => text.push_str(&format!(
                    "screen {} {spec}{}{primary}\n",
                    e.id.number(),
                    seen_text(e)
                )),
                None => text.push_str(&format!(
                    "place {}{}{primary}\n",
                    e.id.number(),
                    seen_text(e)
                )),
            }
        }
    }
    text
}

/// What 0.1 saved in `monitors.txt`: `row` starts a row, then one
/// `WIDTHxHEIGHT[@scale][ at CENTER[ MIDDLE]][ *]` per monitor (`next` for an unknown
/// centre, `*` for the primary one), and the choices as `key value`. Files from
/// before rows have no `row`; from before 0.1.1, no middle. Monitors are numbered in
/// the order of the file, as 0.1 numbered them, so the headset finds them where it
/// saw them last.
fn migrate(text: &str) -> (Settings, SavedScreens) {
    let (settings, _) = parse_settings(text);
    let mut set = SavedScreens::default();
    let mut rows: Vec<Vec<Entry<ScreenId>>> = Vec::new();
    let mut loose: Vec<Entry<ScreenId>> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line == "row" || line.starts_with("row ") {
            rows.push(Vec::new());
            continue;
        }
        let (line, star) = match line.strip_suffix('*') {
            Some(s) => (s.trim(), true),
            None => (line, false),
        };
        let (spec, seen) = match line.split_once(" at ") {
            Some((spec, at)) => (spec.trim(), parse_seen(at.split_whitespace())),
            None => (line, (None, None)),
        };
        let Ok(spec) = spec.parse::<MonitorSpec>() else {
            continue;
        };
        let id = ScreenId::saved(set.specs.len() as u32);
        set.specs.insert(id, spec);
        if star {
            set.primary = Some(id);
        }
        let entry = Entry {
            id,
            center: seen.0,
            middle: seen.1,
        };
        match rows.last_mut() {
            Some(row) => row.push(entry),
            None => loose.push(entry),
        }
    }

    rows.retain(|r| !r.is_empty());
    if !loose.is_empty() {
        rows.push(loose);
    }
    set.rows = rows;
    (settings, set)
}

/// When a paired headset last had a session, and its app then.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Seen {
    /// Seconds since 1970.
    pub at: u64,
    /// The headset app's version (`0.2.0-dev+p6`), as its hello said.
    pub app: String,
}

/// The file `headsets.txt`: `seen KEY SECONDS [APP]`, a line for each headset, KEY its
/// whole key in hex. Lines it cannot read are left out.
fn parse_seen_headsets(text: &str) -> BTreeMap<String, Seen> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            if words.next()? != "seen" {
                return None;
            }
            let key = words.next()?;
            if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let at = words.next()?.parse().ok()?;
            let app = words.next().unwrap_or_default().to_owned();
            Some((key.to_ascii_lowercase(), Seen { at, app }))
        })
        .collect()
}

fn seen_headsets_text(seen: &BTreeMap<String, Seen>) -> String {
    seen.iter()
        .map(|(key, s)| {
            let app: String = s.app.chars().filter(|c| !c.is_whitespace()).collect();
            if app.is_empty() {
                format!("seen {key} {}\n", s.at)
            } else {
                format!("seen {key} {} {app}\n", s.at)
            }
        })
        .collect()
}

/// The writes waiting for the writer thread, the latest of each file.
enum Job {
    Write(PathBuf, String),
    /// Says when everything before it is on disk.
    Sync(Sender<()>),
    Stop,
}

/// Writes `text` to `path` so that it survives a crash or a power cut: a whole new
/// file or the old one, never half of either.
fn write_durably(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    prostir_proto::secure::write_atomic(path, text.as_bytes())?;
    // The rename itself is kept only once the directory is on disk.
    std::fs::File::open(dir)?.sync_all()
}

fn spawn_writer() -> Option<(Sender<Job>, Receiver<()>)> {
    let (tx, jobs) = mpsc::channel::<Job>();
    let (done_tx, done) = mpsc::channel();
    std::thread::Builder::new()
        .name("store-writer".into())
        .spawn(move || {
            let mut stop = false;
            while !stop {
                let Ok(first) = jobs.recv() else { break };
                // The latest text of each file.
                let mut latest: BTreeMap<PathBuf, String> = BTreeMap::new();
                let mut synced = Vec::new();
                for job in std::iter::once(first).chain(jobs.try_iter()) {
                    match job {
                        Job::Write(path, text) => {
                            latest.insert(path, text);
                        }
                        Job::Sync(done) => synced.push(done),
                        Job::Stop => stop = true,
                    }
                }
                for (path, text) in latest {
                    if let Err(e) = write_durably(&path, &text) {
                        warn!("cannot save {}: {e}", path.display());
                    }
                }
                for done in synced {
                    let _ = done.send(());
                }
            }
            let _ = done_tx.send(());
        })
        .inspect_err(|e| warn!("no thread to save settings in: {e}"))
        .ok()?;
    Some((tx, done))
}

/// `~/.config/prostir`, where `questmon` was before the rename.
pub fn config_dir() -> Option<PathBuf> {
    let base = crate::platform::config_home()?;
    let dir = base.join("prostir");
    let old = base.join("questmon");
    if !dir.exists() && old.exists() {
        let _ = std::fs::rename(&old, &dir);
    }
    Some(dir)
}

/// Prostir never ran for this user (nothing of it in `dir`, `~/.config/prostir`): not
/// its settings, its screens, the settings of 0.1, its key or a paired headset.
pub fn first_start(dir: &Path) -> bool {
    [
        "settings.txt",
        "screens.txt",
        "monitors.txt",
        "identity",
        "paired_headsets",
    ]
    .iter()
    .all(|name| !dir.join(name).exists())
}

/// The settings and the set of screens, owned by the main loop.
pub struct Store {
    /// `~/.config/prostir`; none: nothing is saved (no home directory).
    dir: Option<PathBuf>,
    settings: Settings,
    settings_kept: Kept,
    screens: SavedScreens,
    screens_kept: Kept,
    /// The next number to give, and the end of the block reserved on disk.
    next: u32,
    reserved: u32,
    run: Run,
    settings_changed: bool,
    screens_changed: bool,
    /// When each paired headset last had a session (`headsets.txt`), by its key in
    /// hex.
    seen: BTreeMap<String, Seen>,
    seen_changed: bool,
    writer: Option<(Sender<Job>, Receiver<()>)>,
}

impl Store {
    /// The store of `dir` (moving the settings of 0.1 over), with what holds for this
    /// run only.
    pub fn open(dir: Option<PathBuf>, run: Run) -> Self {
        let mut store = Self {
            dir,
            settings: Settings::default(),
            settings_kept: Kept::default(),
            screens: SavedScreens::default(),
            screens_kept: Kept::default(),
            next: FIRST_ID,
            reserved: FIRST_ID,
            run,
            settings_changed: false,
            screens_changed: false,
            seen: BTreeMap::new(),
            seen_changed: false,
            writer: None,
        };
        store.load();
        store.writer = store.dir.as_ref().and_then(|_| spawn_writer());
        store
    }

    /// A number for a new screen, never given before: from the block reserved on disk,
    /// reserving the next block (and waiting until that is written) when this one is
    /// used up.
    pub fn new_id(&mut self) -> ScreenId {
        if self.next >= self.reserved {
            self.reserved = self.next + ID_BLOCK;
            self.screens_changed = true;
            self.flush_and_wait();
        }
        let id = ScreenId::saved(self.next);
        self.next += 1;
        id
    }

    /// Writes what changed and waits until it is on disk.
    fn flush_and_wait(&mut self) {
        self.flush();
        if let Some((tx, _)) = &self.writer {
            let (done_tx, done) = mpsc::channel();
            if tx.send(Job::Sync(done_tx)).is_ok() && done.recv_timeout(CLOSE_WAIT).is_err() {
                warn!(
                    "the screens' numbers not saved within {} s",
                    CLOSE_WAIT.as_secs()
                );
            }
        }
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(name))
    }

    fn load(&mut self) {
        let Some(dir) = self.dir.clone() else { return };
        // A write cut short leaves its temporary file; the real one is whole.
        for name in ["settings.txt.tmp", "screens.txt.tmp", "headsets.txt.tmp"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        if let Some(text) = read("headsets.txt") {
            self.seen = parse_seen_headsets(&text);
        }
        let (settings, screens) = (read("settings.txt"), read("screens.txt"));
        if settings.is_none() && screens.is_none() {
            // A first start: nothing to move over.
            let Some(old) = read("monitors.txt") else {
                return;
            };
            (self.settings, self.screens) = migrate(&old);
            // The counter switch had a file of its own.
            if read("count").is_some_and(|t| t.trim() == "off") {
                self.settings.count = false;
            }
            info!("settings moved over from monitors.txt");
            self.settings_changed = true;
            self.screens_changed = !self.screens.specs.is_empty();
            self.start_counter(None);
            return;
        }
        if let Some(text) = settings {
            (self.settings, self.settings_kept) = parse_settings(&text);
        }
        let mut next = None;
        if let Some(text) = screens {
            let (set, kept, counter) = parse_screens(&text);
            next = counter;
            if set.specs.is_empty() && counter.is_none() {
                // Nothing in it could be read: kept aside, so the next write does not
                // wipe out what went wrong.
                warn!("screens.txt has no monitor in it; kept as screens.txt.bad");
                let _ = std::fs::copy(dir.join("screens.txt"), dir.join("screens.txt.bad"));
            } else {
                (self.screens, self.screens_kept) = (set, kept);
            }
        }
        self.start_counter(next);
    }

    /// The counter goes on above every number saved and every number the file says may
    /// have been given out (`saved`), and never below [`FIRST_ID`].
    fn start_counter(&mut self, saved: Option<u32>) {
        let above = self.screens.highest().map_or(0, |n| n + 1);
        self.next = saved.unwrap_or(0).max(above).max(FIRST_ID);
        self.reserved = self.next;
    }

    /// The settings that hold now: as saved, but for what the command line says for
    /// this run.
    pub fn wanted(&self) -> Settings {
        Settings {
            bitrate: self.run.bitrate.unwrap_or(self.settings.bitrate),
            codec: self.run.codec.unwrap_or(self.settings.codec),
            software: self.run.software.unwrap_or(self.settings.software),
            ..self.settings.clone()
        }
    }

    /// Changes settings the command line has no say in.
    pub fn change(&mut self, change: impl FnOnce(&mut Settings)) {
        let before = self.settings.clone();
        change(&mut self.settings);
        self.settings_changed |= self.settings != before;
    }

    /// Video quality chosen while running: from now on it holds, over the command line.
    pub fn set_bitrate(&mut self, mbps: u32) {
        self.run.bitrate = None;
        self.change(|s| s.bitrate = mbps);
    }

    /// How to encode, chosen in the headset menu: it holds from now on, over the
    /// command line.
    pub fn set_encoding(&mut self, bitrate: u32, sharp: bool, codec: Codec, software: bool) {
        self.run.bitrate = None;
        self.run.codec = None;
        self.run.software = None;
        self.change(|s| {
            s.bitrate = bitrate;
            s.sharp = sharp;
            s.codec = codec;
            s.software = software;
        });
    }

    /// Starts this run with `monitors` from the command line, if any (see
    /// [`startup_set`]).
    pub fn start_with(&mut self, monitors: &[MonitorSpec]) {
        match startup_set(&self.screens, monitors) {
            Some(set) if !monitors.is_empty() => self.run.screens = Some(set),
            // The first one: kept once it changes.
            Some(set) => self.screens = set,
            None => {}
        }
    }

    /// The set of monitors this run: the saved one, or the one from the command line.
    pub fn screens(&self) -> &SavedScreens {
        self.run.screens.as_ref().unwrap_or(&self.screens)
    }

    /// The set as it is now. `chosen`: the person changed which monitors there are,
    /// so it is saved even when the run started with monitors from the command line;
    /// otherwise a change to a set from the command line stays in this run.
    pub fn set_screens(&mut self, set: SavedScreens, chosen: bool) {
        if chosen {
            self.run.screens = None;
        }
        match &mut self.run.screens {
            Some(run) => *run = set,
            None => {
                self.screens_changed |= self.screens != set;
                self.screens = set;
            }
        }
    }

    /// Headset `key` has a session now, or its session ended: seen now, with its app
    /// (`app`, from its hello; none keeps the one known).
    pub fn headset_seen(&mut self, key: &[u8; 32], at: u64, app: Option<&str>) {
        let seen = self.seen.entry(hex(key)).or_default();
        seen.at = at;
        if let Some(app) = app.filter(|a| !a.is_empty()) {
            app.clone_into(&mut seen.app);
        }
        self.seen_changed = true;
    }

    /// When headset `key` last had a session, and its app then, if known.
    pub fn last_seen(&self, key: &[u8; 32]) -> Option<&Seen> {
        self.seen.get(&hex(key))
    }

    /// Forgets when the headsets not in `paired` were seen (they were forgotten).
    pub fn keep_seen_of(&mut self, paired: &[[u8; 32]]) {
        let keep: Vec<String> = paired.iter().map(hex).collect();
        let before = self.seen.len();
        self.seen.retain(|key, _| keep.contains(key));
        self.seen_changed |= self.seen.len() != before;
    }

    /// Hands what changed to the writer thread.
    pub fn flush(&mut self) {
        let mut jobs = Vec::new();
        if std::mem::take(&mut self.seen_changed)
            && let Some(path) = self.path("headsets.txt")
        {
            jobs.push((path, seen_headsets_text(&self.seen)));
        }
        if std::mem::take(&mut self.settings_changed)
            && let Some(path) = self.path("settings.txt")
        {
            jobs.push((path, settings_text(&self.settings, &self.settings_kept)));
        }
        if std::mem::take(&mut self.screens_changed)
            && let Some(path) = self.path("screens.txt")
        {
            let text = screens_text(&self.screens, &self.screens_kept, self.reserved);
            jobs.push((path, text));
        }
        for (path, text) in jobs {
            match &self.writer {
                Some((tx, _)) => {
                    let _ = tx.send(Job::Write(path, text));
                }
                // Without the thread, here and now.
                None => {
                    if let Err(e) = write_durably(&path, &text) {
                        warn!("cannot save {}: {e}", path.display());
                    }
                }
            }
        }
    }

    /// Writes what is left and waits for it (at most [`CLOSE_WAIT`]).
    pub fn close(mut self) {
        self.flush();
        if let Some((tx, done)) = self.writer.take() {
            let _ = tx.send(Job::Stop);
            if done.recv_timeout(CLOSE_WAIT).is_err() {
                warn!("settings not saved within {} s", CLOSE_WAIT.as_secs());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &str) -> MonitorSpec {
        s.parse().unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prostir-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every monitors.txt 0.1 ever wrote moves over: sizes, scales, rows, where each was
    /// seen, the primary one, and the choices.
    #[test]
    fn every_old_monitors_txt_moves_over() {
        // Before rows were saved: one row.
        let (_, set) = migrate("2560x1440\n1920x1080@1.5\n");
        assert_eq!(set.specs.len(), 2);
        assert_eq!(set.specs[&ScreenId::saved(1)].scale, 1.5);
        assert_eq!(set.rows.len(), 1);
        // Before vertical positions (0.1.0).
        let (s, set) = migrate("row\n2560x1440 at -1300 *\n1920x1080\nmirror off\nbitrate 30\n");
        assert_eq!(set.primary, Some(ScreenId::saved(0)));
        assert_eq!(
            set.rows,
            vec![vec![
                Entry {
                    id: ScreenId::saved(0),
                    center: Some(-1300),
                    middle: None
                },
                Entry::new(ScreenId::saved(1)),
            ]]
        );
        assert!(!s.mirror && s.bitrate == 30);
        // 0.1.1 and 0.1.2: rows, middles, `next`, every choice.
        let (s, set) = migrate(
            "row\n2560x1440 at -1300 200\nrow\n1440x2560 at next -40 *\nmirror on\naudio off\n\
             mic off\nbitrate 40\nsharp on\ncodec h264\nsoftware on\n",
        );
        assert_eq!(set.rows.len(), 2);
        assert_eq!(set.rows[1][0].center, None);
        assert_eq!(set.rows[1][0].middle, Some(-40));
        assert_eq!(set.primary, Some(ScreenId::saved(1)));
        assert_eq!(
            s,
            Settings {
                bitrate: 40,
                sharp: true,
                codec: Codec::H264,
                software: true,
                audio: false,
                mirror: true,
                count: true,
                autostart: None,
                window: prostir_api::WindowLook::default(),
                access: BTreeMap::new(),
            }
        );
    }

    /// The settings window's look is kept, and only when it is not the default; a
    /// value that does not read stays as an unknown line.
    #[test]
    fn the_window_look_is_kept() {
        let (s, kept) = parse_settings("bitrate 30\n");
        assert!(!settings_text(&s, &kept).contains("window-"));
        let (s, kept) =
            parse_settings("window-theme dark\nwindow-palette graphite\nwindow-language uk\n");
        assert_eq!(
            s.window,
            prostir_api::WindowLook {
                theme: prostir_api::ThemeMode::Dark,
                palette: "graphite".into(),
                language: Some("uk".into()),
            }
        );
        let text = settings_text(&s, &kept);
        assert!(
            text.contains("window-theme dark\nwindow-palette graphite\nwindow-language uk\n"),
            "{text}"
        );
        assert_eq!(parse_settings(&text).0, s);
        let (s, kept) = parse_settings("window-theme purple\nwindow-palette a b\n");
        assert_eq!(s.window, prostir_api::WindowLook::default());
        assert_eq!(kept.unknown.len(), 2);
    }

    /// The choice to start with the session is kept once made, and written only then:
    /// none before the first start.
    #[test]
    fn the_autostart_choice_is_kept() {
        let (s, kept) = parse_settings("bitrate 30\n");
        assert_eq!(s.autostart, None);
        assert!(!settings_text(&s, &kept).contains("autostart"));
        for (line, on) in [("autostart off\n", false), ("autostart on\n", true)] {
            let (s, kept) = parse_settings(line);
            assert_eq!(s.autostart, Some(on));
            let text = settings_text(&s, &kept);
            assert!(text.contains(line), "{text}");
            assert_eq!(parse_settings(&text).0.autostart, Some(on));
        }
    }

    /// The six monitors of 2026-11-19, as 0.1.2 saved them.
    #[test]
    fn the_six_monitors_move_over() {
        let (s, set) = migrate(
            "row\n3440x1440 at -2872 -72\nrow\n2560x1440 at -1280 1368\n1440x2560 at 720 1280 *\n\
             3440x1440 at -2264 6768\n2560x1440 at -2000 5328\nrow\n3440x1440 at -1800 2808\n\
             mirror on\naudio on\nmic on\nbitrate 30\nsharp on\ncodec h265\nsoftware off\n",
        );
        assert_eq!(set.specs.len(), 6);
        let lens: Vec<usize> = set.rows.iter().map(Vec::len).collect();
        assert_eq!(lens, [1, 4, 1]);
        assert_eq!(set.primary, Some(ScreenId::saved(2)));
        assert!(s.sharp && s.mirror);
        // Read back from the new file as it was moved over.
        let text = screens_text(&set, &Kept::default(), FIRST_ID);
        assert_eq!(parse_screens(&text).0, set);
    }

    #[test]
    fn files_round_trip_and_keep_unknown_lines() {
        let settings = Settings {
            bitrate: 50,
            codec: Codec::H264,
            count: false,
            ..Settings::default()
        };
        let kept = Kept {
            version: 3,
            unknown: vec!["theme dark".into()],
            ..Kept::default()
        };
        let text = settings_text(&settings, &kept);
        assert_eq!(parse_settings(&text), (settings, kept.clone()));

        let mut set = SavedScreens::of(&[spec("2560x1440"), spec("1440x2560@1.25")]);
        set.rows[0][1].center = Some(720);
        set.rows[0][1].middle = Some(-40);
        let laptop = ScreenId::saved(100);
        let hdmi = ScreenId::saved(117);
        set.physical.insert(
            laptop,
            Physical {
                key: PhysicalKey {
                    edid: Some("BOE 0x0a1c 0".into()),
                    connector: "eDP-1".into(),
                },
                builtin: true,
                shown: true,
                seen: 1_795_000_000,
            },
        );
        set.physical.insert(
            hdmi,
            Physical {
                key: PhysicalKey {
                    edid: None,
                    connector: "HDMI-A-1".into(),
                },
                builtin: false,
                shown: false,
                seen: 0,
            },
        );
        set.rows.push(vec![Entry {
            id: laptop,
            center: None,
            middle: Some(2200),
        }]);
        set.primary = Some(ScreenId::saved(1));
        let text = screens_text(&set, &kept, 133);
        assert!(text.contains("next 133\n"), "{text}");
        assert!(text.contains("screen 1 1440x2560@1.25 at 720 -40 primary\n"));
        assert!(text.contains(
            "physical 100 edid \"BOE 0x0a1c 0\" connector eDP-1 builtin shown seen 1795000000\n"
        ));
        assert!(text.contains("physical 117 connector HDMI-A-1\n"));
        assert!(text.contains("place 100 at next 2200\n"));
        assert_eq!(parse_screens(&text), (set, kept, Some(133)));
        // The laptop screen of version 1 is number 100.
        let (old, _, next) = parse_screens("row\nscreen 0 2560x1440\nmirror at 10 20\n");
        assert_eq!(old.rows[0][1].id, laptop);
        assert_eq!(old.rows[0][1].middle, Some(20));
        assert_eq!(next, None);
    }

    /// Each headset's rights: a line for a headset with other rights than all; one
    /// without a line is the main one with every right.
    #[test]
    fn rights_are_kept_by_headset() {
        let key = [0xab; 32];
        let line = format!("access {} guest input mic", "AB".repeat(32));
        let (s, kept) = parse_settings(&line);
        assert!(kept.unknown.is_empty());
        let a = s.access_of(&key);
        assert!(!a.is_main() && a.may(perms::INPUT | perms::MIC) && !a.may(perms::CHANGES));
        assert_eq!(s.access_of(&[1; 32]), Access::MAIN);
        assert!(
            settings_text(&s, &kept)
                .contains(&format!("access {} guest input mic\n", "ab".repeat(32)))
        );
        // A key that is not one: kept as it was, and nothing more.
        let (s, kept) = parse_settings("access 12 main");
        assert!(s.access.is_empty() && kept.unknown == ["access 12 main"]);
        // A role or right mistyped: that headset gets no rights at all (not every
        // right, as without a line), and the line stays as it was to put right.
        for bad in [
            "access {k} boss",
            "access {k} main fly",
            "access {k} guest input mike",
            "access {k}",
        ] {
            let bad = bad.replace("{k}", &"ab".repeat(32));
            let (s, kept) = parse_settings(&bad);
            let a = s.access_of(&key);
            assert!(!a.is_main() && a.perms == 0, "{bad}: {a:?}");
            assert_eq!(kept.unknown, std::slice::from_ref(&bad));
            let text = settings_text(&s, &kept);
            assert_eq!(text.matches("access ").count(), 1, "{text}");
            assert_eq!(parse_settings(&text).0.access_of(&key), a, "the same again");
        }
        // A mistyped line beside a good one for the same headset, in either order:
        // no rights while the mistyped one is there, and both lines kept as they were.
        let good = format!("access {} guest input", "ab".repeat(32));
        let typo = format!("access {} guest inptu", "ab".repeat(32));
        for text in [format!("{good}\n{typo}\n"), format!("{typo}\n{good}\n")] {
            let (s, kept) = parse_settings(&text);
            assert_eq!(s.access_of(&key).perms, 0, "{text}");
            let saved = settings_text(&s, &kept);
            assert!(saved.contains(&format!("{good}\n")), "{saved}");
            assert!(saved.contains(&format!("{typo}\n")), "{saved}");
            let (again, kept_again) = parse_settings(&saved);
            assert_eq!(again.access_of(&key).perms, 0);
            assert_eq!(settings_text(&again, &kept_again), saved, "stable");
        }
    }

    /// Numbers never come back: the next block is on disk before its first number is
    /// given out, so after a crash the counter goes on after it (a gap, never a number
    /// twice). It starts above every number earlier versions gave out.
    #[test]
    fn numbers_never_come_back() {
        let dir = temp_dir("counter");
        std::fs::write(
            dir.join("screens.txt"),
            "row\nscreen 0 2560x1440\nscreen 5 1920x1080\n",
        )
        .unwrap();
        let mut store = Store::open(Some(dir.clone()), Run::default());
        let first = store.new_id();
        assert_eq!(
            first,
            ScreenId::saved(FIRST_ID),
            "above 0..5, 10..12 and 100"
        );
        let on_disk = std::fs::read_to_string(dir.join("screens.txt")).unwrap();
        assert!(
            on_disk.contains(&format!("next {}\n", FIRST_ID + ID_BLOCK)),
            "{on_disk}"
        );
        let given: Vec<ScreenId> = (0..20).map(|_| store.new_id()).collect();
        // Crashed: nothing more written.
        drop(store);
        let mut again = Store::open(Some(dir.clone()), Run::default());
        let after = again.new_id();
        assert!(
            given.iter().all(|g| *g < after),
            "{after:?} after {given:?}"
        );
        assert_eq!(
            after,
            ScreenId::saved(FIRST_ID + 2 * ID_BLOCK),
            "the gap of a block"
        );
        again.close();
        // Saved numbers above the counter push it up.
        std::fs::write(
            dir.join("screens.txt"),
            "next 150\nrow\nscreen 400 2560x1440\n",
        )
        .unwrap();
        let mut high = Store::open(Some(dir.clone()), Run::default());
        assert_eq!(high.new_id(), ScreenId::saved(401));
        high.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Bad lines are skipped and kept; a monitor listed twice counts once.
    #[test]
    fn bad_lines_are_skipped() {
        let (set, kept, _) = parse_screens(
            "row\nscreen 0 2560x1440\nscreen 0 1920x1080\nscreen x 1x1\nscreen 1 9x9\n",
        );
        assert_eq!(set.specs.len(), 1);
        assert_eq!(kept.unknown.len(), 3);
    }

    #[test]
    fn the_command_line_holds_for_this_run_only() {
        let saved = SavedScreens::of(&[spec("2560x1440"), spec("2560x1440")]);
        assert_eq!(startup_set(&saved, &[]), None, "the saved set");
        let set = startup_set(&saved, &[spec("1920x1080")]).unwrap();
        assert_eq!(set.specs.len(), 1);
        assert_eq!(
            startup_set(&SavedScreens::default(), &[]),
            Some(SavedScreens::first())
        );

        let dir = temp_dir("run");
        let run = Run {
            bitrate: Some(8),
            codec: Some(Codec::H264),
            software: Some(true),
            screens: Some(set.clone()),
        };
        let mut store = Store::open(Some(dir.clone()), run);
        store.set_screens(saved.clone(), true);
        store.close();
        // A first start: the defaults and the chosen set are saved, not the options.
        let mut store = Store::open(Some(dir.clone()), Run::default());
        assert_eq!(store.settings, Settings::default());
        assert_eq!(store.screens(), &saved);
        // Changed while running: it holds over the command line, and is saved.
        store.run.bitrate = Some(8);
        store.set_bitrate(15);
        assert_eq!(store.wanted().bitrate, 15);
        // A layout change to a set from the command line stays in this run.
        store.run.screens = Some(set.clone());
        let mut moved = set.clone();
        moved.rows[0][0].center = Some(100);
        store.set_screens(moved, false);
        store.close();
        let store = Store::open(Some(dir.clone()), Run::default());
        assert_eq!(store.settings.bitrate, 15);
        assert_eq!(store.screens(), &saved);
        store.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A first start writes no empty set, so the next start does not take it for a
    /// broken file.
    #[test]
    fn a_first_start_leaves_nothing_broken() {
        let dir = temp_dir("first");
        let mut store = Store::open(Some(dir.clone()), Run::default());
        store.start_with(&[]);
        assert_eq!(store.screens(), &SavedScreens::first());
        store.change(|s| s.audio = false);
        store.close();
        assert!(!dir.join("screens.txt").exists());
        let mut store = Store::open(Some(dir.clone()), Run::default());
        assert!(!store.settings.audio);
        // Arranged in the headset: kept.
        store.start_with(&[]);
        let mut moved = SavedScreens::first();
        moved.rows[0][0].middle = Some(10);
        store.set_screens(moved.clone(), false);
        store.close();
        let store = Store::open(Some(dir.clone()), Run::default());
        assert_eq!(store.screens(), &moved);
        assert!(!dir.join("screens.txt.bad").exists());
        store.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The codec the person chose stays chosen, whatever the graphics card made.
    #[test]
    fn the_wanted_codec_stays() {
        let dir = temp_dir("codec");
        let mut store = Store::open(Some(dir.clone()), Run::default());
        store.set_encoding(30, false, Codec::H265, false);
        store.close();
        let store = Store::open(Some(dir.clone()), Run::default());
        assert_eq!(store.settings.codec, Codec::H265);
        store.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 0.1's files move over once, and stay for a downgrade; a cut-short write is
    /// removed; a file with nothing readable is kept aside.
    #[test]
    fn moving_over_and_broken_files() {
        let dir = temp_dir("migrate");
        let old = "row\n2560x1440 at -1300 200\n1920x1080 *\nmirror off\n";
        std::fs::write(dir.join("monitors.txt"), old).unwrap();
        std::fs::write(dir.join("count"), "off\n").unwrap();
        std::fs::write(dir.join("screens.txt.tmp"), "half a fi").unwrap();
        let store = Store::open(Some(dir.clone()), Run::default());
        assert!(!store.settings.mirror && !store.settings.count);
        assert_eq!(store.screens().specs.len(), 2);
        assert!(!dir.join("screens.txt.tmp").exists());
        store.close();
        assert_eq!(
            std::fs::read_to_string(dir.join("monitors.txt")).unwrap(),
            old
        );
        assert!(dir.join("settings.txt").exists() && dir.join("screens.txt").exists());
        // The new files win from now on.
        std::fs::write(dir.join("monitors.txt"), "3840x2160\n").unwrap();
        let store = Store::open(Some(dir.clone()), Run::default());
        assert_eq!(store.screens().specs.len(), 2);
        store.close();
        // Nothing readable: the default, and the file kept aside.
        std::fs::write(dir.join("screens.txt"), "garbage\n").unwrap();
        let store = Store::open(Some(dir.clone()), Run::default());
        assert!(store.screens().specs.is_empty());
        assert!(dir.join("screens.txt.bad").exists());
        store.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// When each paired headset was last seen, with its app, is kept between runs;
    /// a forgotten headset's goes, and lines that cannot be read are left out.
    #[test]
    fn when_each_headset_was_seen_is_kept() {
        let dir = temp_dir("seen");
        let (a, b) = ([0xa1; 32], [0xb2; 32]);
        let mut store = Store::open(Some(dir.clone()), Run::default());
        assert_eq!(store.last_seen(&a), None);
        store.headset_seen(&a, 100, Some("0.2.0-dev+p6"));
        store.headset_seen(&b, 200, None);
        store.headset_seen(&a, 300, None);
        store.close();
        let mut store = Store::open(Some(dir.clone()), Run::default());
        let seen = |at, app: &str| Seen {
            at,
            app: app.into(),
        };
        assert_eq!(store.last_seen(&a), Some(&seen(300, "0.2.0-dev+p6")));
        assert_eq!(store.last_seen(&b), Some(&seen(200, "")));
        store.keep_seen_of(&[a]);
        assert_eq!(store.last_seen(&b), None);
        store.close();
        let text = std::fs::read_to_string(dir.join("headsets.txt")).unwrap();
        assert_eq!(text, format!("seen {} 300 0.2.0-dev+p6\n", hex(&a)));
        let odd = format!("seen nothex 1\nseen {} soon\nwhatever\n", hex(&a));
        assert!(parse_seen_headsets(&odd).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
