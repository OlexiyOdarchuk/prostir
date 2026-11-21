//! Profiles: a whole setup (monitors, laptop screen, layout, background, mode)
//! switched with one press. Three slots with sensible starting points; any of them
//! can be overwritten with the current setup.

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
    /// is the one `desk` keeps in memory, kept before the cinema view replaced it.
    pub fn of(
        monitors: &[(ScreenId, u32, u32)],
        primary: Option<ScreenId>,
        mirror: bool,
        settings: Settings,
        live: layout::Arrangement,
        rows: &[Vec<ScreenId>],
        desk: &layout::Desk,
    ) -> Self {
        let arrangement = if settings.cinema {
            desk.arrangement().clone()
        } else {
            live
        };
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

    /// The profile's section of `profiles.txt` (after its `[slot]` line), added to
    /// `out`.
    fn write_text(&self, out: &mut String) {
        use std::fmt::Write as _;
        out.push_str("monitors ");
        for (i, (w, h)) in self.sizes.iter().enumerate() {
            let space = if i > 0 { " " } else { "" };
            let star = if i == usize::from(self.primary) {
                "*"
            } else {
                ""
            };
            let _ = write!(out, "{space}{star}{w}x{h}");
        }
        let mirror = if self.mirror { "on" } else { "off" };
        let _ = writeln!(out, "\nmirror {mirror}");
        for (i, p) in self.places.iter().enumerate() {
            let (a, q) = (p.position, p.rotation);
            let _ = writeln!(
                out,
                "place {i} {} {} {} {} {} {} {} {}",
                a.x, a.y, a.z, q.x, q.y, q.z, q.w, p.width
            );
        }
        // Older readers skip these lines: they are no settings of theirs.
        if !self.places.is_empty() {
            if let Some((eye, yaw)) = self.center {
                let _ = writeln!(out, "center {} {} {} {yaw}", eye.x, eye.y, eye.z);
                if let Some(r) = self.radius {
                    let _ = writeln!(out, "radius {r}");
                }
            }
            if !self.rows.is_empty() {
                out.push_str("rows ");
                for (n, row) in self.rows.iter().enumerate() {
                    if n > 0 {
                        out.push_str(" / ");
                    }
                    for (k, i) in row.iter().enumerate() {
                        let space = if k > 0 { " " } else { "" };
                        let _ = write!(out, "{space}{i}");
                    }
                }
                out.push('\n');
            }
        }
        self.settings.write_text(out);
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

/// The profiles `profiles.txt` holds: `[slot]` sections in one file; a slot it does
/// not have keeps its starting point.
pub fn parse(text: &str) -> [Profile; SLOTS] {
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

/// The text of `profiles.txt`, added to `out` (a buffer kept between saves).
pub fn write_text(out: &mut String, profiles: &[Profile; SLOTS]) {
    use std::fmt::Write as _;
    for (i, p) in profiles.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "[{i}]");
        p.write_text(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Curve;

    fn text_of(profiles: &[Profile; SLOTS]) -> String {
        let mut text = String::new();
        write_text(&mut text, profiles);
        text
    }

    /// The text is the one the app wrote before the files' writer took it over: a
    /// profile's lines, the settings after them, an empty line between sections.
    #[test]
    fn the_text_as_before() {
        let mut profiles: [Profile; SLOTS] = std::array::from_fn(Profile::default_for);
        profiles[0].places = vec![
            Placement {
                position: Vec3::new(-1.0, 0.5, -1.25),
                rotation: Quat::IDENTITY,
                width: 1.2,
            };
            3
        ];
        profiles[0].center = Some((Vec3::new(0.0, 1.5, 0.0), 0.25));
        profiles[0].radius = Some(1.3);
        profiles[0].rows = vec![vec![0, 1], vec![2]];
        let text = text_of(&profiles);
        let settings = Settings::default().to_text();
        let cinema = Profile::default_for(1).settings.to_text();
        let lying = Profile::default_for(2).settings.to_text();
        let place = "-1 0.5 -1.25 0 0 0 1 1.2";
        assert_eq!(
            text,
            format!(
                "[0]\nmonitors 2560x1440 *2560x1440 2560x1440\nmirror on\n\
                 place 0 {place}\nplace 1 {place}\nplace 2 {place}\n\
                 center 0 1.5 0 0.25\nradius 1.3\nrows 0 1 / 2\n{settings}\n\
                 [1]\nmonitors *3840x2160\nmirror off\n{cinema}\n\
                 [2]\nmonitors *2560x1440 2560x1440\nmirror off\n{lying}"
            )
        );
    }

    #[test]
    fn roundtrip() {
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
        let back = parse(&text_of(&profiles));
        assert_eq!(back[..2], profiles[..2]);
        for (a, b) in back[2].places.iter().zip(&profiles[2].places) {
            assert!(a.position.distance(b.position) < 1e-6 && (a.width - b.width).abs() < 1e-6);
        }
        assert_eq!(back[2].sizes, profiles[2].sizes);
        assert_eq!(back[2].center, profiles[2].center);
        assert_eq!(back[2].radius, profiles[2].radius);
        assert_eq!(back[2].rows, profiles[2].rows);
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
        let p = parse("");
        assert_eq!(p[0].sizes.len(), 3);
        assert!(p[1].settings.cinema);
        assert_eq!(p[2].settings.preset, Preset::Overhead);
    }
}
