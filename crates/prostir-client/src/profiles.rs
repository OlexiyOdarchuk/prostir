//! Profiles: a whole setup (monitors, laptop screen, layout, background, mode)
//! switched with one press. Three slots with sensible starting points; any of them
//! can be overwritten with the current setup.

use std::path::Path;

use glam::{Quat, Vec3};

use crate::layout::{self, Placement, ScreenId};
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
    /// The eye and the yaw `places` were laid out around, and the circle they stand
    /// on (as the desk arrangement keeps them, see `layout::Arrangement`). Profiles
    /// saved without them have places in the room as it was then.
    pub center: Option<(Vec3, f32)>,
    pub radius: Option<f32>,
    /// Where each monitor stood, by its place in `sizes`: all of them or none (a
    /// profile from before 0.2.0, or a starting point: the preset lays them out).
    pub places: Vec<Placement>,
    /// The rows the PC had, top to bottom and each left to right, by place in `sizes`
    /// (the laptop screen is not among them): which monitors stay neighbours while
    /// they are moved about (see `prostir_layout::rows`). Empty for a profile saved
    /// without them.
    pub rows: Vec<Vec<usize>>,
}

impl Profile {
    /// Slot 0: work, 1: cinema, 2: lying down.
    pub fn default_for(slot: usize) -> Self {
        let mut settings = Settings::default();
        let (sizes, primary, mirror) = match slot {
            1 => {
                settings.cinema = true;
                settings.background = Background::Black;
                (vec![(3840, 2160)], 0, false)
            }
            2 => {
                settings.preset = Preset::Overhead;
                (vec![(2560, 1440), (2560, 1440)], 0, false)
            }
            _ => (vec![(2560, 1440); 3], 1, true),
        };
        Self {
            sizes,
            primary,
            mirror,
            settings,
            center: None,
            radius: None,
            places: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// The setup as it stands: `monitors` (number, width, height) left to right, the
    /// `primary` one, whether the laptop screen is wanted, `settings`, and the desk
    /// arrangement `live` with the `rows` the PC has (by number). In cinema the desk
    /// is the one `desk` keeps, saved before the cinema view replaced it.
    pub fn of(
        monitors: &[(ScreenId, u32, u32)],
        primary: Option<ScreenId>,
        mirror: bool,
        settings: Settings,
        live: layout::Arrangement,
        rows: &[Vec<ScreenId>],
        desk: &layout::Desk,
    ) -> Self {
        let arrangement = if settings.cinema { desk.load() } else { live };
        let places: Option<Vec<Placement>> = monitors
            .iter()
            .map(|m| arrangement.placements.get(&m.0).copied())
            .collect();
        let index = |id: &ScreenId| monitors.iter().position(|m| m.0 == *id);
        let rows: Vec<Vec<usize>> = rows
            .iter()
            .map(|r| r.iter().filter_map(index).collect::<Vec<usize>>())
            .filter(|r| !r.is_empty())
            .collect();
        let placed = places.is_some();
        Self {
            primary: monitors
                .iter()
                .position(|m| Some(m.0) == primary)
                .unwrap_or(0) as u8,
            sizes: monitors.iter().map(|m| (m.1, m.2)).collect(),
            mirror,
            settings,
            center: arrangement.center.filter(|_| placed),
            radius: arrangement
                .radius
                .filter(|_| placed && arrangement.center.is_some()),
            places: places.unwrap_or_default(),
            rows: if placed { rows } else { Vec::new() },
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let mut sizes = Vec::new();
        let mut primary = 0;
        let mut mirror = false;
        let mut places: Vec<(usize, Placement)> = Vec::new();
        let (mut center, mut radius, mut rows) = (None, None, Vec::new());
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
                Some(("radius", v)) => {
                    radius = v
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|r| r.is_finite() && *r > 0.1);
                }
                Some(("rows", v)) => rows = parse_rows(v).unwrap_or_default(),
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
        // Rows of these monitors, each once, or none.
        let mut seen: Vec<usize> = rows.iter().flatten().copied().collect();
        seen.sort_unstable();
        let unique = seen.windows(2).all(|w| w[0] != w[1]);
        if places.is_empty() || !unique || seen.last().is_some_and(|&i| i >= sizes.len()) {
            rows.clear();
        }
        let center = center.filter(|_| !places.is_empty());
        (!sizes.is_empty()).then(|| Self {
            sizes,
            primary,
            mirror,
            settings: Settings::parse(text),
            center,
            radius: radius.filter(|_| center.is_some()),
            places,
            rows,
        })
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
        let mut places =
            self.places
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
        // Older readers skip these lines: they are no settings of theirs.
        if !self.places.is_empty() {
            if let Some((eye, yaw)) = self.center {
                let _ = writeln!(places, "center {} {} {} {yaw}", eye.x, eye.y, eye.z);
                if let Some(r) = self.radius {
                    let _ = writeln!(places, "radius {r}");
                }
            }
            if !self.rows.is_empty() {
                let rows: Vec<String> = self
                    .rows
                    .iter()
                    .map(|r| r.iter().map(usize::to_string).collect::<Vec<_>>().join(" "))
                    .collect();
                let _ = writeln!(places, "rows {}", rows.join(" / "));
            }
        }
        format!(
            "monitors {}\nmirror {}\n{places}{}",
            monitors.join(" "),
            if self.mirror { "on" } else { "off" },
            self.settings.to_text()
        )
    }
}

/// A profile's monitors once the PC made them: `made` pairs the numbers the PC gave
/// them with the profile's places, in the order of its sizes. Each stands around the
/// user's center `now` (eye and yaw) as it stood around `center`, the one the profile
/// was saved with, so whatever is worked out as seen from the eye (the rows the PC gets,
/// "Line up", "In front", the magnet) sees the profile as it was; a profile saved
/// without one has its monitors where they were in the room. And its `rows` (by place
/// in its sizes) by the PC's numbers.
pub fn standing(
    made: &[(ScreenId, Placement)],
    center: Option<(Vec3, f32)>,
    rows: &[Vec<usize>],
    now: (Vec3, f32),
) -> (Vec<(ScreenId, Placement)>, Vec<Vec<ScreenId>>) {
    let from = center.unwrap_or(now);
    let placed = made
        .iter()
        .map(|(id, p)| (*id, layout::carried(p, from, now)))
        .collect();
    let rows = rows
        .iter()
        .map(|r| r.iter().filter_map(|&i| made.get(i).map(|m| m.0)).collect())
        .filter(|r: &Vec<ScreenId>| !r.is_empty())
        .collect();
    (placed, rows)
}

/// `0 1 2 / 3 4`: rows of places in a profile's sizes.
fn parse_rows(v: &str) -> Option<Vec<Vec<usize>>> {
    v.split('/')
        .map(|r| r.split_whitespace().map(|i| i.parse().ok()).collect())
        .filter(|r: &Option<Vec<usize>>| r.as_ref().is_none_or(|r| !r.is_empty()))
        .collect()
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
#[expect(
    clippy::disallowed_methods,
    reason = "the core's own file access, until step 7.3 hands it to persist"
)]
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

#[expect(
    clippy::disallowed_methods,
    reason = "the core's own file access, until step 7.3 hands it to persist"
)]
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
#[expect(
    clippy::disallowed_methods,
    reason = "tests make the folders the code under test reads and writes in"
)]
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
        profiles[2].radius = Some(1.35);
        profiles[2].rows = vec![vec![0, 1], vec![2]];
        save(&path, &profiles);
        let back = load(&path);
        assert_eq!(back[..2], profiles[..2]);
        for (a, b) in back[2].places.iter().zip(&profiles[2].places) {
            assert!(a.position.distance(b.position) < 1e-6 && (a.width - b.width).abs() < 1e-6);
        }
        assert_eq!(back[2].sizes, profiles[2].sizes);
        assert_eq!(back[2].center, profiles[2].center);
        assert_eq!(back[2].radius, profiles[2].radius);
        assert_eq!(back[2].rows, profiles[2].rows);
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

    /// Rows are of the profile's monitors, each once, and only with its places; the
    /// circle only with the center.
    #[test]
    fn rows_and_radius_only_as_they_belong() {
        let places = "place 0 0 0 -1 0 0 0 1 1.2\nplace 1 1 0 -1 0 0 0 1 1.2\n";
        let parse = |rest: &str| {
            Profile::parse(&format!("monitors 2560x1440 2560x1440\n{places}{rest}")).unwrap()
        };
        assert_eq!(parse("rows 1 / 0\n").rows, vec![vec![1], vec![0]]);
        assert!(parse("rows 0 2\n").rows.is_empty(), "a monitor it has not");
        assert!(parse("rows 0 / 0 1\n").rows.is_empty(), "one twice");
        assert!(parse("rows 0 x\n").rows.is_empty());
        assert_eq!(parse("radius 1.3\n").radius, None, "no center");
        assert_eq!(parse("center 0 1 0 0\nradius 1.3\n").radius, Some(1.3));
        let p = Profile::parse("monitors 2560x1440\ncenter 0 1 0 0\nrows 0\n").unwrap();
        assert!(p.center.is_none() && p.rows.is_empty(), "no places");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let p = load(Path::new("/nonexistent/profiles.txt"));
        assert_eq!(p[0].sizes.len(), 3);
        assert!(p[1].settings.cinema);
        assert_eq!(p[2].settings.preset, Preset::Overhead);
    }
}
