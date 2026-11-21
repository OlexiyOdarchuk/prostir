//! Profiles: a whole setup (monitors, laptop screen, layout, background, mode)
//! switched with one press. Three slots with sensible starting points; any of them
//! can be overwritten with the current setup.

use std::path::Path;

use glam::{Quat, Vec3};

use crate::layout::{self, Placement};
use crate::settings::{Background, Preset, Settings};

pub const SLOTS: usize = 3;
/// Monitors in a profile: more than any PC makes (it answers with its own limit).
const MAX_SIZES: usize = 10;

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
    /// The eye and the yaw `places` were laid out around (as the desk arrangement keeps
    /// them, see `layout::Arrangement`). Profiles saved without one have places in
    /// the room as it was then.
    pub center: Option<(Vec3, f32)>,
    /// Where each monitor stood, by its place in `sizes`: all of them or none (a
    /// profile from before 0.2.0, or a starting point: the preset lays them out).
    pub places: Vec<Placement>,
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
                    center: None,
                    places: Vec::new(),
                }
            }
            2 => {
                settings.preset = Preset::Overhead;
                Self {
                    sizes: vec![(2560, 1440), (2560, 1440)],
                    primary: 0,
                    mirror: false,
                    settings,
                    center: None,
                    places: Vec::new(),
                }
            }
            _ => Self {
                sizes: vec![(2560, 1440); 3],
                primary: 1,
                mirror: true,
                settings,
                center: None,
                places: Vec::new(),
            },
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let mut sizes = Vec::new();
        let mut primary = 0;
        let mut mirror = false;
        let mut places: Vec<(usize, Placement)> = Vec::new();
        let mut center = None;
        for line in text.lines() {
            if let Some(c) = layout::parse_center(line) {
                center = Some(c);
                continue;
            }
            match line.split_once(' ') {
                Some(("place", v)) => {
                    if let Some(p) = parse_place(v) {
                        places.push(p);
                    }
                }
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
        // A file edited by hand may list more than any PC makes.
        sizes.truncate(MAX_SIZES);
        if usize::from(primary) >= sizes.len() {
            primary = 0;
        }
        // A place for every monitor, or none.
        places.sort_by_key(|p| p.0);
        places.dedup_by_key(|p| p.0);
        let places: Vec<Placement> = if places.iter().map(|p| p.0).eq(0..sizes.len()) {
            places.into_iter().map(|p| p.1).collect()
        } else {
            Vec::new()
        };
        (!sizes.is_empty()).then(|| Self {
            sizes,
            primary,
            mirror,
            settings: Settings::parse(text),
            center: center.filter(|_| !places.is_empty()),
            places,
        })
    }

    /// Its places as laid out around the origin facing -Z (as `layout::arrange` lays a
    /// preset out), to stand around the user the way they stood around the user when
    /// saved, wherever the user sits and faces now: rows, the desktop, "Line up" and "In
    /// front" then see them from the eye they were arranged around. A profile saved
    /// without its center is taken as laid out around `now`: its monitors come back
    /// where they were in the room.
    pub fn local_places(&self, now: (Vec3, f32)) -> Vec<Placement> {
        let from = self.center.unwrap_or(now);
        self.places
            .iter()
            .map(|p| layout::carried(p, from, (Vec3::ZERO, 0.0)))
            .collect()
    }

    fn to_text(&self) -> String {
        use std::fmt::Write as _;
        let monitors: Vec<String> = self
            .sizes
            .iter()
            .enumerate()
            .map(|(i, (w, h))| {
                let star = if i == self.primary as usize { "*" } else { "" };
                format!("{star}{w}x{h}")
            })
            .collect();
        let places = self
            .places
            .iter()
            .enumerate()
            .fold(String::new(), |mut places, (i, p)| {
                let (a, q) = (p.position, p.rotation);
                let _ = writeln!(
                    places,
                    "place {i} {} {} {} {} {} {} {} {}",
                    a.x, a.y, a.z, q.x, q.y, q.z, q.w, p.width
                );
                places
            });
        // Older readers skip the line: it is no setting of theirs.
        let center = self
            .center
            .filter(|_| !self.places.is_empty())
            .map_or(String::new(), |(eye, yaw)| {
                format!("center {} {} {} {yaw}\n", eye.x, eye.y, eye.z)
            });
        format!(
            "monitors {}\nmirror {}\n{center}{places}{}",
            monitors.join(" "),
            if self.mirror { "on" } else { "off" },
            self.settings.to_text()
        )
    }
}

/// `INDEX x y z qx qy qz qw width`.
fn parse_place(v: &str) -> Option<(usize, Placement)> {
    let mut words = v.split_whitespace();
    let i = words.next()?.parse().ok()?;
    let n: Vec<f32> = words.map(|w| w.parse().ok()).collect::<Option<_>>()?;
    let [x, y, z, qx, qy, qz, qw, width] = n[..] else {
        return None;
    };
    let p = Placement {
        position: Vec3::new(x, y, z),
        rotation: Quat::from_xyzw(qx, qy, qz, qw).normalize(),
        width,
    };
    (p.position.is_finite() && p.rotation.is_finite() && width.is_finite() && width > 0.0)
        .then_some((i, p))
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
        profiles[2].places = (0..3)
            .map(|i| Placement {
                position: Vec3::new(i as f32, 0.5, -1.0),
                rotation: Quat::from_rotation_y(0.25),
                width: 1.2,
            })
            .collect();
        profiles[2].center = Some((Vec3::new(0.3, 1.2, -0.4), -0.7));
        save(&path, &profiles);
        let back = load(&path);
        assert_eq!(back[..2], profiles[..2]);
        for (a, b) in back[2].places.iter().zip(&profiles[2].places) {
            assert!(a.position.distance(b.position) < 1e-6 && (a.width - b.width).abs() < 1e-6);
        }
        assert_eq!(back[2].sizes, profiles[2].sizes);
        assert_eq!(back[2].center, profiles[2].center);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A profile never asks for more monitors than any PC makes.
    #[test]
    fn too_many_monitors_are_cut() {
        let list = vec!["1920x1080"; 20].join(" ");
        let p = Profile::parse(&format!("monitors {list} *640x480\n")).unwrap();
        assert_eq!(p.sizes.len(), MAX_SIZES);
        assert_eq!(p.primary, 0, "the primary one was cut off");
    }

    /// Profiles saved by 0.1 (no places) still load, laid out by their preset; a
    /// profile missing a place has none.
    #[test]
    fn old_profiles_load() {
        let p = Profile::parse("monitors 2560x1440 *1440x2560\nmirror on\npreset wall\n").unwrap();
        assert!(p.places.is_empty());
        assert_eq!(p.settings.preset, Preset::Wall);
        let p =
            Profile::parse("monitors 2560x1440 2560x1440\nplace 0 0 0 -1 0 0 0 1 1.2\n").unwrap();
        assert!(p.places.is_empty(), "one of two places");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let p = load(Path::new("/nonexistent/profiles.txt"));
        assert_eq!(p[0].sizes.len(), 3);
        assert!(p[1].settings.cinema);
        assert_eq!(p[2].settings.preset, Preset::Overhead);
    }
}
