//! Profiles: saved arrangements, switched to with one press. A profile keeps which
//! monitors there are (their sizes and scales, the primary one, the PC's own displays
//! shown) and where they stand, nothing more: cinema, the background, the curve and
//! the preset are settings over whatever profile is on, and stay as they are when one
//! is switched to. Three slots, empty until the setup as it stands is saved into one.

#![forbid(unsafe_code)]

use glam::Vec3;
use prostir_layout::space::Frame;
use prostir_proto::state::Mode;

use crate::ids::{DisplayKey, PcId};
use crate::layout::{self, Arrangement, Key, Placement};
use crate::persist::{self, LoadError};

pub const SLOTS: usize = 3;
/// Monitors in a profile: more than any PC makes (it answers with its own limit).
const MAX_MONITORS: usize = 10;

/// The slots: a profile saved in each, or none.
pub type Slots = [Option<Profile>; SLOTS];

/// No profile saved in any slot.
pub fn empty() -> Slots {
    std::array::from_fn(|_| None)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// The monitors, left to right: their sizes and scales.
    pub monitors: Vec<Mode>,
    /// Index into `monitors`.
    pub primary: u8,
    /// The displays of the PCs themselves that were shown (a laptop's panel, say).
    pub shown: Vec<DisplayKey>,
    /// Where each monitor stood, by its place in `monitors`, in the profile's own
    /// frame: the eye at the origin, straight ahead -Z, tilted up by `tilt`. All of
    /// them or none (saved before every monitor had a place: the preset lays them out).
    pub places: Vec<Placement>,
    /// How far the frame is tilted up, in radians: lying down the arrangement is a
    /// band overhead, seen from below (see `layout::frame`).
    pub tilt: f32,
    /// The circle the monitors stand on.
    pub radius: f32,
    /// The rows the PC had, top to bottom and each left to right, by place in
    /// `monitors` (the PC's own displays are not among them): which monitors stay
    /// neighbours while they are moved about (see `prostir_layout::rows`). Only with
    /// places.
    pub rows: Vec<Vec<usize>>,
}

impl Profile {
    /// The setup as it stands: `monitors` (number, size and scale) left to right, the
    /// `primary` one, the PC's own displays `shown`, the arrangement `live` with the
    /// `rows` the PC has (by number), seen from its center tilted up by `tilt`. In
    /// cinema `desk` is the desk arrangement kept in memory before the cinema view
    /// replaced it, and that is what is saved.
    pub fn of<K: Key>(
        monitors: &[(K, Mode)],
        primary: Option<K>,
        shown: Vec<DisplayKey>,
        live: &Arrangement<K>,
        desk: Option<&Arrangement<K>>,
        tilt: f32,
        rows: &[Vec<K>],
    ) -> Self {
        let arrangement = desk.unwrap_or(live);
        let (eye, yaw) = arrangement.center.unwrap_or((Vec3::ZERO, 0.0));
        let frame = Frame::tilted(eye, yaw, tilt);
        let places: Option<Vec<Placement>> = monitors
            .iter()
            .map(|m| arrangement.placements.get(&m.0).map(|p| frame.unplace(*p)))
            .collect();
        let index = |id: &K| monitors.iter().position(|m| m.0 == *id);
        let rows = match places {
            Some(_) => rows
                .iter()
                .map(|r| r.iter().filter_map(index).collect::<Vec<usize>>())
                .filter(|r| !r.is_empty())
                .collect(),
            None => Vec::new(),
        };
        Self {
            primary: primary
                .and_then(|p| index(&p))
                .and_then(|i| u8::try_from(i).ok())
                .unwrap_or(0),
            monitors: monitors.iter().map(|m| m.1).collect(),
            shown,
            places: places.unwrap_or_default(),
            tilt,
            radius: arrangement.radius.unwrap_or(layout::RADIUS),
            rows,
        }
    }

    /// The profile's section of `profiles.txt` (after its `[slot]` line), added to
    /// `out`.
    fn write_text(&self, out: &mut String) {
        use std::fmt::Write as _;
        out.push_str("monitors");
        for (i, m) in self.monitors.iter().enumerate() {
            let star = if i == usize::from(self.primary) {
                "*"
            } else {
                ""
            };
            let _ = write!(out, " {star}{}x{}", m.w, m.h);
            if m.scale != 100 {
                let _ = write!(out, "@{}", m.scale);
            }
        }
        out.push('\n');
        for k in &self.shown {
            let _ = writeln!(out, "shown {k}");
        }
        for (i, p) in self.places.iter().enumerate() {
            let _ = write!(out, "place {i}");
            layout::write_place(out, p, None);
        }
        if !self.places.is_empty() {
            let _ = writeln!(out, "frame {} {}", self.tilt, self.radius);
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
}

/// PC `pc` was forgotten: no profile shows its displays any more. True when one did,
/// and `profiles.txt` is to be written.
pub fn forget_pc(profiles: &mut Slots, pc: PcId) -> bool {
    let mut changed = false;
    for p in profiles.iter_mut().flatten() {
        let before = p.shown.len();
        p.shown.retain(|k| k.pc != pc);
        changed |= p.shown.len() != before;
    }
    changed
}

/// A profile's lines as they are read, before they are checked together.
#[derive(Default)]
struct Draft {
    monitors: Option<(Vec<Mode>, u8)>,
    shown: Vec<DisplayKey>,
    places: Vec<(usize, Placement)>,
    frame: Option<(f32, f32)>,
    rows: Option<Vec<Vec<usize>>>,
}

impl Draft {
    /// Takes one line of the section; `None` for one this version does not write.
    fn line(&mut self, line: &str) -> Option<()> {
        let (word, rest) = line.split_once(' ')?;
        match word {
            "monitors" if self.monitors.is_none() => self.monitors = Some(parse_monitors(rest)?),
            "shown" => {
                let k = DisplayKey::parse(rest)?;
                if self.shown.contains(&k) {
                    return None;
                }
                self.shown.push(k);
            }
            "place" => {
                let (i, p) = rest.split_once(' ')?;
                let i: usize = i.parse().ok()?;
                if self.places.iter().any(|p| p.0 == i) {
                    return None;
                }
                self.places.push((i, layout::parse_place(p, false)?.0));
            }
            "frame" if self.frame.is_none() => {
                let (tilt, radius) = rest.split_once(' ')?;
                let tilt: f32 = tilt.parse().ok()?;
                let upright = -std::f32::consts::FRAC_PI_2..=std::f32::consts::FRAC_PI_2;
                if !upright.contains(&tilt) {
                    return None;
                }
                self.frame = Some((tilt, layout::parse_radius(radius)?));
            }
            "rows" if self.rows.is_none() => self.rows = Some(parse_rows(rest)?),
            _ => return None,
        }
        Some(())
    }

    /// The profile its lines make, if they make one: its monitors, a place for each
    /// with the frame they are in or none and no frame, rows only with places and of
    /// its monitors, each once.
    fn profile(mut self) -> Option<Profile> {
        let (monitors, primary) = self.monitors?;
        let n = monitors.len();
        self.places.sort_by_key(|p| p.0);
        if !self.places.is_empty() && !self.places.iter().map(|p| p.0).eq(0..n) {
            return None;
        }
        let rows = self.rows.unwrap_or_default();
        let mut seen: Vec<usize> = rows.iter().flatten().copied().collect();
        seen.sort_unstable();
        if seen.windows(2).any(|w| w[0] == w[1]) || seen.last().is_some_and(|&i| i >= n) {
            return None;
        }
        let (tilt, radius) = match (self.frame, self.places.is_empty()) {
            (Some(frame), false) => frame,
            (None, true) if rows.is_empty() => (0.0, layout::RADIUS),
            _ => return None,
        };
        Some(Profile {
            monitors,
            primary,
            shown: self.shown,
            places: self.places.into_iter().map(|p| p.1).collect(),
            tilt,
            radius,
            rows,
        })
    }
}

/// `2560x1440 *2560x1440@150 1440x2560`: the monitors left to right, the primary one
/// starred, a scale other than 100 % after `@`.
fn parse_monitors(text: &str) -> Option<(Vec<Mode>, u8)> {
    let mut monitors = Vec::new();
    let mut primary = None;
    for (i, m) in text.split(' ').enumerate() {
        let m = match m.strip_prefix('*') {
            Some(m) if primary.is_none() => {
                primary = Some(u8::try_from(i).ok()?);
                m
            }
            Some(_) => return None,
            None => m,
        };
        let (size, scale) = match m.split_once('@') {
            Some((size, scale)) => (size, scale.parse::<u16>().ok().filter(|s| *s > 0)?),
            None => (m, 100),
        };
        let (w, h) = size.split_once('x')?;
        monitors.push(Mode {
            w: w.parse().ok().filter(|w| *w > 0)?,
            h: h.parse().ok().filter(|h| *h > 0)?,
            scale,
        });
    }
    (monitors.len() <= MAX_MONITORS).then_some(())?;
    Some((monitors, primary?))
}

/// `0 1 2 / 3 4`: rows of places in a profile's monitors.
fn parse_rows(text: &str) -> Option<Vec<Vec<usize>>> {
    text.split(" / ")
        .map(|r| {
            r.split(' ')
                .map(|i| i.parse().ok())
                .collect::<Option<Vec<usize>>>()
        })
        .collect()
}

/// A profile's monitors once the PC made them: `made` pairs the numbers the PC gave
/// them with the profile's places, in the order of its monitors. Each stands around
/// the user's center `now` (eye and yaw), tilted up by `tilt`, as it stood around the
/// center it was saved with, so whatever is worked out as seen from the eye (the rows
/// the PC gets, "Line up", "In front", the magnet) sees the profile as it was. And
/// its `rows` (by place in its monitors) by the PC's numbers.
pub fn standing<K: Key>(
    made: &[(K, Placement)],
    tilt: f32,
    rows: &[Vec<usize>],
    now: (Vec3, f32),
) -> (Vec<(K, Placement)>, Vec<Vec<K>>) {
    let frame = Frame::tilted(now.0, now.1, tilt);
    let placed = made
        .iter()
        .map(|(id, p)| {
            let mut p = frame.place(*p);
            p.rotation = p.rotation.normalize();
            (*id, p)
        })
        .collect();
    let rows = rows
        .iter()
        .map(|r| r.iter().filter_map(|&i| made.get(i).map(|m| m.0)).collect())
        .filter(|r: &Vec<K>| !r.is_empty())
        .collect();
    (placed, rows)
}

/// The profiles `profiles.txt` holds, by slot:
///
/// ```text
/// format 2
///
/// [0]
/// monitors 2560x1440 *2560x1440@150 1440x2560
/// shown 0123456789abcdef:100
/// place 0 -1.1 0 -0.9 0 0.38 0 0.92 0.77
/// place 1 0 0 -1.2 0 0 0 1 0.77
/// place 2 1 0 -0.95 0 -0.36 0 0.93 0.43
/// frame 0 1.3
/// rows 0 1 2
/// ```
///
/// A slot that is not there is empty. A file this version does not write so is not
/// read at all.
pub fn parse(text: &str) -> Result<Slots, LoadError> {
    let mut out = empty();
    let mut current: Option<(usize, usize, Draft)> = None;
    let finish = |section: Option<(usize, usize, Draft)>, out: &mut Slots| {
        if let Some((slot, n, draft)) = section {
            let profile = draft.profile().ok_or(LoadError::Line(n))?;
            if let Some(o) = out.get_mut(slot) {
                *o = Some(profile);
            }
        }
        Ok::<(), LoadError>(())
    };
    for (n, line) in persist::body(text)? {
        if line.is_empty() {
            continue;
        }
        if let Some(slot) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            finish(current.take(), &mut out)?;
            let slot = slot
                .parse::<usize>()
                .ok()
                .filter(|i| out.get(*i).is_some_and(Option::is_none))
                .ok_or(LoadError::Line(n))?;
            current = Some((slot, n, Draft::default()));
            continue;
        }
        match &mut current {
            Some((_, _, draft)) => draft.line(line).ok_or(LoadError::Line(n))?,
            None => return Err(LoadError::Line(n)),
        }
    }
    finish(current, &mut out)?;
    Ok(out)
}

/// The text of `profiles.txt` (see [`parse`]), added to `out` (a buffer kept between
/// saves).
pub fn write_text(out: &mut String, profiles: &Slots) {
    use std::fmt::Write as _;
    persist::write_format(out);
    for (i, p) in profiles.iter().enumerate() {
        if let Some(p) = p {
            let _ = writeln!(out, "\n[{i}]");
            p.write_text(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Quat;
    use prostir_proto::state::DisplayId;

    fn text_of(profiles: &Slots) -> String {
        let mut text = String::new();
        write_text(&mut text, profiles);
        text
    }

    fn place(x: f32) -> Placement {
        Placement {
            position: Vec3::new(x, 0.5, -1.25),
            rotation: Quat::IDENTITY,
            width: 1.2,
        }
    }

    fn sample() -> Profile {
        let laptop = DisplayKey::new(PcId([0xab; 8]), DisplayId(100));
        Profile {
            monitors: vec![
                Mode::new(2560, 1440),
                Mode {
                    scale: 150,
                    ..Mode::new(2560, 1440)
                },
                Mode::new(1440, 2560),
            ],
            primary: 1,
            shown: vec![laptop],
            places: vec![place(-1.0), place(0.0), place(1.0)],
            tilt: 0.0,
            radius: 1.3,
            rows: vec![vec![0, 1], vec![2]],
        }
    }

    /// The text of the file: the format line, then each saved slot after an empty
    /// line; a slot never saved is not there.
    #[test]
    fn the_text_of_the_file() {
        let mut profiles = empty();
        assert_eq!(text_of(&profiles), "format 2\n");
        profiles[1] = Some(sample());
        let place = "-1 0.5 -1.25 0 0 0 1 1.2";
        assert_eq!(
            text_of(&profiles),
            format!(
                "format 2\n\n[1]\nmonitors 2560x1440 *2560x1440@150 1440x2560\n\
                 shown abababababababab:100\n\
                 place 0 {place}\nplace 1 0 0.5 -1.25 0 0 0 1 1.2\nplace 2 1 0.5 -1.25 0 0 0 1 1.2\n\
                 frame 0 1.3\nrows 0 1 / 2\n"
            )
        );
    }

    #[test]
    fn roundtrip() {
        let mut profiles = empty();
        profiles[0] = Some(sample());
        profiles[2] = Some(Profile {
            monitors: vec![Mode::new(1920, 1080)],
            primary: 0,
            shown: Vec::new(),
            places: (0..1)
                .map(|i| Placement {
                    position: Vec3::new(i as f32, 0.5, -1.0),
                    rotation: Quat::from_rotation_y(0.25),
                    width: 1.2,
                })
                .collect(),
            tilt: layout::OVERHEAD_PITCH,
            radius: 1.35,
            rows: Vec::new(),
        });
        let back = parse(&text_of(&profiles)).unwrap();
        assert_eq!(back[0], profiles[0]);
        assert_eq!(back[1], None);
        let (a, b) = (back[2].as_ref().unwrap(), profiles[2].as_ref().unwrap());
        assert!(a.places[0].rotation.angle_between(b.places[0].rotation) < 1e-4);
        assert_eq!((a.tilt, a.radius), (b.tilt, b.radius));
        // A profile saved before its monitors had places: none, no frame, no rows.
        let mut unplaced = sample();
        unplaced.places.clear();
        unplaced.rows.clear();
        unplaced.radius = layout::RADIUS;
        let text = text_of(&[Some(unplaced.clone()), None, None]);
        assert!(!text.contains("frame"));
        assert_eq!(parse(&text).unwrap()[0], Some(unplaced));
    }

    /// Files of 0.1 and of the builds before this format are not read; neither is a
    /// section this version does not write: a monitor list without its primary or
    /// with two, more monitors than any PC makes, a place missing or twice, places
    /// without their frame or a frame without places, rows of monitors it has not
    /// or with one twice, a setting, a slot twice or out of range, a line before the
    /// first slot.
    #[test]
    fn what_is_not_read() {
        let old = "[0]\nmonitors 2560x1440 *1440x2560\nmirror on\npreset wall\n";
        assert_eq!(parse(old).err(), Some(LoadError::Format));
        assert_eq!(parse("").err(), Some(LoadError::Format));
        let place = "0 0 -1 0 0 0 1 1.2";
        let two = format!("monitors 2560x1440 *2560x1440\nplace 0 {place}\nplace 1 {place}\n");
        let good = format!("{two}frame 0 1.3\n");
        assert!(parse(&format!("format 2\n[0]\n{good}")).unwrap()[0].is_some());
        let many = ["1920x1080"; 11].join(" ");
        for bad in [
            "monitors 2560x1440 2560x1440\n".to_owned(),
            "monitors *2560x1440 *2560x1440\n".to_owned(),
            format!("monitors *{many}\n"),
            "monitors *2560x1440@0\n".to_owned(),
            "monitors *0x1440\n".to_owned(),
            "shown 3\n".to_owned(),
            format!("monitors 2560x1440 *2560x1440\nplace 0 {place}\nframe 0 1.3\n"),
            format!("monitors 2560x1440 *2560x1440\nplace 0 {place}\nplace 0 {place}\n"),
            two.clone(),
            "monitors *2560x1440\nframe 0 1.3\n".to_owned(),
            format!("{good}rows 0 2\n"),
            format!("{good}rows 0 / 0 1\n"),
            format!("{good}rows 0 x\n"),
            "monitors *2560x1440\nrows 0\n".to_owned(),
            format!("{good}frame 0 1.3\n"),
            format!("{two}frame 2 1.3\n"),
            format!("{good}preset wall\n"),
            format!("{good}mirror on\n"),
            format!("{good}[0]\n{good}"),
            format!("{good}[3]\n{good}"),
        ] {
            let text = format!("format 2\n[0]\n{bad}");
            assert!(matches!(parse(&text), Err(LoadError::Line(_))), "{bad:?}");
        }
        assert_eq!(
            parse(&format!("format 2\n{good}")).err(),
            Some(LoadError::Line(2)),
            "before any slot"
        );
    }

    /// A PC forgotten leaves the profiles that showed its displays, and only its own.
    #[test]
    fn a_forgotten_pc_leaves_the_profiles() {
        let mut profiles = empty();
        let mut p = sample();
        let other = DisplayKey::new(PcId([1; 8]), DisplayId(100));
        p.shown.push(other);
        profiles[2] = Some(p);
        assert!(forget_pc(&mut profiles, PcId([0xab; 8])));
        assert_eq!(profiles[2].as_ref().unwrap().shown, [other]);
        assert!(!forget_pc(&mut profiles, PcId([0xab; 8])));
    }

    /// Saved in its own frame, a profile comes back around wherever the user is
    /// then as it stood around them when it was saved, tilted or not; in cinema the
    /// desk arrangement is saved, not the cinema view.
    #[test]
    fn a_profile_stands_around_the_user_as_saved() {
        let saved_at = (Vec3::new(0.2, 1.2, -0.1), 0.4);
        let later = (Vec3::new(-0.5, 1.0, 0.3), -1.1);
        let monitors = [(7_u32, Mode::new(2560, 1440)), (9, Mode::new(1440, 2560))];
        let room = [(7, place(-0.4)), (9, place(0.6))];
        let live = Arrangement {
            placements: room.into_iter().collect(),
            sizes: std::collections::HashMap::new(),
            center: Some(saved_at),
            radius: Some(1.4),
        };
        for tilt in [0.0, layout::OVERHEAD_PITCH] {
            let p = Profile::of(
                &monitors,
                Some(9),
                Vec::new(),
                &live,
                None,
                tilt,
                &[vec![7, 9]],
            );
            assert_eq!((p.primary, p.radius, p.tilt), (1, 1.4, tilt));
            assert_eq!(p.rows, vec![vec![0, 1]]);
            let made: Vec<(u32, Placement)> = [7, 9].into_iter().zip(p.places.clone()).collect();
            let (back, rows) = standing(&made, tilt, &p.rows, saved_at);
            assert_eq!(rows, vec![vec![7, 9]]);
            for ((_, b), (_, r)) in back.iter().zip(&room) {
                assert!(b.position.distance(r.position) < 1e-5, "{tilt}");
                assert!(b.rotation.angle_between(r.rotation) < 1e-4);
            }
            // Elsewhere: the same as seen from the eye.
            let (moved, _) = standing(&made, tilt, &p.rows, later);
            let seen =
                |q: &Placement, at: (Vec3, f32)| Frame::upright(at.0, at.1).unplace(*q).position;
            for ((_, m), (_, r)) in moved.iter().zip(&room) {
                assert!(seen(m, later).distance(seen(r, saved_at)) < 1e-5);
            }
        }
        // In cinema: the desk.
        let view = Arrangement {
            placements: std::iter::once((7, place(0.0))).collect(),
            ..live.clone()
        };
        let p = Profile::of(&monitors, None, Vec::new(), &view, Some(&live), 0.0, &[]);
        assert_eq!((p.places.len(), p.primary), (2, 0));
        // Not every monitor placed: no places, no rows.
        let p = Profile::of(&monitors, None, Vec::new(), &view, None, 0.0, &[vec![7]]);
        assert!(p.places.is_empty() && p.rows.is_empty());
    }
}
