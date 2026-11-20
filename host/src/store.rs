//! What the PC keeps between runs, with one owner: the settings (`settings.txt`) and
//! the set of screens with the rows the headset shows them in (`screens.txt`), both
//! plain `key value` text in `~/.config/prostir/`.
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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use log::{info, warn};
use prostir_layout::desk::Entry;
use prostir_proto::{Codec, MAX_MONITORS};

use crate::platform::MonitorSpec;
use crate::screen::ScreenId;

/// The file format this version writes.
const VERSION: u32 = 1;
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
    /// The headset microphone as the PC's microphone.
    pub mic: bool,
    /// The laptop screen in the headset.
    pub mirror: bool,
    /// Count this PC among users (see `usage`).
    pub count: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bitrate: 30,
            sharp: false,
            codec: Codec::H265,
            software: false,
            audio: true,
            mic: true,
            mirror: true,
            count: true,
        }
    }
}

/// The set of virtual monitors and how the headset shows them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SavedScreens {
    pub specs: BTreeMap<ScreenId, MonitorSpec>,
    /// Rows from top to bottom, left to right, with where each is seen; the laptop
    /// screen ([`ScreenId::BUILTIN`]) too, where the headset put it.
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
            primary: None,
        }
    }

    /// `specs` side by side in one row, numbered from 0, where nothing is known.
    pub fn of(specs: &[MonitorSpec]) -> Self {
        let ids: Vec<ScreenId> = (0..specs.len() as u32).map(ScreenId::saved).collect();
        Self {
            specs: ids.iter().copied().zip(specs.iter().cloned()).collect(),
            rows: vec![ids.iter().map(|&id| Entry::new(id)).collect()],
            primary: None,
        }
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
        let mut set = SavedScreens::of(&args[..args.len().min(MAX_MONITORS)]);
        // The laptop screen stays where it was seen.
        if let Some(e) = saved
            .rows
            .iter()
            .flatten()
            .find(|e| e.id == ScreenId::BUILTIN)
        {
            set.rows.push(vec![*e]);
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
            ("mic", Some(v)) => {
                s.mic = v;
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
            _ => false,
        };
        if !known {
            kept.unknown.push(line.to_owned());
        }
    }
    (s, kept)
}

fn settings_text(s: &Settings, kept: &Kept) -> String {
    format!(
        "{}bitrate {}\nsharp {}\ncodec {}\nsoftware {}\naudio {}\nmic {}\nmirror {}\ncount {}\n",
        kept.head(),
        s.bitrate,
        on_off(s.sharp),
        codec_name(s.codec),
        on_off(s.software),
        on_off(s.audio),
        on_off(s.mic),
        on_off(s.mirror),
        on_off(s.count),
    )
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

/// `row`, then `screen N WxH[@scale] [at C M] [primary]` and `mirror [at C M]`.
fn parse_screens(text: &str) -> (SavedScreens, Kept) {
    let mut set = SavedScreens::default();
    let mut kept = Kept::default();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut words = line.split_whitespace();
        let known = match words.next() {
            Some("version") => words
                .next()
                .and_then(|v| v.parse().ok())
                .map(|v| kept.version = v)
                .is_some(),
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
                if id == ScreenId::BUILTIN || set.specs.contains_key(&id) {
                    return None;
                }
                set.specs.insert(id, spec);
                if primary {
                    set.primary = Some(id);
                }
                if set.rows.is_empty() {
                    set.rows.push(Vec::new());
                }
                set.rows.last_mut()?.push(Entry {
                    id,
                    center: seen.0,
                    middle: seen.1,
                });
                Some(())
            })()
            .is_some(),
            Some("mirror") => {
                let seen = match words.next() {
                    Some("at") => parse_seen(words),
                    _ => (None, None),
                };
                if set.rows.is_empty() {
                    set.rows.push(Vec::new());
                }
                if let Some(row) = set.rows.last_mut() {
                    row.push(Entry {
                        id: ScreenId::BUILTIN,
                        center: seen.0,
                        middle: seen.1,
                    });
                }
                true
            }
            _ => false,
        };
        if !known {
            kept.unknown.push(line.to_owned());
        }
    }
    set.rows.retain(|r| !r.is_empty());
    (set, kept)
}

fn screens_text(set: &SavedScreens, kept: &Kept) -> String {
    let mut text = kept.head();
    for row in &set.rows {
        text.push_str("row\n");
        for e in row {
            if e.id == ScreenId::BUILTIN {
                text.push_str(&format!("mirror{}\n", seen_text(e)));
            } else if let Some(spec) = set.specs.get(&e.id) {
                let primary = if set.primary == Some(e.id) {
                    " primary"
                } else {
                    ""
                };
                text.push_str(&format!(
                    "screen {} {spec}{}{primary}\n",
                    e.id.number(),
                    seen_text(e)
                ));
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

/// The writes waiting for the writer thread, the latest of each file.
enum Job {
    Write(PathBuf, String),
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
                for job in std::iter::once(first).chain(jobs.try_iter()) {
                    match job {
                        Job::Write(path, text) => {
                            latest.insert(path, text);
                        }
                        Job::Stop => stop = true,
                    }
                }
                for (path, text) in latest {
                    if let Err(e) = write_durably(&path, &text) {
                        warn!("cannot save {}: {e}", path.display());
                    }
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

/// The settings and the set of screens, owned by the main loop.
pub struct Store {
    /// `~/.config/prostir`; none: nothing is saved (no home directory).
    dir: Option<PathBuf>,
    settings: Settings,
    settings_kept: Kept,
    screens: SavedScreens,
    screens_kept: Kept,
    run: Run,
    settings_changed: bool,
    screens_changed: bool,
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
            run,
            settings_changed: false,
            screens_changed: false,
            writer: None,
        };
        store.load();
        store.writer = store.dir.as_ref().and_then(|_| spawn_writer());
        store
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(name))
    }

    fn load(&mut self) {
        let Some(dir) = self.dir.clone() else { return };
        // A write cut short leaves its temporary file; the real one is whole.
        for name in ["settings.txt.tmp", "screens.txt.tmp"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
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
            return;
        }
        if let Some(text) = settings {
            (self.settings, self.settings_kept) = parse_settings(&text);
        }
        if let Some(text) = screens {
            let (set, kept) = parse_screens(&text);
            if set.specs.is_empty() {
                // Nothing in it could be read: kept aside, so the next write does not
                // wipe out what went wrong.
                warn!("screens.txt has no monitor in it; kept as screens.txt.bad");
                let _ = std::fs::copy(dir.join("screens.txt"), dir.join("screens.txt.bad"));
            } else {
                (self.screens, self.screens_kept) = (set, kept);
            }
        }
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

    /// Hands what changed to the writer thread.
    pub fn flush(&mut self) {
        let mut jobs = Vec::new();
        if std::mem::take(&mut self.settings_changed)
            && let Some(path) = self.path("settings.txt")
        {
            jobs.push((path, settings_text(&self.settings, &self.settings_kept)));
        }
        // Never a set without monitors: that would read as a broken file.
        if std::mem::take(&mut self.screens_changed)
            && !self.screens.specs.is_empty()
            && let Some(path) = self.path("screens.txt")
        {
            jobs.push((path, screens_text(&self.screens, &self.screens_kept)));
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
                mic: false,
                mirror: true,
                count: true,
            }
        );
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
        let text = screens_text(&set, &Kept::default());
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
        };
        let text = settings_text(&settings, &kept);
        assert_eq!(parse_settings(&text), (settings, kept.clone()));

        let mut set = SavedScreens::of(&[spec("2560x1440"), spec("1440x2560@1.25")]);
        set.rows[0][1].center = Some(720);
        set.rows[0][1].middle = Some(-40);
        set.rows.push(vec![Entry {
            id: ScreenId::BUILTIN,
            center: None,
            middle: Some(2200),
        }]);
        set.primary = Some(ScreenId::saved(1));
        let text = screens_text(&set, &kept);
        assert!(text.contains("screen 1 1440x2560@1.25 at 720 -40 primary\n"));
        assert!(text.contains("mirror at next 2200\n"));
        assert_eq!(parse_screens(&text), (set, kept));
    }

    /// Bad lines are skipped and kept; a monitor listed twice counts once.
    #[test]
    fn bad_lines_are_skipped() {
        let (set, kept) = parse_screens(
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
        store.change(|s| s.mic = false);
        store.close();
        assert!(!dir.join("screens.txt").exists());
        let mut store = Store::open(Some(dir.clone()), Run::default());
        assert!(!store.settings.mic);
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
}
