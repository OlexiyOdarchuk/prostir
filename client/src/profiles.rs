//! Profiles: a whole setup (monitors, laptop screen, layout, background, mode)
//! switched with one press. Three slots with sensible starting points; any of them
//! can be overwritten with the current setup.

use std::path::Path;

use crate::settings::{Background, Preset, Settings};

pub const SLOTS: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Monitor sizes, left to right.
    pub sizes: Vec<(u32, u32)>,
    /// Index into `sizes`.
    pub primary: u8,
    pub mirror: bool,
    /// Layout, curve, background and cinema are applied; hands and focus stay as
    /// they are (they are habits, not part of a setup).
    pub settings: Settings,
}

impl Profile {
    /// Slot 0: work, 1: cinema, 2: lying down.
    pub fn default_for(slot: usize) -> Self {
        let mut settings = Settings::default();
        match slot {
            1 => {
                settings.cinema = true;
                settings.background = Background::Black;
                Self {
                    sizes: vec![(3840, 2160)],
                    primary: 0,
                    mirror: false,
                    settings,
                }
            }
            2 => {
                settings.preset = Preset::Overhead;
                Self {
                    sizes: vec![(2560, 1440), (2560, 1440)],
                    primary: 0,
                    mirror: false,
                    settings,
                }
            }
            _ => Self {
                sizes: vec![(2560, 1440); 3],
                primary: 1,
                mirror: true,
                settings,
            },
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let mut sizes = Vec::new();
        let mut primary = 0;
        let mut mirror = false;
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("monitors", list)) => {
                    for (i, m) in list.split_whitespace().enumerate() {
                        let m = m
                            .strip_prefix('*')
                            .inspect(|_| primary = i as u8)
                            .unwrap_or(m);
                        let (w, h) = m.split_once('x')?;
                        sizes.push((w.parse().ok()?, h.parse().ok()?));
                    }
                }
                Some(("mirror", v)) => mirror = v == "on",
                _ => {}
            }
        }
        // A file edited by hand (or cut short) may list more than a PC makes.
        sizes.truncate(prostir_proto::MAX_MONITORS);
        if usize::from(primary) >= sizes.len() {
            primary = 0;
        }
        (!sizes.is_empty()).then(|| Self {
            sizes,
            primary,
            mirror,
            settings: Settings::parse(text),
        })
    }

    fn to_text(&self) -> String {
        let monitors: Vec<String> = self
            .sizes
            .iter()
            .enumerate()
            .map(|(i, (w, h))| {
                let star = if i == self.primary as usize { "*" } else { "" };
                format!("{star}{w}x{h}")
            })
            .collect();
        format!(
            "monitors {}\nmirror {}\n{}",
            monitors.join(" "),
            if self.mirror { "on" } else { "off" },
            self.settings.to_text()
        )
    }
}

/// `[slot]` sections in one file.
pub fn load(path: &Path) -> [Profile; SLOTS] {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: [Profile; SLOTS] = std::array::from_fn(Profile::default_for);
    for section in text.split("\n[").map(|s| s.trim_start_matches('[')) {
        let Some((slot, body)) = section.split_once("]\n") else {
            continue;
        };
        if let (Ok(i), Some(p)) = (slot.parse::<usize>(), Profile::parse(body))
            && i < SLOTS
        {
            out[i] = p;
        }
    }
    out
}

pub fn save(path: &Path, profiles: &[Profile; SLOTS]) {
    let text: String = profiles
        .iter()
        .enumerate()
        .map(|(i, p)| format!("[{i}]\n{}", p.to_text()))
        .collect::<Vec<_>>()
        .join("\n");
    if let Err(e) = prostir_proto::secure::write_atomic(path, text.as_bytes()) {
        log::warn!("cannot save profiles to {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Curve;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("prostir-profiles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles.txt");
        let mut profiles: [Profile; SLOTS] = std::array::from_fn(Profile::default_for);
        profiles[2].sizes = vec![(1920, 1080), (1440, 2560), (3440, 1440)];
        profiles[2].primary = 2;
        profiles[2].mirror = true;
        profiles[2].settings.curve = Curve::Strong;
        save(&path, &profiles);
        assert_eq!(load(&path), profiles);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A profile never asks for more monitors than a PC makes.
    #[test]
    fn too_many_monitors_are_cut() {
        let list = vec!["1920x1080"; 20].join(" ");
        let p = Profile::parse(&format!("monitors {list} *640x480\n")).unwrap();
        assert_eq!(p.sizes.len(), prostir_proto::MAX_MONITORS);
        assert_eq!(p.primary, 0, "the primary one was cut off");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let p = load(Path::new("/nonexistent/profiles.txt"));
        assert_eq!(p[0].sizes.len(), 3);
        assert!(p[1].settings.cinema);
        assert_eq!(p[2].settings.preset, Preset::Overhead);
    }
}
