//! Headset-side preferences chosen in the menu, kept between launches.

use std::path::Path;

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
    Panorama,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub preset: Preset,
    pub curve: Curve,
    pub background: Background,
    /// Hands drive the pointer too. Off by default: hands over a keyboard would
    /// otherwise click and drag things while typing.
    pub hands: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preset: Preset::Arc,
            curve: Curve::Flat,
            background: Background::Black,
            hands: false,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        let mut s = Self::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return s;
        };
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
                Some(("hands", v)) => s.hands = v == "on",
                Some(("background", v)) => {
                    s.background = match v {
                        "room" => Background::Room,
                        "panorama" => Background::Panorama,
                        _ => Background::Black,
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self, path: &Path) {
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
            Background::Panorama => "panorama",
        };
        let hands = if self.hands { "on" } else { "off" };
        let text =
            format!("preset {preset}\ncurve {curve}\nbackground {background}\nhands {hands}\n");
        if let Err(e) = std::fs::write(path, text) {
            log::warn!("cannot save settings to {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("qmon-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.txt");
        let s = Settings {
            preset: Preset::Overhead,
            curve: Curve::Gentle,
            background: Background::Panorama,
            hands: true,
        };
        s.save(&path);
        assert_eq!(Settings::load(&path), s);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
