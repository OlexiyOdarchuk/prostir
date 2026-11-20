//! The window's look as data: TOML files with a light and a dark set of colour tokens,
//! sizes, type and motion. Widgets never hold a colour or a size of their own; they
//! name a token.
//!
//! Layers, each key by key on the one below: the built-in `themes/ink.toml` (every
//! key), the chosen built-in palette (`themes/graphite.toml` has only colours), and the
//! person's `~/.config/prostir/window-theme.toml`. Whatever a layer gets wrong keeps
//! the value below it and is listed in [`Theme::issues`]; the window always opens.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui::Color32;

pub const INK: &str = include_str!("../themes/ink.toml");
pub const GRAPHITE: &str = include_str!("../themes/graphite.toml");

/// The only format there is so far.
const FORMAT: i64 = 1;

/// The built-in palettes, chosen on the Look page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PaletteName {
    /// "Prostir": blue-tinted surfaces.
    #[default]
    Ink,
    /// "Neutral": grey surfaces.
    Graphite,
}

impl PaletteName {
    pub const ALL: [PaletteName; 2] = [PaletteName::Ink, PaletteName::Graphite];

    /// The name in the state and in the theme files.
    pub fn id(self) -> &'static str {
        match self {
            PaletteName::Ink => "ink",
            PaletteName::Graphite => "graphite",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == id)
    }

    fn source(self) -> Option<&'static str> {
        match self {
            PaletteName::Ink => None,
            PaletteName::Graphite => Some(GRAPHITE),
        }
    }
}

/// Light or dark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    Light,
    Dark,
}

impl Scheme {
    pub fn id(self) -> &'static str {
        match self {
            Scheme::Light => "light",
            Scheme::Dark => "dark",
        }
    }
}

fn color(v: &toml::Value) -> Option<Color32> {
    let s = v.as_str()?.strip_prefix('#')?;
    if !s.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
    match s.len() {
        6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(
            byte(0)?,
            byte(2)?,
            byte(4)?,
            byte(6)?,
        )),
        _ => None,
    }
}

/// A number within `min..=max`; integers are numbers too.
fn number(min: f32, max: f32) -> impl Fn(&toml::Value) -> Option<f32> {
    move |v| {
        let n = v.as_float().or_else(|| v.as_integer().map(|i| i as f64))? as f32;
        (n.is_finite() && (min..=max).contains(&n)).then_some(n)
    }
}

fn font(v: &toml::Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A TOML table as a struct: each field is a token with its own parser.
macro_rules! tokens {
    ($(#[$doc:meta])* $name:ident { $($k:ident : $ty:ty = $parse:expr),* $(,)? }) => {
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq)]
        pub struct $name { $(pub $k: $ty,)* }

        impl $name {
            pub const KEYS: &'static [&'static str] = &[$(stringify!($k)),*];

            fn apply(&mut self, table: &toml::Table, section: &str, issues: &mut Vec<Issue>) {
                for (key, value) in table {
                    let at = format!("[{section}] {key}");
                    match key.as_str() {
                        $(stringify!($k) => match ($parse)(value) {
                            Some(v) => self.$k = v,
                            None => issues.push(Issue { at, kind: IssueKind::BadValue(value.to_string()) }),
                        },)*
                        _ => issues.push(Issue { at, kind: IssueKind::Unknown }),
                    }
                }
            }
        }
    };
}

tokens!(
    /// One scheme's colours.
    Palette {
        background: Color32 = color,
        sidebar: Color32 = color,
        surface: Color32 = color,
        well: Color32 = color,
        control: Color32 = color,
        control_hover: Color32 = color,
        border: Color32 = color,
        text: Color32 = color,
        text_muted: Color32 = color,
        accent: Color32 = color,
        accent_hover: Color32 = color,
        accent_text: Color32 = color,
        accent_ink: Color32 = color,
        focus: Color32 = color,
        ok: Color32 = color,
        warning: Color32 = color,
        danger: Color32 = color,
        screen: Color32 = color,
        screen_pc: Color32 = color,
        screen_text: Color32 = color,
        selection: Color32 = color,
        shadow: Color32 = color,
    }
);

tokens!(
    /// The app icon's colours.
    Brand {
        tile: Color32 = color,
        screen: Color32 = color,
        screen_side: Color32 = color,
        base: Color32 = color,
    }
);

tokens!(
    /// Sizes in logical pixels.
    Shape {
        radius_small: f32 = number(0.0, 32.0),
        radius: f32 = number(0.0, 32.0),
        radius_large: f32 = number(0.0, 48.0),
        stroke: f32 = number(0.0, 4.0),
        focus_width: f32 = number(1.0, 6.0),
        spacing: f32 = number(2.0, 32.0),
        padding: f32 = number(4.0, 48.0),
        section_gap: f32 = number(4.0, 96.0),
        control_height: f32 = number(24.0, 72.0),
        sidebar_width: f32 = number(160.0, 400.0),
        content_max_width: f32 = number(480.0, 2000.0),
    }
);

tokens!(
    /// Fonts and text sizes.
    Type {
        font: String = font,
        font_medium: String = font,
        size_small: f32 = number(8.0, 32.0),
        size_body: f32 = number(9.0, 36.0),
        size_heading: f32 = number(9.0, 40.0),
        size_title: f32 = number(12.0, 64.0),
        size_display: f32 = number(12.0, 72.0),
    }
);

tokens!(
    /// Durations in milliseconds.
    Motion {
        hover_ms: f32 = number(0.0, 2000.0),
        theme_ms: f32 = number(0.0, 2000.0),
        state_ms: f32 = number(0.0, 2000.0),
    }
);

impl Palette {
    /// Between `self` (0) and `to` (1), for the light and dark cross-fade.
    pub fn lerp(&self, to: &Self, t: f32) -> Self {
        let l = |a: Color32, b: Color32| mix(a, b, t);
        macro_rules! each {
            ($($k:ident),*) => { Palette { $($k: l(self.$k, to.$k),)* } };
        }
        each!(
            background,
            sidebar,
            surface,
            well,
            control,
            control_hover,
            border,
            text,
            text_muted,
            accent,
            accent_hover,
            accent_text,
            accent_ink,
            focus,
            ok,
            warning,
            danger,
            screen,
            screen_pc,
            screen_text,
            selection,
            shadow
        )
    }
}

/// Between `a` (0) and `b` (1), channel by channel.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    if t <= 0.0 {
        return a;
    }
    if t >= 1.0 {
        return b;
    }
    let (a, b) = (a.to_srgba_unmultiplied(), b.to_srgba_unmultiplied());
    let c = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(c(0), c(1), c(2), c(3))
}

/// Something in the person's theme file the window did not use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// Where: `[dark] text`, a font's path, or the file itself (empty).
    pub at: String,
    pub kind: IssueKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueKind {
    /// The file is not TOML; the parser's message.
    NotToml(String),
    /// `format` is not one this window reads.
    Format(String),
    /// A value of the wrong kind or out of range; the value as written.
    BadValue(String),
    /// A key or section the window does not know.
    Unknown,
    /// A font file that cannot be read; the reason.
    Font(String),
}

/// A pair of colours that are too close to read one on the other.
#[derive(Clone, Debug, PartialEq)]
pub struct LowContrast {
    pub scheme: Scheme,
    pub fg: &'static str,
    pub bg: &'static str,
    pub ratio: f32,
    pub need: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    pub light: Palette,
    pub dark: Palette,
    pub brand: Brand,
    pub shape: Shape,
    pub type_: Type,
    pub motion: Motion,
    /// What in the person's file was not used.
    pub issues: Vec<Issue>,
    /// Text that would be hard to read, after all the layers.
    pub contrast: Vec<LowContrast>,
}

impl Theme {
    pub fn palette(&self, scheme: Scheme) -> &Palette {
        match scheme {
            Scheme::Light => &self.light,
            Scheme::Dark => &self.dark,
        }
    }

    fn empty() -> Self {
        Theme {
            name: String::new(),
            light: Palette::default(),
            dark: Palette::default(),
            brand: Brand::default(),
            shape: Shape::default(),
            type_: Type::default(),
            motion: Motion::default(),
            issues: Vec::new(),
            contrast: Vec::new(),
        }
    }

    fn apply(&mut self, src: &str, issues: &mut Vec<Issue>) {
        let doc = match src.parse::<toml::Table>() {
            Ok(doc) => doc,
            Err(e) => {
                issues.push(Issue {
                    at: String::new(),
                    kind: IssueKind::NotToml(e.message().trim().to_owned()),
                });
                return;
            }
        };
        if let Some(f) = doc.get("format")
            && f.as_integer() != Some(FORMAT)
        {
            // A newer format may mean something else by the same keys: none of it is used.
            issues.push(Issue {
                at: "format".into(),
                kind: IssueKind::Format(f.to_string()),
            });
            return;
        }
        for (key, value) in &doc {
            match (key.as_str(), value) {
                ("format", _) => {}
                ("name", toml::Value::String(s)) => self.name = s.clone(),
                ("light", toml::Value::Table(t)) => self.light.apply(t, "light", issues),
                ("dark", toml::Value::Table(t)) => self.dark.apply(t, "dark", issues),
                ("brand", toml::Value::Table(t)) => self.brand.apply(t, "brand", issues),
                ("shape", toml::Value::Table(t)) => self.shape.apply(t, "shape", issues),
                ("type", toml::Value::Table(t)) => self.type_.apply(t, "type", issues),
                ("motion", toml::Value::Table(t)) => self.motion.apply(t, "motion", issues),
                ("name" | "light" | "dark" | "brand" | "shape" | "type" | "motion", v) => issues
                    .push(Issue {
                        at: key.clone(),
                        kind: IssueKind::BadValue(v.to_string()),
                    }),
                _ => issues.push(Issue {
                    at: key.clone(),
                    kind: IssueKind::Unknown,
                }),
            }
        }
    }

    /// A built-in palette alone. The built-in files are complete and clean (tested).
    pub fn builtin(palette: PaletteName) -> Self {
        let mut theme = Self::empty();
        let mut issues = Vec::new();
        theme.apply(INK, &mut issues);
        if let Some(src) = palette.source() {
            theme.apply(src, &mut issues);
        }
        debug_assert!(issues.is_empty(), "built-in theme: {issues:?}");
        theme.contrast = theme.check_contrast();
        theme
    }

    /// The built-in palette with the person's file on top, key by key.
    pub fn load(palette: PaletteName, user: Option<&str>) -> Self {
        let mut theme = Self::builtin(palette);
        let mut issues = Vec::new();
        if let Some(src) = user {
            let before = theme.type_.clone();
            theme.apply(src, &mut issues);
            for (field, name) in [
                (&mut theme.type_.font, "font"),
                (&mut theme.type_.font_medium, "font_medium"),
            ] {
                if let Err(why) = font_exists(field) {
                    issues.push(Issue {
                        at: format!("[type] {name}"),
                        kind: IssueKind::Font(why),
                    });
                    *field = if name == "font" {
                        before.font.clone()
                    } else {
                        before.font_medium.clone()
                    };
                }
            }
        }
        for i in &issues {
            log::warn!("window theme: not used: {} {:?}", i.at, i.kind);
        }
        theme.issues = issues;
        theme.contrast = theme.check_contrast();
        for c in &theme.contrast {
            log::warn!(
                "window theme: low contrast in {}: {} on {} is {:.1}:1, needs {}:1",
                c.scheme.id(),
                c.fg,
                c.bg,
                c.ratio,
                c.need
            );
        }
        theme
    }

    /// Text and the backgrounds it is drawn on, against WCAG AA: 4.5:1 for text, 3:1
    /// for the focus ring and large text.
    fn check_contrast(&self) -> Vec<LowContrast> {
        let mut out = Vec::new();
        for scheme in [Scheme::Light, Scheme::Dark] {
            let p = self.palette(scheme);
            let pairs = [
                ("text", p.text, "background", p.background, 4.5),
                ("text", p.text, "surface", p.surface, 4.5),
                ("text", p.text, "sidebar", p.sidebar, 4.5),
                ("text", p.text, "control", p.control, 4.5),
                ("text", p.text, "well", p.well, 4.5),
                ("text_muted", p.text_muted, "background", p.background, 4.5),
                ("text_muted", p.text_muted, "surface", p.surface, 4.5),
                ("text_muted", p.text_muted, "sidebar", p.sidebar, 4.5),
                ("accent_text", p.accent_text, "accent", p.accent, 4.5),
                ("accent_ink", p.accent_ink, "surface", p.surface, 4.5),
                ("danger", p.danger, "surface", p.surface, 4.5),
                ("screen_text", p.screen_text, "screen", p.screen, 4.5),
                ("screen_text", p.screen_text, "screen_pc", p.screen_pc, 4.5),
                ("focus", p.focus, "background", p.background, 3.0),
                ("focus", p.focus, "surface", p.surface, 3.0),
            ];
            for (fg, cf, bg, cb, need) in pairs {
                let ratio = contrast(cf, cb);
                if ratio < need {
                    out.push(LowContrast {
                        scheme,
                        fg,
                        bg,
                        ratio,
                        need,
                    });
                }
            }
        }
        out
    }
}

/// The built-in font names, or a file that can be read.
fn font_exists(name: &str) -> Result<(), String> {
    if crate::fonts::is_builtin(name) {
        return Ok(());
    }
    let path = expand_home(name);
    std::fs::File::open(&path)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// `~/x` as the home folder's `x`.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// WCAG contrast ratio of two opaque colours (alpha is ignored).
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let lum = |c: Color32| {
        let [r, g, b, _] = c.to_srgba_unmultiplied();
        let ch = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// Where the person's theme file lives: `$XDG_CONFIG_HOME/prostir/window-theme.toml`
/// (`~/.config/...`). The name differs from the headset themes' `theme.toml`.
pub fn user_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("prostir/window-theme.toml")
}

/// The person's theme file, looked at once a second while the window is open.
pub struct ThemeFile {
    path: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    last_look: f64,
    text: Option<String>,
}

impl ThemeFile {
    /// `None`: no file at all (tests).
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut f = Self {
            path,
            stamp: None,
            last_look: f64::NEG_INFINITY,
            text: None,
        };
        f.read();
        f
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether the file is there.
    pub fn exists(&self) -> bool {
        self.text.is_some()
    }

    /// Its text, if it is there.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    fn stamp(&self) -> Option<(SystemTime, u64)> {
        let m = std::fs::metadata(self.path.as_ref()?).ok()?;
        Some((m.modified().ok()?, m.len()))
    }

    fn read(&mut self) {
        self.stamp = self.stamp();
        self.text = self
            .path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok());
    }

    /// Whether the file changed since the last look; looks at most once a second
    /// (`now` in seconds).
    pub fn changed(&mut self, now: f64) -> bool {
        if now - self.last_look < 1.0 {
            return false;
        }
        self.last_look = now;
        if self.stamp() == self.stamp {
            return false;
        }
        self.read();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ink_sets_every_key_and_graphite_every_colour() {
        let ink: toml::Table = INK.parse().unwrap();
        for (section, keys) in [
            ("light", Palette::KEYS),
            ("dark", Palette::KEYS),
            ("brand", Brand::KEYS),
            ("shape", Shape::KEYS),
            ("type", Type::KEYS),
            ("motion", Motion::KEYS),
        ] {
            let t = ink[section].as_table().unwrap();
            for k in keys {
                assert!(t.contains_key(*k), "ink: [{section}] {k} missing");
            }
            assert_eq!(t.len(), keys.len(), "ink: [{section}] has extra keys");
        }
        let graphite: toml::Table = GRAPHITE.parse().unwrap();
        for section in ["light", "dark"] {
            let t = graphite[section].as_table().unwrap();
            for k in Palette::KEYS {
                assert!(t.contains_key(*k), "graphite: [{section}] {k} missing");
            }
        }
    }

    #[test]
    fn built_in_palettes_are_clean_and_readable() {
        for p in PaletteName::ALL {
            let t = Theme::load(p, None);
            assert!(t.issues.is_empty(), "{p:?}: {:?}", t.issues);
            assert!(t.contrast.is_empty(), "{p:?}: {:?}", t.contrast);
        }
        assert_ne!(
            Theme::builtin(PaletteName::Ink).dark.background,
            Theme::builtin(PaletteName::Graphite).dark.background
        );
        // Graphite has only colours: the rest is ink's.
        assert_eq!(
            Theme::builtin(PaletteName::Ink).shape,
            Theme::builtin(PaletteName::Graphite).shape
        );
    }

    #[test]
    fn the_user_file_goes_on_top_key_by_key() {
        let t = Theme::load(
            PaletteName::Graphite,
            Some("format = 1\n[dark]\naccent = \"#FF0000\"\n[shape]\nradius = 4\n"),
        );
        let base = Theme::builtin(PaletteName::Graphite);
        assert_eq!(t.dark.accent, Color32::from_rgb(255, 0, 0));
        assert_eq!(t.dark.text, base.dark.text);
        assert_eq!(t.light, base.light);
        assert_eq!(t.shape.radius, 4.0);
        assert_eq!(t.shape.radius_large, base.shape.radius_large);
        assert!(t.issues.is_empty());
    }

    #[test]
    fn bad_keys_fall_back_and_are_listed() {
        let t = Theme::load(
            PaletteName::Ink,
            Some(
                "[dark]\ntext = \"white\"\nwat = 1\n[shape]\nradius = 400\n[type]\nfont = \"/nowhere/x.ttf\"\n[sound]\nloud = true\nname = 3\n",
            ),
        );
        let base = Theme::builtin(PaletteName::Ink);
        assert_eq!(t.dark.text, base.dark.text);
        assert_eq!(t.shape.radius, base.shape.radius);
        assert_eq!(t.type_.font, base.type_.font);
        let at: Vec<&str> = t.issues.iter().map(|i| i.at.as_str()).collect();
        assert_eq!(
            at,
            [
                "[dark] text",
                "[dark] wat",
                "[shape] radius",
                "sound",
                "[type] font"
            ]
        );
        assert!(matches!(t.issues[0].kind, IssueKind::BadValue(ref v) if v == "\"white\""));
        assert_eq!(t.issues[1].kind, IssueKind::Unknown);
        assert!(matches!(t.issues[4].kind, IssueKind::Font(_)));
    }

    #[test]
    fn a_broken_file_or_a_newer_format_gives_the_built_in_theme() {
        let base = Theme::builtin(PaletteName::Ink);
        let t = Theme::load(PaletteName::Ink, Some("this is [ not toml"));
        assert_eq!(
            (t.light.clone(), t.dark.clone()),
            (base.light.clone(), base.dark.clone())
        );
        assert!(matches!(
            t.issues[..],
            [Issue {
                kind: IssueKind::NotToml(_),
                ..
            }]
        ));
        let t = Theme::load(
            PaletteName::Ink,
            Some("format = 2\n[dark]\ntext = \"#000000\"\n"),
        );
        assert_eq!(t.dark.text, base.dark.text);
        assert!(matches!(
            t.issues[..],
            [Issue {
                kind: IssueKind::Format(_),
                ..
            }]
        ));
    }

    #[test]
    fn low_contrast_is_found() {
        let t = Theme::load(
            PaletteName::Ink,
            Some("[dark]\ntext_muted = \"#33405A\"\n[light]\naccent = \"#7FA0F0\"\n"),
        );
        let found: Vec<(Scheme, &str, &str)> =
            t.contrast.iter().map(|c| (c.scheme, c.fg, c.bg)).collect();
        assert!(
            found.contains(&(Scheme::Dark, "text_muted", "background")),
            "{found:?}"
        );
        assert!(
            found.contains(&(Scheme::Light, "accent_text", "accent")),
            "{found:?}"
        );
        assert!(t.contrast.iter().all(|c| c.ratio < c.need));
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
    }

    #[test]
    fn the_file_is_read_again_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("prostir-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("window-theme.toml");
        let mut f = ThemeFile::new(Some(path.clone()));
        assert!(!f.exists());
        assert!(!f.changed(0.0));
        std::fs::write(&path, "[shape]\nradius = 3\n").unwrap();
        assert!(!f.changed(0.5), "looks only once a second");
        assert!(f.changed(1.0));
        assert_eq!(f.text(), Some("[shape]\nradius = 3\n"));
        assert!(!f.changed(2.5));
        std::fs::write(&path, "[shape]\nradius = 12\n").unwrap();
        assert!(f.changed(4.0));
        std::fs::remove_file(&path).unwrap();
        assert!(f.changed(5.0));
        assert!(!f.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
