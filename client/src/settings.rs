//! Headset-side preferences chosen in the menu, kept between launches.

use std::path::Path;

pub use crate::background::Sky;
use crate::i18n::{Lang, Language};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Side by side on an arc around the user.
    Arc,
    /// Two rows.
    Grid,
    /// One above another.
    Column,
    /// Above the head, for lying down.
    Overhead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    Flat,
    Gentle,
    Strong,
}

impl Curve {
    /// Cylinder radius as a multiple of the monitor's distance from the user; `None` is flat.
    pub fn radius_factor(self) -> Option<f32> {
        match self {
            Curve::Flat => None,
            Curve::Gentle => Some(2.0),
            Curve::Strong => Some(1.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    Black,
    /// Passthrough cameras.
    Room,
    /// A built-in panorama.
    Sky(Sky),
    /// The user's own `background.jpg/png`.
    Custom,
}

/// What tracked hands do in the headset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandMode {
    /// Not drawn and not tracked by the app (saves rendering every frame).
    Hidden,
    /// Drawn, e.g. to see them over the keyboard, but they do not click or drag.
    Visible,
    /// Drawn and point, click and drag. Off by default: hands over a keyboard
    /// would otherwise click things while typing.
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub preset: Preset,
    pub curve: Curve,
    pub background: Background,
    pub hands: HandMode,
    /// Dim the monitors the mouse is not on.
    pub focus: bool,
    /// One big screen further away; the other monitors are hidden.
    pub cinema: bool,
    /// Position in the row of the monitor shown in cinema mode; `None`: the largest
    /// landscape one.
    pub cinema_screen: Option<u8>,
    /// The first-run tips were shown and dismissed.
    pub intro_seen: bool,
    pub language: Language,
    /// The language of the PC last connected to (for "like the PC" before connecting).
    pub pc_language: Option<Lang>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preset: Preset::Arc,
            curve: Curve::Flat,
            background: Background::Black,
            hands: HandMode::Visible,
            focus: false,
            cinema: false,
            cinema_screen: None,
            intro_seen: false,
            language: Language::LikePc,
            pc_language: None,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Reads `key value` lines; unknown keys are ignored and missing ones default.
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("preset", v)) => {
                    s.preset = match v {
                        "grid" => Preset::Grid,
                        "column" => Preset::Column,
                        "overhead" => Preset::Overhead,
                        _ => Preset::Arc,
                    }
                }
                Some(("curve", v)) => {
                    s.curve = match v {
                        "gentle" => Curve::Gentle,
                        "strong" => Curve::Strong,
                        _ => Curve::Flat,
                    }
                }
                Some(("focus", v)) => s.focus = v == "on",
                Some(("cinema", v)) => s.cinema = v == "on",
                Some(("cinema-screen", v)) => s.cinema_screen = v.parse().ok(),
                Some(("intro", v)) => s.intro_seen = v == "seen",
                Some(("language", v)) => {
                    s.language = match v {
                        "uk" => Language::Fixed(Lang::Uk),
                        "en" => Language::Fixed(Lang::En),
                        _ => Language::LikePc,
                    }
                }
                Some(("pc-language", v)) => s.pc_language = Lang::from_code(v),
                Some(("hands", v)) => {
                    s.hands = match v {
                        "hidden" => HandMode::Hidden,
                        "on" | "control" => HandMode::Control,
                        _ => HandMode::Visible,
                    }
                }
                Some(("background", v)) => {
                    s.background = match v {
                        "room" => Background::Room,
                        "custom" => Background::Custom,
                        "panorama" | "sky-night" => Background::Sky(Sky::Night),
                        "sky-sunset" => Background::Sky(Sky::Sunset),
                        "sky-space" => Background::Sky(Sky::Space),
                        "sky-aurora" => Background::Sky(Sky::Aurora),
                        _ => Background::Black,
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self, path: &Path) {
        if let Err(e) = std::fs::write(path, self.to_text()) {
            log::warn!("cannot save settings to {}: {e}", path.display());
        }
    }

    pub fn to_text(&self) -> String {
        let preset = match self.preset {
            Preset::Arc => "arc",
            Preset::Grid => "grid",
            Preset::Column => "column",
            Preset::Overhead => "overhead",
        };
        let curve = match self.curve {
            Curve::Flat => "flat",
            Curve::Gentle => "gentle",
            Curve::Strong => "strong",
        };
        let background = match self.background {
            Background::Black => "black",
            Background::Room => "room",
            Background::Custom => "custom",
            Background::Sky(Sky::Night) => "sky-night",
            Background::Sky(Sky::Sunset) => "sky-sunset",
            Background::Sky(Sky::Space) => "sky-space",
            Background::Sky(Sky::Aurora) => "sky-aurora",
        };
        let hands = match self.hands {
            HandMode::Hidden => "hidden",
            HandMode::Visible => "visible",
            HandMode::Control => "control",
        };
        let on = |b: bool| if b { "on" } else { "off" };
        format!(
            "preset {preset}\ncurve {curve}\nbackground {background}\nhands {hands}\n\
             focus {}\ncinema {}\ncinema-screen {}\nlanguage {}\npc-language {}\nintro {}\n",
            on(self.focus),
            on(self.cinema),
            self.cinema_screen
                .map_or_else(|| "auto".to_owned(), |i| i.to_string()),
            match self.language {
                Language::LikePc => "like-pc",
                Language::Fixed(l) => l.code(),
            },
            self.pc_language.map_or("", Lang::code),
            if self.intro_seen { "seen" } else { "new" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("prostir-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.txt");
        let s = Settings {
            preset: Preset::Overhead,
            curve: Curve::Gentle,
            background: Background::Sky(Sky::Aurora),
            hands: HandMode::Control,
            focus: true,
            cinema: true,
            cinema_screen: Some(2),
            intro_seen: true,
            language: Language::Fixed(Lang::En),
            pc_language: Some(Lang::Uk),
        };
        s.save(&path);
        assert_eq!(Settings::load(&path), s);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
