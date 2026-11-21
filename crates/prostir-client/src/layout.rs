//! Where monitors float in the room, how controllers move them, and the files of the
//! arrangement.
//!
//! How the monitors are arranged (the presets, a new monitor, "Line up"), the rows they
//! make and the PC desktop made of those is `prostir_layout`, the code the PC runs too;
//! here the headset's state goes in and out of it.

use std::cell::RefCell;
use std::collections::HashMap;

use glam::{Quat, Vec2, Vec3};
use prostir_layout::circle::{self, Limits};
use prostir_layout::desk::{self, Spot};
use prostir_layout::space::{Frame, Monitor};
use prostir_layout::view::Room;
use prostir_layout::{place, rows, tidy};
use prostir_proto::state::{DisplayId, Placed, Row};

use crate::ids::{DisplayKey, Thing};
use crate::persist::{self, LoadError};
use crate::settings::Preset;

pub use prostir_layout::circle::{OVERHEAD_PITCH, RADIUS};

/// What the arrangement's rules take a monitor's number to be: a display of a PC in
/// the headset ([`DisplayKey`]), any number in the tests. The rules never look into it.
pub trait Key: Copy + Ord + std::hash::Hash {}

impl<K: Copy + Ord + std::hash::Hash> Key for K {}
pub use prostir_layout::space::{METRES_PER_PIXEL, Placement, facing};

/// Lines `value` (half size `half`) up with a neighbour at `other` (half size
/// `other_half`) only when it is all but lined up already: centres, or first or last
/// edges, within 3 cm. Anything else stays exactly where the user put it.
fn align(value: f32, half: f32, other: f32, other_half: f32) -> f32 {
    const NEAR: f32 = 0.03;
    let options = [other, other + other_half - half, other - other_half + half];
    options
        .into_iter()
        .filter(|o| (o - value).abs() <= NEAR)
        .min_by(|a, b| (a - value).abs().total_cmp(&(b - value).abs()))
        .unwrap_or(value)
}

/// How monitor `m` stands next to `n`, seen from `eye`: projected onto `n`'s plane.
#[derive(Debug, Clone, Copy)]
struct Beside {
    /// Gap in `n`'s plane (metres; 0 when they overlap).
    gap: f32,
    /// Beside `n` (else above or below it).
    across: bool,
    /// Which side: 1 right or above, -1 left or below.
    side: f32,
    /// How far along that edge of `n` the centre of `m` is (metres, in `n`'s plane).
    along: f32,
}

/// The corners of a flat monitor `p`, `h` metres high.
fn corners(p: &Placement, h: f32) -> [Vec3; 4] {
    let (x, y) = (p.width / 2.0, h / 2.0);
    [(-x, -y), (x, -y), (x, y), (-x, y)]
        .map(|(a, b)| p.position + p.rotation * Vec3::new(a, b, 0.0))
}

/// `m` (`mh` high) as seen from `eye` against `n` (`nh` high). `None` when `m` is not
/// in front of `n`'s plane from there (too far around to be its neighbour).
fn beside(n: &Placement, nh: f32, m: &Placement, mh: f32, eye: Vec3) -> Option<Beside> {
    let inv = n.rotation.inverse();
    let eye_l = inv * (eye - n.position);
    if eye_l.z <= 0.05 {
        return None;
    }
    let (mut lo, mut hi) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
    let mut seen = 0;
    for c in corners(m, mh) {
        let c = inv * (c - n.position);
        // Along the line of sight from the eye through the corner, onto n's plane. A
        // corner far around (on a wide arc) does not reach the plane: the near ones
        // are what count.
        let denom = eye_l.z - c.z;
        if denom <= 0.05 {
            continue;
        }
        let at = eye_l + (c - eye_l) * (eye_l.z / denom);
        lo = lo.min(at.truncate());
        hi = hi.max(at.truncate());
        seen += 1;
    }
    if seen < 2 {
        return None;
    }
    let (nw2, nh2) = (n.width / 2.0, nh / 2.0);
    let gx = (lo.x - nw2).max(-nw2 - hi.x);
    let gy = (lo.y - nh2).max(-nh2 - hi.y);
    let across = gx > gy;
    let mid = (lo + hi) / 2.0;
    let offset = inv * (m.position - n.position);
    Some(Beside {
        gap: gx.max(gy).max(0.0),
        across,
        side: if (if across { mid.x } else { mid.y }) >= 0.0 {
            1.0
        } else {
            -1.0
        },
        along: if across { offset.y } else { offset.x },
    })
}

/// `m` (`mw` × `mh`) joined to `n` (`nh` high) along one of its edges, like a hinge:
/// the shared edge is one line in the room, and `m` turns about it to face `eye`, so a
/// monitor beside `n` curves around the user and one above or below tilts towards
/// them like a laptop lid. `along`: where on that edge (see [`Beside::along`]).
fn hinge(n: &Placement, nh: f32, mw: f32, mh: f32, b: &Beside, eye: Vec3) -> Placement {
    let eye_l = n.rotation.inverse() * (eye - n.position);
    let (pivot, arm) = if b.across {
        (
            Vec3::new(b.side * n.width / 2.0, b.along, 0.0),
            Vec3::new(b.side * mw / 2.0, 0.0, 0.0),
        )
    } else {
        (
            Vec3::new(b.along, b.side * nh / 2.0, 0.0),
            Vec3::new(0.0, b.side * mh / 2.0, 0.0),
        )
    };
    let turn = |a: f32| {
        if b.across {
            Quat::from_rotation_y(a)
        } else {
            Quat::from_rotation_x(a)
        }
    };
    // The angle depends on where the centre ends up and the other way round.
    let mut a = 0.0f32;
    for _ in 0..8 {
        let d = eye_l - (pivot + turn(a) * arm);
        a = if b.across {
            d.x.atan2(d.z)
        } else {
            (-d.y).atan2(d.z)
        };
    }
    let r = turn(a);
    Placement {
        position: n.position + n.rotation * (pivot + r * arm),
        rotation: (n.rotation * r).normalize(),
        width: mw,
    }
}

/// A monitor of the arrangement: placement, height, width in pixels.
type Part<K> = (K, Placement, f32, f32);

fn parts<K: Key>(placements: &HashMap<K, Placement>, monitors: &[(K, u32, u32)]) -> Vec<Part<K>> {
    monitors
        .iter()
        .filter_map(|&(id, w, h)| {
            let p = *placements.get(&id)?;
            Some((id, p, p.width * h as f32 / w.max(1) as f32, w.max(1) as f32))
        })
        .collect()
}

/// The neighbour `m` would join among `others`, and how: the nearest as seen from
/// `eye` (between two it touches equally, the one its centre is closer to).
fn nearest<K: Key>(m: &Part<K>, others: &[Part<K>], eye: Vec3) -> Option<(Part<K>, Beside)> {
    others
        .iter()
        .filter(|o| o.0 != m.0)
        .filter_map(|o| Some((*o, beside(&o.1, o.2, &m.1, m.2, eye)?)))
        .min_by(|(a, ba), (b, bb)| {
            ba.gap.total_cmp(&bb.gap).then(
                a.1.position
                    .distance(m.1.position)
                    .total_cmp(&b.1.position.distance(m.1.position)),
            )
        })
}

/// Where along `n`'s edge `m` goes: where the user put it (lined up only when it all
/// but is), keeping some of the edges shared so the mouse can cross.
fn settle_along<K: Key>(b: &Beside, n: &Part<K>, mw: f32, mh: f32) -> f32 {
    let (half_m, half_n) = if b.across {
        (mh / 2.0, n.2 / 2.0)
    } else {
        (mw / 2.0, n.1.width / 2.0)
    };
    let shared = 0.15 * (2.0 * half_m).min(2.0 * half_n);
    align(b.along, half_m, 0.0, half_n).clamp(-(half_n + half_m - shared), half_n + half_m - shared)
}

/// Where a monitor being let go of would stick: the monitor it joins and the edge.
#[derive(Debug, Clone, Copy)]
pub struct Stick<K> {
    pub placement: Placement,
    pub neighbour: K,
    /// Side by side (else above or below).
    pub across: bool,
    /// Which side of the neighbour: 1 right or above, -1 left or below.
    pub side: f32,
}

/// The magnet for one monitor: let go almost against another (within 15 % of a
/// monitor, as seen from `eye`) and at about the same distance, where it would stick
/// to it, edge to edge, where along the edge it is (see [`hinge`]); `None` otherwise.
pub fn stick<K: Key>(
    placements: &HashMap<K, Placement>,
    monitors: &[(K, u32, u32)],
    eye: Vec3,
    id: K,
) -> Option<Stick<K>> {
    let all = parts(placements, monitors);
    let m = *all.iter().find(|m| m.0 == id)?;
    let (n, b) = nearest(&m, &all, eye)?;
    if b.gap >= 0.15 * m.1.width.min(n.1.width) {
        return None;
    }
    let b = Beside {
        along: settle_along(&b, &n, m.1.width, m.2),
        ..b
    };
    let placement = hinge(&n.1, n.2, m.1.width, m.2, &b, eye);
    // Pulled much nearer or pushed away on purpose: it stays there.
    let depth = m.1.position.distance(eye) / placement.position.distance(eye).max(0.1);
    if !(0.7..=1.45).contains(&depth) {
        return None;
    }
    Some(Stick {
        placement,
        neighbour: n.0,
        across: b.across,
        side: b.side,
    })
}

/// The magnet on a wall: like [`stick`], but in the wall's plane (monitors there are
/// flat in one plane, facing the same way).
pub fn stick_on_wall<K: Key>(
    placements: &HashMap<K, Placement>,
    monitors: &[(K, u32, u32)],
    center: (Vec3, f32),
    id: K,
) -> Option<Stick<K>> {
    let unturn = Quat::from_rotation_y(-center.1);
    let turn = Quat::from_rotation_y(center.1);
    // (id, local centre, width, height)
    let all: Vec<(K, Vec3, f32, f32)> = monitors
        .iter()
        .filter_map(|&(oid, w, h)| {
            let p = placements.get(&oid)?;
            Some((
                oid,
                unturn * (p.position - center.0),
                p.width,
                p.width * h as f32 / w.max(1) as f32,
            ))
        })
        .collect();
    let (_, mut at, mw, mh) = *all.iter().find(|m| m.0 == id)?;
    let gaps = |a: Vec3, aw: f32, ah: f32, b: Vec3, bw: f32, bh: f32| {
        (
            (a.x - b.x).abs() - (aw + bw) / 2.0,
            (a.y - b.y).abs() - (ah + bh) / 2.0,
        )
    };
    let sep = |o: &(K, Vec3, f32, f32)| {
        let (gx, gy) = gaps(at, mw, mh, o.1, o.2, o.3);
        (gx.max(gy).max(0.0), at.distance(o.1))
    };
    let others: Vec<(K, Vec3, f32, f32)> = all.iter().filter(|o| o.0 != id).copied().collect();
    let n = *others.iter().min_by(|a, b| {
        let (sa, da) = sep(a);
        let (sb, db) = sep(b);
        sa.total_cmp(&sb).then(da.total_cmp(&db))
    })?;
    let depth = at.z / n.1.z.min(-0.1);
    if sep(&n).0 >= mw.min(n.2) / 3.0 || !(0.7..=1.45).contains(&depth) {
        return None;
    }
    let (gx, gy) = gaps(at, mw, mh, n.1, n.2, n.3);
    let across = gx > gy;
    let side = |a: f32, b: f32| if a >= b { 1.0 } else { -1.0 };
    let dir = if across {
        side(at.x, n.1.x)
    } else {
        side(at.y, n.1.y)
    };
    at.z = n.1.z;
    if across {
        at.x = n.1.x + dir * (n.2 + mw) / 2.0;
        at.y = align(at.y, mh / 2.0, n.1.y, n.3 / 2.0);
    } else {
        at.y = n.1.y + dir * (n.3 + mh) / 2.0;
        at.x = align(at.x, mw / 2.0, n.1.x, n.2 / 2.0);
    }
    // Still on top of another one: past it, the same way.
    for _ in 0..others.len() {
        let Some(o) = others.iter().find(|o| {
            let (gx, gy) = gaps(at, mw, mh, o.1, o.2, o.3);
            gx < -1e-3 && gy < -1e-3
        }) else {
            break;
        };
        if across {
            at.x = o.1.x + dir * (o.2 + mw) / 2.0;
        } else {
            at.y = o.1.y + dir * (o.3 + mh) / 2.0;
        }
    }
    Some(Stick {
        placement: Placement {
            position: center.0 + turn * at,
            rotation: turn,
            width: mw,
        },
        neighbour: n.0,
        across,
        side: dir,
    })
}

/// The monitor shown in cinema mode: `pick` if it is among `monitors`, else the
/// largest landscape one (films are wide), else the first.
pub fn cinema_choice<K: Key>(monitors: &[(K, u32, u32)], pick: Option<K>) -> Option<K> {
    pick.and_then(|k| monitors.iter().find(|m| m.0 == k))
        .or_else(|| {
            monitors
                .iter()
                .filter(|m| m.1 > m.2)
                .max_by_key(|m| u64::from(m.1) * u64::from(m.2))
        })
        .or_else(|| monitors.first())
        .map(|m| m.0)
}

/// Cinema: one monitor big and further away, straight ahead at eye level; the others
/// are not shown.
pub fn cinema<K: Key>(monitors: &[(K, u32, u32)], pick: Option<K>) -> HashMap<K, Placement> {
    const DISTANCE: f32 = 3.0;
    /// About 56° wide, like a good seat in a cinema.
    const WIDTH: f32 = 3.2;
    let id = cinema_choice(monitors, pick);
    let mut out = HashMap::new();
    if let Some(id) = id {
        let position = Vec3::new(0.0, -0.05, -DISTANCE);
        out.insert(
            id,
            Placement {
                position,
                rotation: facing(position, Vec3::new(0.0, -0.05, 0.0)),
                width: WIDTH,
            },
        );
    }
    out
}

/// The layout's name for `preset`.
fn shape(preset: Preset) -> circle::Preset {
    match preset {
        Preset::Arc => circle::Preset::Arc,
        Preset::Wall => circle::Preset::Wall,
        Preset::Overhead => circle::Preset::Overhead,
    }
}

/// `(id, width, height)` as the layout takes them.
fn pixel_sizes<K: Key>(monitors: &[(K, u32, u32)]) -> Vec<(K, prostir_layout::space::PixelSize)> {
    monitors
        .iter()
        .map(|&(id, w, h)| (id, prostir_layout::space::PixelSize::new(w, h)))
        .collect()
}

/// How far from the head `preset` puts `monitors` (a wall's distance): the circle
/// grows when they do not fit around the user (see `prostir_layout::circle`).
pub fn preset_radius<K: Key>(preset: Preset, monitors: &[(K, u32, u32)]) -> f32 {
    circle::radius_of(shape(preset), &pixel_sizes(monitors), &Limits::default())
}

/// Places every monitor for a preset, around the origin facing -Z (see
/// `prostir_layout::circle`). `monitors` and `mirror` hold `(id, width_px,
/// height_px)`; the monitors go left to right in the given order (as on the desktop),
/// in one row while it fits around the user, else in rows; the laptop mirror goes under
/// the primary monitor. `curve`: the monitors are drawn curved, on a cylinder this many
/// times the arrangement's radius (`None`: flat); the mirror then continues the
/// cylinder of the monitor above it.
pub fn arrange<K: Key>(
    preset: Preset,
    monitors: &[(K, u32, u32)],
    primary: Option<K>,
    mirror: Option<(K, u32, u32)>,
    curve: Option<f32>,
) -> HashMap<K, Placement> {
    circle::arrange(
        shape(preset),
        &pixel_sizes(monitors),
        primary,
        mirror.map(|m| pixel_sizes(&[m])[0]),
        curve,
        &Limits::default(),
    )
    .into_iter()
    .collect()
}

/// Where the arrangement is seen from and how it is turned: from `center` (eye, yaw);
/// lying down, along the band tilted up overhead (seen from below, left and right would
/// make no sense).
pub fn frame(center: (Vec3, f32), preset: Preset) -> Frame {
    if preset == Preset::Overhead {
        Frame::tilted(center.0, center.1, OVERHEAD_PITCH)
    } else {
        Frame::upright(center.0, center.1)
    }
}

/// How far up [`frame`] is tilted for `preset`, in radians.
pub fn tilt(preset: Preset) -> f32 {
    if preset == Preset::Overhead {
        OVERHEAD_PITCH
    } else {
        0.0
    }
}

/// Every one of `monitors` (id, pixel size) that has a placement.
fn placed<K: Key>(
    placements: &HashMap<K, Placement>,
    monitors: &[(K, u32, u32)],
) -> Vec<Monitor<K>> {
    pixel_sizes(monitors)
        .into_iter()
        .filter_map(|(id, px)| {
            Some(Monitor {
                id,
                px,
                placement: *placements.get(&id)?,
            })
        })
        .collect()
}

/// The rows of `monitors` as seen from `frame`, top to bottom and each left to right:
/// monitors side by side as the eye sees them. Two neighbours in `previous` (the rows
/// the PC has) stay neighbours until one is clearly moved away, so a hand's tremor
/// never moves a monitor from one row to another (see `prostir_layout::rows`).
fn rows_seen<K: Key>(room: &Room<K>, monitors: &[Monitor<K>], previous: &[Vec<K>]) -> Vec<Vec<K>> {
    let ids: Vec<K> = monitors.iter().map(|m| m.id).collect();
    rows::rows_of(&ids, room, Some(previous))
}

/// What the PC lays its desktop out by: the rows the monitors are seen in, and the
/// desktop made of them, in the pixels of the streams. The PC makes the same desktop of
/// it, to the pixel, and the map in the menu shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desktop<K> {
    /// Top to bottom, each left to right.
    pub rows: Vec<Vec<K>>,
    pub plan: Vec<Spot<K>>,
}

impl<K: Key> Desktop<K> {
    /// The desktop for `monitors` as they stand in `placements`, seen from `center`,
    /// the `primary` monitor's row first. `sent`: the desktop the PC has. Its rows keep
    /// their monitors until one is clearly moved out, and while the rows stay its plan
    /// does as long as it fits what is seen as well, give or take a hand's tremor (see
    /// `prostir_layout::desk::settled`): a nudge changes nothing on the PC.
    pub fn of(
        placements: &HashMap<K, Placement>,
        monitors: &[(K, u32, u32)],
        center: (Vec3, f32),
        preset: Preset,
        primary: Option<K>,
        sent: Option<&Desktop<K>>,
    ) -> Self {
        let all = placed(placements, monitors);
        let room = Room::new(&frame(center, preset), &all);
        let rows = rows_seen(&room, &all, sent.map_or(&[], |d| &d.rows));
        let size = |id: K| {
            let m = all.iter().find(|m| m.id == id)?;
            Some(desk::Size {
                w: i32::try_from(m.px.w).ok()?,
                h: i32::try_from(m.px.h).ok()?,
            })
        };
        let wanted = desk::Desk {
            rows: rows.clone(),
            main: primary,
            ..desk::Desk::default()
        };
        let fresh = desk::plan(&wanted, size, &room);
        let plan = match sent {
            Some(s) if s.rows == rows => desk::settled(&s.plan, &fresh, &room),
            _ => fresh,
        };
        Self { rows, plan }
    }

    /// What [`Desktop::of`] takes as the PC's desktop when only its `rows` are known (a
    /// profile switched to keeps its rows, not its plan): their pairs of neighbours stay
    /// pairs until one is clearly moved away, and the plan is made afresh. `None` for no
    /// rows: rows then start afresh too.
    pub fn history(rows: Vec<Vec<K>>) -> Option<Self> {
        (!rows.is_empty()).then(|| Self {
            rows,
            plan: Vec::new(),
        })
    }

    /// As `Arrange` carries it: the rows, and where each monitor's spot is centred
    /// (see `PROTOCOL.md`, "Layout"), of the monitors `number` gives a number on the
    /// PC it goes to (a PC hears of its own displays only).
    pub fn wire(&self, number: impl Fn(K) -> Option<DisplayId>) -> Vec<Row> {
        self.rows
            .iter()
            .map(|row| Row {
                entries: row
                    .iter()
                    .filter_map(|&k| {
                        let at = self.plan.iter().find(|s| s.id == k).map(Spot::wire);
                        Some(Placed {
                            id: number(k)?,
                            center: at.map(|a| a.0),
                            middle: at.map(|a| a.1),
                        })
                    })
                    .collect(),
            })
            .filter(|row| !row.entries.is_empty())
            .collect()
    }

    /// The monitors for the map in the menu: centre and half size in pixels, up
    /// positive.
    pub fn spots(&self) -> HashMap<K, [f32; 4]> {
        self.plan
            .iter()
            .map(|s| {
                let (w, h) = (s.w as f32, s.h as f32);
                let (x, y) = (s.x as f32 + w / 2.0, s.y as f32 + h / 2.0);
                (s.id, [x, -y, w / 2.0, h / 2.0])
            })
            .collect()
    }
}

/// What a [`Desktop`] is made of.
#[derive(Debug, Clone, PartialEq)]
struct DesktopInputs<K> {
    placed: Vec<(K, Placement, u32, u32)>,
    center: (Vec3, f32),
    preset: Preset,
    primary: Option<K>,
    sent: Option<Desktop<K>>,
}

/// [`Desktop::of`], made again only when what it is made of changes: the headset asks
/// for it every frame, and planning takes about a millisecond for six monitors.
#[derive(Debug)]
pub struct DesktopMemo<K>(RefCell<Option<(DesktopInputs<K>, Desktop<K>)>>);

impl<K> Default for DesktopMemo<K> {
    fn default() -> Self {
        Self(RefCell::new(None))
    }
}

impl<K: Key> DesktopMemo<K> {
    /// [`Desktop::of`] these.
    pub fn get(
        &self,
        placements: &HashMap<K, Placement>,
        monitors: &[(K, u32, u32)],
        center: (Vec3, f32),
        preset: Preset,
        primary: Option<K>,
        sent: Option<&Desktop<K>>,
    ) -> Desktop<K> {
        let inputs = DesktopInputs {
            placed: monitors
                .iter()
                .filter_map(|&(id, w, h)| Some((id, *placements.get(&id)?, w, h)))
                .collect(),
            center,
            preset,
            primary,
            sent: sent.cloned(),
        };
        if let Some((known, desktop)) = &*self.0.borrow()
            && *known == inputs
        {
            return desktop.clone();
        }
        let desktop = Desktop::of(placements, monitors, center, preset, primary, sent);
        *self.0.borrow_mut() = Some((inputs, desktop.clone()));
        desktop
    }
}

/// "Line up" (the A button): the crookedness taken out of every row (a monitor turned
/// or hanging crooked, gaps, steps) and nothing more. How each row stands (tilted back,
/// overhead, turned) and where, the rows themselves and the gaps between them stay (see
/// `prostir_layout::tidy`). `previous`: the rows the PC has. `lid`: the laptop screen,
/// which is not lined up and moves only down out of the way of a monitor lined up over
/// it.
pub fn line_up<K: Key>(
    placements: &HashMap<K, Placement>,
    monitors: &[(K, u32, u32)],
    center: (Vec3, f32),
    preset: Preset,
    primary: Option<K>,
    previous: &[Vec<K>],
    lid: Option<K>,
) -> HashMap<K, Placement> {
    let all = placed(placements, monitors);
    let frame = frame(center, preset);
    let rows = rows_seen(&Room::new(&frame, &all), &all, previous);
    tidy::line_up(&all, &frame, primary, Some(&rows), lid)
        .into_iter()
        .collect()
}

/// Where a new monitor of `size` pixels goes next to the placed ones of `monitors`
/// (it is not among them), which do not move: at the end of the longest row with
/// room, the end nearest the gaze (a direction in the room); over the top row or under
/// the bottom one only when no row has room; covering nothing (see
/// `prostir_layout::place`). The first one goes straight ahead on a circle of
/// `radius`. `previous`: the rows the PC has.
#[allow(clippy::too_many_arguments)]
pub fn place_new<K: Key>(
    placements: &HashMap<K, Placement>,
    monitors: &[(K, u32, u32)],
    size: (u32, u32),
    center: (Vec3, f32),
    preset: Preset,
    gaze: Vec3,
    previous: &[Vec<K>],
    radius: f32,
) -> Placement {
    let all = placed(placements, monitors);
    let frame = frame(center, preset);
    let rows = rows_seen(&Room::new(&frame, &all), &all, previous);
    place::place_new(
        &all,
        &frame,
        gaze,
        prostir_layout::space::PixelSize::new(size.0, size.1),
        Some(&rows),
        &Limits {
            radius,
            ..Limits::default()
        },
    )
}

#[derive(Debug, Clone, Copy)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
}

/// Distance along the ray to the monitor surface and the hit point in monitor-local
/// coordinates, if the ray goes through it.
pub fn hit(ray: Ray, p: &Placement, height: f32) -> Option<(f32, Vec2)> {
    let normal = p.rotation * Vec3::Z;
    let denom = ray.dir.dot(normal);
    if denom.abs() < 1e-4 {
        return None;
    }
    let t = (p.position - ray.origin).dot(normal) / denom;
    if t <= 0.0 {
        return None;
    }
    let local = p.rotation.inverse() * (ray.origin + ray.dir * t - p.position);
    (local.x.abs() <= p.width / 2.0 && local.y.abs() <= height / 2.0)
        .then_some((t, Vec2::new(local.x, local.y)))
}

/// A curved monitor: the part of a cylinder (axis along the monitor's up) whose
/// chord is the flat monitor, so curved neighbours still meet edge to edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cylinder {
    /// Centre of the cylinder; the visible arc lies along its rotation's -Z.
    pub center: Vec3,
    pub radius: f32,
    pub central_angle: f32,
}

impl Cylinder {
    pub fn new(p: &Placement, radius: f32) -> Self {
        let half = p.width / 2.0;
        let radius = radius.max(half * 1.001);
        let normal = p.rotation * Vec3::Z;
        Self {
            center: p.position + normal * (radius * radius - half * half).sqrt(),
            radius,
            central_angle: 2.0 * (half / radius).asin(),
        }
    }

    /// Arc length, i.e. how wide the image is laid out on the cylinder.
    pub fn arc_length(&self) -> f32 {
        self.radius * self.central_angle
    }
}

/// A ray hit on a monitor (flat or curved): distance, the hit point in flat-monitor
/// coordinates (x right, y up, metres from the centre), the world point and the
/// surface normal facing the viewer.
#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub t: f32,
    pub local: Vec2,
    pub point: Vec3,
    pub normal: Vec3,
}

pub fn hit_surface(ray: Ray, p: &Placement, height: f32, curve_radius: Option<f32>) -> Option<Hit> {
    let Some(radius) = curve_radius else {
        let (t, local) = hit(ray, p, height)?;
        return Some(Hit {
            t,
            local,
            point: ray.origin + ray.dir * t,
            normal: p.rotation * Vec3::Z,
        });
    };
    let c = Cylinder::new(p, radius);
    let inv = p.rotation.inverse();
    let o = inv * (ray.origin - c.center);
    let d = inv * ray.dir;
    // (o.x + t d.x)² + (o.z + t d.z)² = r²
    let a = d.x * d.x + d.z * d.z;
    if a < 1e-8 {
        return None;
    }
    let b = 2.0 * (o.x * d.x + o.z * d.z);
    let cc = o.x * o.x + o.z * o.z - c.radius * c.radius;
    let disc = b * b - 4.0 * a * cc;
    if disc < 0.0 {
        return None;
    }
    let sq = disc.sqrt();
    let mut roots = [(-b - sq) / (2.0 * a), (-b + sq) / (2.0 * a)];
    roots.sort_by(f32::total_cmp);
    roots.into_iter().filter(|t| *t > 0.0).find_map(|t| {
        let q = o + d * t;
        let angle = q.x.atan2(-q.z);
        if angle.abs() > c.central_angle / 2.0 || q.y.abs() > height / 2.0 {
            return None;
        }
        let point = ray.origin + ray.dir * t;
        let normal = p.rotation * Vec3::new(-q.x, 0.0, -q.z).normalize();
        // Flat-equivalent coordinate: position along the chord.
        let local = Vec2::new(c.radius * angle.sin(), q.y);
        Some(Hit {
            t,
            local,
            point,
            normal,
        })
    })
}

/// Where `local` (a hit's flat-equivalent point) lies across the image: 0..=1 from
/// the top left. On a curve the image is laid out along the arc.
pub fn image_uv(p: &Placement, height: f32, curve_radius: Option<f32>, local: Vec2) -> Vec2 {
    let v = 0.5 - local.y / height;
    let u = match curve_radius {
        None => local.x / p.width + 0.5,
        Some(radius) => {
            let c = Cylinder::new(p, radius);
            (local.x / c.radius).clamp(-1.0, 1.0).asin() / c.central_angle + 0.5
        }
    };
    Vec2::new(u.clamp(0.0, 1.0), v.clamp(0.0, 1.0))
}

/// The world point at `uv` (0..=1 from the top left) of a monitor's image, just in
/// front of it, and the surface normal there.
pub fn image_point(
    p: &Placement,
    height: f32,
    curve_radius: Option<f32>,
    uv: Vec2,
) -> (Vec3, Vec3) {
    let y = (0.5 - uv.y) * height;
    match curve_radius {
        None => {
            let normal = p.rotation * Vec3::Z;
            let point = p.position + p.rotation * Vec3::new((uv.x - 0.5) * p.width, y, 0.0);
            (point + normal * 0.004, normal)
        }
        Some(radius) => {
            let c = Cylinder::new(p, radius);
            let angle = (uv.x - 0.5) * c.central_angle;
            let q = Vec3::new(c.radius * angle.sin(), y, -c.radius * angle.cos());
            let normal = p.rotation * Vec3::new(-q.x, 0.0, -q.z).normalize();
            (c.center + p.rotation * q + normal * 0.004, normal)
        }
    }
}

/// A thing held by one controller or hand: a display, the menu or the keyboard
/// window.
#[derive(Debug, Clone, Copy)]
pub struct Grab {
    pub id: Thing,
    /// Distance from the controller to the grabbed point.
    pub distance: f32,
    /// Grabbed point relative to the monitor centre, in the monitor's plane, as a
    /// fraction of its width so resizing keeps the point under the controller.
    pub anchor: Vec2,
    /// For hands: distance at the start and how far the hand was from the head, so
    /// reaching out pushes the monitor away and pulling in brings it closer.
    reach: Option<(f32, f32)>,
    /// For hands taking hold of something within reach: it moves with the hand one to
    /// one, like a thing held (hand and its position when it was taken).
    near: bool,
    held_from: Option<(Vec3, Vec3)>,
}

/// Closer than this from the hand, a grab holds the thing itself rather than
/// steering it along the ray.
const NEAR_GRAB: f32 = 0.35;

impl Grab {
    pub fn start(id: Thing, distance: f32, local_hit: Vec2, placement: &Placement) -> Self {
        Self {
            id,
            distance,
            anchor: local_hit / placement.width,
            reach: None,
            near: false,
            held_from: None,
        }
    }

    /// Distance follows how far the hand reaches from the head (no thumbstick on hands).
    /// Within reach, it follows the hand one to one instead.
    pub fn with_reach(mut self, hand: Vec3, head: Vec3) -> Self {
        if self.distance < NEAR_GRAB {
            self.near = true;
        } else {
            self.reach = Some((self.distance, hand.distance(head).max(0.05)));
        }
        self
    }

    /// Moves the monitor so the grabbed point follows the ray, facing the head.
    /// `stick` pushes/pulls (y) and resizes (x).
    pub fn update(
        &mut self,
        placement: &mut Placement,
        ray: Ray,
        head: Vec3,
        stick: Vec2,
        dt: f32,
    ) {
        if self.near {
            let (hand, start) = *self
                .held_from
                .get_or_insert((ray.origin, placement.position));
            placement.position = start + (ray.origin - hand);
            placement.rotation = facing(placement.position, head);
            return;
        }
        let dead = |v: f32| if v.abs() < 0.15 { 0.0 } else { v };
        self.distance = match self.reach {
            Some((d0, r0)) => d0 * (ray.origin.distance(head) / r0).powi(3),
            None => self.distance * (1.0 + dead(stick.y) * 1.5 * dt),
        }
        .clamp(0.3, 6.0);
        placement.width = (placement.width * (1.0 + dead(stick.x) * 1.0 * dt)).clamp(0.2, 8.0);
        let point = ray.origin + ray.dir * self.distance;
        let offset = (self.anchor * placement.width).extend(0.0);
        // Two passes: the orientation depends on the position and vice versa.
        for _ in 0..2 {
            placement.position = point - placement.rotation * offset;
            placement.rotation = facing(placement.position, head);
        }
    }
}

#[cfg(test)]
mod grab_tests {
    use super::*;

    #[test]
    fn a_thing_held_near_follows_the_hand_one_to_one() {
        let head = Vec3::new(0.0, 1.6, 0.0);
        let mut p = Placement {
            position: Vec3::new(0.0, 1.4, -0.6),
            rotation: facing(Vec3::new(0.0, 1.4, -0.6), head),
            width: 0.6,
        };
        let hand = Vec3::new(0.05, 1.35, -0.4);
        let mut g = Grab::start(Thing::Menu, 0.2, Vec2::ZERO, &p).with_reach(hand, head);
        let ray = |o: Vec3| Ray {
            origin: o,
            dir: -Vec3::Z,
        };
        g.update(&mut p, ray(hand), head, Vec2::ZERO, 0.01);
        // Pulled 25 cm towards the user: the menu comes 25 cm closer.
        g.update(
            &mut p,
            ray(hand + Vec3::new(0.0, 0.0, 0.25)),
            head,
            Vec2::ZERO,
            0.01,
        );
        assert!((p.position.z - (-0.35)).abs() < 1e-4, "{:?}", p.position);
    }
}

/// The desk arrangement: where the user put the monitors and the PCs' own displays.
/// Read from `layout.txt` once at the start and held in memory from then on (the file
/// follows it and is never read again). Cinema mode shows its own one-screen view
/// instead, and that view never takes the desk arrangement's place here.
#[derive(Default)]
pub struct Desk {
    saved: Arrangement,
    /// The screen cinema shows (see [`cinema_choice`]): one picked in the menu.
    cinema: Option<DisplayKey>,
}

/// A stream's size in pixels, width then height.
pub type PixelSize = (u32, u32);

/// An arrangement as `layout.txt` keeps the desk one, of the displays of every PC.
#[derive(Clone)]
pub struct Arrangement<K = DisplayKey> {
    pub placements: HashMap<K, Placement>,
    /// The stream size each placement was made for.
    pub sizes: HashMap<K, PixelSize>,
    /// The eye and the yaw the arrangement was laid out around.
    pub center: Option<(Vec3, f32)>,
    /// The circle it stands on.
    pub radius: Option<f32>,
}

impl<K> Default for Arrangement<K> {
    fn default() -> Self {
        Self {
            placements: HashMap::new(),
            sizes: HashMap::new(),
            center: None,
            radius: None,
        }
    }
}

impl Desk {
    /// The arrangement `layout.txt` holds (of monitors and the PCs' own displays:
    /// window panels are placed afresh when they appear, and never saved):
    ///
    /// ```text
    /// format 2
    /// center 0 1.6 0 0.25
    /// radius 1.3
    /// cinema 0123456789abcdef:3
    /// 0123456789abcdef:3 -0.6 1.6 -1.2 0 0.2 0 0.98 0.77 2560x1440
    /// ```
    ///
    /// A file this version does not write so is not read at all.
    pub fn parse(text: &str) -> Result<Self, LoadError> {
        let mut saved = Arrangement::default();
        let mut cinema = None;
        for (n, line) in persist::body(text)? {
            let bad = LoadError::Line(n);
            let (word, rest) = line.split_once(' ').ok_or(bad)?;
            match word {
                "center" if saved.center.is_none() => {
                    saved.center = Some(parse_center(rest).ok_or(bad)?);
                }
                "radius" if saved.radius.is_none() => {
                    saved.radius = Some(parse_radius(rest).ok_or(bad)?);
                }
                "cinema" if cinema.is_none() => cinema = Some(DisplayKey::parse(rest).ok_or(bad)?),
                _ => {
                    let key = DisplayKey::parse(word).ok_or(bad)?;
                    let (p, size) = parse_place(rest, true).ok_or(bad)?;
                    if saved.placements.insert(key, p).is_some() {
                        return Err(bad);
                    }
                    if let Some(size) = size {
                        saved.sizes.insert(key, size);
                    }
                }
            }
        }
        Ok(Self { saved, cinema })
    }

    /// The desk arrangement as last kept.
    pub fn arrangement(&self) -> &Arrangement {
        &self.saved
    }

    /// Keeps `placements` (made for the stream `sizes`, around `center`, on the circle
    /// of `radius`) as the desk arrangement, unless they are the cinema view; true when
    /// kept, and `layout.txt` is to be written. What is kept is what the file says:
    /// a size only for a placement. Its maps keep their memory from one time to the
    /// next.
    pub fn keep(
        &mut self,
        placements: impl IntoIterator<Item = (DisplayKey, Placement)>,
        sizes: &HashMap<DisplayKey, PixelSize>,
        (center, radius): ((Vec3, f32), f32),
        cinema: bool,
    ) -> bool {
        if cinema {
            return false;
        }
        let saved = &mut self.saved;
        saved.placements.clear();
        saved.placements.extend(placements);
        saved.sizes.clear();
        saved.sizes.extend(
            sizes
                .iter()
                .filter(|(id, _)| saved.placements.contains_key(id))
                .map(|(id, size)| (*id, *size)),
        );
        saved.center = Some(center);
        saved.radius = Some(radius);
        true
    }

    /// The screen cinema shows, as picked in the menu.
    pub fn cinema(&self) -> Option<DisplayKey> {
        self.cinema
    }

    /// Cinema shows `screen` from now on; true when that is a change, and
    /// `layout.txt` is to be written.
    pub fn pick_cinema(&mut self, screen: Option<DisplayKey>) -> bool {
        std::mem::replace(&mut self.cinema, screen) != screen
    }

    /// Forgets the places of the displays `gone`, and the cinema's pick if it is one of
    /// them; true when it had one of them, and `layout.txt` is to be written.
    pub fn forget(&mut self, gone: &[DisplayKey]) -> bool {
        let saved = &mut self.saved;
        let before = saved.placements.len();
        saved.placements.retain(|k, _| !gone.contains(k));
        saved.sizes.retain(|k, _| !gone.contains(k));
        let picked = self.cinema.is_some_and(|k| gone.contains(&k));
        if picked {
            self.cinema = None;
        }
        saved.placements.len() != before || picked
    }

    /// The text of `layout.txt` (see [`Desk::parse`]), added to `out` (a buffer kept
    /// between saves).
    pub fn write_text(&self, out: &mut String) {
        use std::fmt::Write as _;
        let saved = &self.saved;
        persist::write_format(out);
        if let Some((eye, yaw)) = saved.center {
            let _ = writeln!(out, "center {} {} {} {yaw}", eye.x, eye.y, eye.z);
        }
        if let Some(radius) = saved.radius {
            let _ = writeln!(out, "radius {radius}");
        }
        if let Some(screen) = self.cinema {
            let _ = writeln!(out, "cinema {screen}");
        }
        for key in ascending(&saved.placements) {
            if let Some(p) = saved.placements.get(&key) {
                let _ = write!(out, "{key}");
                write_place(out, p, saved.sizes.get(&key).copied());
            }
        }
    }
}

/// Where the keyboard window lies, as `keyboard.txt` keeps it:
///
/// ```text
/// format 2
/// keyboard 0 0.8 -0.4 -0.71 0 0 0.71 0.42
/// ```
pub fn parse_keyboard(text: &str) -> Result<Placement, LoadError> {
    let mut place = None;
    let mut last = 1;
    for (n, line) in persist::body(text)? {
        last = n;
        let p = line
            .strip_prefix("keyboard ")
            .and_then(|rest| parse_place(rest, false))
            .filter(|_| place.is_none())
            .ok_or(LoadError::Line(n))?;
        place = Some(p.0);
    }
    place.ok_or(LoadError::Line(last + 1))
}

/// The text of `keyboard.txt` for the window at `place` (see [`parse_keyboard`]),
/// added to `out`.
pub fn write_keyboard(out: &mut String, place: &Placement) {
    persist::write_format(out);
    out.push_str("keyboard");
    write_place(out, place, None);
}

/// The keys of `map` from the smallest up, without a sorted copy (an arrangement has
/// a handful).
fn ascending<K: Key, V>(map: &HashMap<K, V>) -> impl Iterator<Item = K> + '_ {
    let mut last: Option<K> = None;
    std::iter::from_fn(move || {
        let next = map
            .keys()
            .copied()
            .filter(|id| last.is_none_or(|l| *id > l))
            .min()?;
        last = Some(next);
        Some(next)
    })
}

/// ` x y z qx qy qz qw width`, then the stream size it was placed for (` 2560x1440`)
/// when known, and the end of the line, added to `out`.
pub(crate) fn write_place(out: &mut String, p: &Placement, size: Option<PixelSize>) {
    use std::fmt::Write as _;
    let (pos, q) = (p.position, p.rotation);
    let _ = write!(
        out,
        " {} {} {} {} {} {} {} {}",
        pos.x, pos.y, pos.z, q.x, q.y, q.z, q.w, p.width
    );
    if let Some((w, h)) = size {
        let _ = write!(out, " {w}x{h}");
    }
    out.push('\n');
}

/// `x y z qx qy qz qw width`, then (when `sized`) the stream size it was placed for
/// if there is one (`2560x1440`), as [`write_place`] writes them: a position and a
/// turn that are numbers, a width above nothing.
pub(crate) fn parse_place(text: &str, sized: bool) -> Option<(Placement, Option<PixelSize>)> {
    let mut words = text.split(' ');
    let n = numbers::<8>(&mut words)?;
    let size = match words.next() {
        None => None,
        Some(w) if sized => {
            let (w, h) = w.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        }
        Some(_) => return None,
    };
    if words.next().is_some() {
        return None;
    }
    let p = Placement {
        position: Vec3::new(n[0], n[1], n[2]),
        rotation: Quat::from_xyzw(n[3], n[4], n[5], n[6]).normalize(),
        width: n[7],
    };
    (p.position.is_finite() && p.rotation.is_finite() && p.width.is_finite() && p.width > 0.0)
        .then_some((p, size))
}

/// The next `N` words as numbers, if they are.
fn numbers<'a, const N: usize>(words: &mut impl Iterator<Item = &'a str>) -> Option<[f32; N]> {
    let mut out = [0.0; N];
    for n in &mut out {
        *n = words.next()?.parse().ok()?;
    }
    Some(out)
}

/// `x y z yaw`: where the arrangement was laid out around.
pub(crate) fn parse_center(text: &str) -> Option<(Vec3, f32)> {
    let mut words = text.split(' ');
    let [x, y, z, yaw] = numbers::<4>(&mut words)?;
    let eye = Vec3::new(x, y, z);
    (words.next().is_none() && eye.is_finite() && yaw.is_finite()).then_some((eye, yaw))
}

/// A circle's radius: a number over 10 cm.
pub(crate) fn parse_radius(text: &str) -> Option<f32> {
    text.parse::<f32>()
        .ok()
        .filter(|r| r.is_finite() && *r > 0.1)
}

/// `p` for a stream now `now` pixels in size instead of `was`: the same size of a
/// pixel, centred where it was. A monitor that comes back with another resolution
/// under the same number (a profile, a rotated monitor) would otherwise keep its old
/// width in metres and look bigger or smaller than its neighbours.
pub fn refit(p: Placement, was: PixelSize, now: PixelSize) -> Placement {
    if was.0 == 0 || was == now {
        return p;
    }
    Placement {
        width: p.width * now.0 as f32 / was.0 as f32,
        ..p
    }
}

/// Which way an arrangement faces from `eye`: the mean direction of its monitors, in
/// the yaw convention of `center` (0 is -Z, positive turns towards -X).
pub fn facing_yaw<K: Key>(placements: &HashMap<K, Placement>, eye: Vec3) -> Option<f32> {
    let (sin, cos) = placements
        .values()
        .map(|p| {
            let d = p.position - eye;
            (-d.x).atan2(-d.z)
        })
        .fold((0.0f32, 0.0f32), |(s, c), a| (s + a.sin(), c + a.cos()));
    (sin.abs() > 1e-6 || cos.abs() > 1e-6).then(|| sin.atan2(cos))
}

/// A monitor drawn after another (over it, where they overlap as seen) is drawn before
/// it only once it is further from the head than that one by more than this (5 cm): two
/// at about one distance, as a monitor glued over a row and that row, keep their order,
/// so the strip where they overlap never flickers from one to the other as the head
/// moves.
pub const DRAW_HOLD: f32 = 0.05;

/// The order the monitors are drawn in, far to near (the nearest last, over the others):
/// `order` holds each one's distance from the head (one held in a hand at -1, last of
/// all) and is put in order in place; `before` is the order of the frame before. It
/// starts from that order, the new ones after it, and two next to each other swap only
/// where the one drawn later is further by more than [`DRAW_HOLD`]. Nothing is
/// allocated.
pub fn draw_order<K: Key>(order: &mut [(f32, K)], before: &[K]) {
    let rank = |id: K| before.iter().position(|b| *b == id).unwrap_or(usize::MAX);
    // As drawn before, the new ones after, far to near (an insertion sort: in place).
    let first = |a: &(f32, K), b: &(f32, K)| (rank(a.1), b.0) < (rank(b.1), a.0);
    for i in 1..order.len() {
        let mut j = i;
        while j > 0 && first(&order[j], &order[j - 1]) {
            order.swap(j - 1, j);
            j -= 1;
        }
    }
    for _ in 0..order.len() {
        let mut swapped = false;
        for i in 1..order.len() {
            if order[i].0 > order[i - 1].0 + DRAW_HOLD {
                order.swap(i - 1, i);
                swapped = true;
            }
        }
        if !swapped {
            break;
        }
    }
}

/// `p`, laid out around `from` (eye and yaw, as the headset's `center`), standing the
/// same way around `to`: turned about the vertical by the difference of the yaws, the
/// eye carried to the eye. What is worked out as seen from the eye (rows, the desktop,
/// "Line up", the magnet) sees it from `to` as it saw it from `from`.
pub fn carried(p: &Placement, from: (Vec3, f32), to: (Vec3, f32)) -> Placement {
    let turn = Quat::from_rotation_y(to.1 - from.1);
    Placement {
        position: to.0 + turn * (p.position - from.0),
        rotation: (turn * p.rotation).normalize(),
        width: p.width,
    }
}

/// The yaw "In front" brings straight ahead, seen from `eye`: the `primary` monitor's
/// direction, else the mean direction of `monitors` (what the user arranged around
/// them, not the layout's old forward, which placing by hand has long left behind).
pub fn ahead<K: Key>(
    placements: &HashMap<K, Placement>,
    monitors: &[K],
    primary: Option<K>,
    eye: Vec3,
) -> Option<f32> {
    let direction = |p: &Placement| {
        let d = p.position - eye;
        (-d.x).atan2(-d.z)
    };
    primary
        .and_then(|id| placements.get(&id))
        .map(direction)
        .or_else(|| {
            let (sin, cos) = monitors
                .iter()
                .filter_map(|id| placements.get(id))
                .map(direction)
                .fold((0.0f32, 0.0f32), |(s, c), a| (s + a.sin(), c + a.cos()));
            (sin != 0.0 || cos != 0.0).then(|| sin.atan2(cos))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PcId;

    /// Display `n` of the one PC of these tests.
    fn k(n: u32) -> DisplayKey {
        DisplayKey::new(PcId([5; 8]), DisplayId(n))
    }

    /// `map`, by the displays of the PC of these tests.
    fn keyed<V>(map: HashMap<u32, V>) -> HashMap<DisplayKey, V> {
        map.into_iter().map(|(n, v)| (k(n), v)).collect()
    }

    /// `map`, by the numbers of its displays.
    fn numbered<V: Clone>(map: &HashMap<DisplayKey, V>) -> HashMap<u32, V> {
        map.iter().map(|(k, v)| (k.id.0, v.clone())).collect()
    }

    /// Every number as it is on the PC.
    fn as_is(n: u32) -> Option<DisplayId> {
        Some(DisplayId(n))
    }

    #[test]
    fn arc_has_no_gaps_and_faces_user() {
        let l = arrange(
            Preset::Arc,
            &[(0, 2560, 1440), (1, 2560, 1440), (2, 1920, 1080)],
            Some(1),
            None,
            None,
        );
        for p in l.values() {
            let normal = p.rotation * Vec3::Z;
            let to_axis = (Vec3::new(0.0, p.position.y, 0.0) - p.position).normalize();
            assert!(normal.dot(to_axis) > 0.999);
        }
        // Right edge of one monitor is the left edge of the next.
        let edge = |p: &Placement, side: f32| {
            p.position + p.rotation * Vec3::new(side * p.width / 2.0, 0.0, 0.0)
        };
        for (a, b) in [(0, 1), (1, 2)] {
            let (ea, eb) = (edge(&l[&a], 1.0), edge(&l[&b], -1.0));
            assert!(
                Vec2::new(ea.x - eb.x, ea.z - eb.z).length() < 1e-4,
                "gap between {a} and {b}"
            );
        }
        assert!(l[&0].position.x < l[&1].position.x && l[&1].position.x < l[&2].position.x);
    }

    /// Where each monitor is seen from the head: yaw span and height span.
    fn seen_spans(p: &Placement, h: f32) -> ((f32, f32), (f32, f32)) {
        let flat = Vec2::new(p.position.x, p.position.z).length();
        let yaw = p.position.x.atan2(-p.position.z);
        let half = (p.width / 2.0 / flat.max(0.1)).atan();
        (
            (yaw - half, yaw + half),
            (p.position.y - h / 2.0, p.position.y + h / 2.0),
        )
    }

    /// Any monitors in a preset: none in front of another as seen from the head, every
    /// row within the arc.
    #[test]
    fn presets_never_overlap() {
        let sizes = [
            (1920, 1080),
            (2560, 1440),
            (3440, 1440),
            (5120, 1440),
            (3840, 2160),
            (1440, 2560),
        ];
        let mut seed = 0x1234_u64;
        for case in 0..2000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let n = 1 + (seed >> 33) as usize % 10;
            let mons: Vec<(u32, u32, u32)> = (0..n)
                .map(|i| {
                    let (w, h) = sizes[(seed >> (i * 3 % 30 + 2)) as usize % sizes.len()];
                    (i as u32, w, h)
                })
                .collect();
            let primary = Some(((seed >> 7) as usize % n) as u32);
            let l = arrange(Preset::Arc, &mons, primary, None, None);
            let spans: Vec<_> = mons
                .iter()
                .map(|&(id, w, h)| {
                    let p = l[&id];
                    seen_spans(&p, p.width * h as f32 / w as f32)
                })
                .collect();
            for i in 0..n {
                for j in i + 1..n {
                    let ((a0, a1), (b0, b1)) = spans[i];
                    let ((c0, c1), (d0, d1)) = spans[j];
                    let across = a0 < c1 - 1e-3 && c0 < a1 - 1e-3;
                    let up = b0 < d1 - 1e-4 && d0 < b1 - 1e-4;
                    assert!(
                        !(across && up),
                        "case {case}: {mons:?}: {i} and {j} overlap"
                    );
                }
            }
            // The circle they stand on is the preset's radius.
            let radius = preset_radius(Preset::Arc, &mons);
            for (&(_, w, _), p) in mons.iter().map(|m| (m, l[&m.0])) {
                let half = w as f32 * METRES_PER_PIXEL / 2.0;
                let edge =
                    (Vec2::new(p.position.x, p.position.z).length().powi(2) + half * half).sqrt();
                assert!(
                    (edge - radius).abs() < 1e-3,
                    "case {case}: {edge} vs {radius}"
                );
            }
        }
    }

    #[test]
    fn mirror_goes_under_primary() {
        let l = arrange(
            Preset::Arc,
            &[(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)],
            Some(1),
            Some((100, 2880, 1800)),
            None,
        );
        let (m, p) = (l[&100], l[&1]);
        assert!(m.position.y < p.position.y - 0.5);
        assert!((m.position.x - p.position.x).abs() < 1e-3);
        assert!(m.width <= circle::MIRROR_MAX_WIDTH + 1e-6);
        // Leans back to face the eyes.
        assert!((m.rotation * Vec3::Z).y > 0.2);
    }

    #[test]
    fn every_preset_places_everything() {
        let mons = [
            (0, 2560, 1440),
            (1, 1920, 1080),
            (2, 1440, 2560),
            (3, 3440, 1440),
        ];
        for preset in [Preset::Arc, Preset::Wall, Preset::Overhead] {
            let l = arrange(preset, &mons, Some(0), Some((100, 2880, 1800)), None);
            assert_eq!(l.len(), 5, "{preset:?}");
            assert!(
                l.values()
                    .all(|p| p.position.is_finite() && p.rotation.is_finite())
            );
        }
    }

    #[test]
    fn strong_curve_on_arc_is_centred_on_the_head() {
        let l = arrange(
            Preset::Arc,
            &[(0, 2560, 1440), (1, 2560, 1440)],
            None,
            None,
            None,
        );
        for p in l.values() {
            let c = Cylinder::new(p, RADIUS);
            assert!(
                Vec2::new(c.center.x, c.center.z).length() < 1e-3,
                "{:?}",
                c.center
            );
        }
    }

    #[test]
    fn curved_hit_matches_flat_at_centre_and_edges() {
        let p = Placement {
            position: Vec3::new(0.0, 0.0, -1.0),
            rotation: Quat::IDENTITY,
            width: 1.2,
        };
        let ray = Ray {
            origin: Vec3::ZERO,
            dir: -Vec3::Z,
        };
        let h = hit_surface(ray, &p, 0.7, Some(1.5)).unwrap();
        assert!(h.local.length() < 1e-4);
        assert!(h.t > 1.0, "the arc bulges away from the viewer");
        assert!(h.normal.dot(Vec3::Z) > 0.999);
        // Aiming at the flat edge hits the curved edge (shared chord endpoints).
        let edge = Ray {
            origin: Vec3::ZERO,
            dir: Vec3::new(0.599, 0.0, -1.0).normalize(),
        };
        let h = hit_surface(edge, &p, 0.7, Some(1.5)).unwrap();
        assert!((h.local.x - 0.599).abs() < 0.01, "{:?}", h.local);
        assert!(
            hit_surface(
                Ray {
                    origin: Vec3::ZERO,
                    dir: Vec3::Z
                },
                &p,
                0.7,
                Some(1.5)
            )
            .is_none()
        );
    }

    #[test]
    fn order_follows_given_sequence_and_directions() {
        // Portrait in the middle: order [0, 2, 1].
        let l = arrange(
            Preset::Arc,
            &[(0, 2560, 1440), (2, 1440, 2560), (1, 2560, 1440)],
            Some(2),
            None,
            None,
        );
        assert!(l[&0].position.x < l[&2].position.x && l[&2].position.x < l[&1].position.x);
        assert!(l[&2].position.x.abs() < 1e-3, "portrait is straight ahead");
    }

    #[test]
    fn ray_hits_center() {
        let p = Placement {
            position: Vec3::new(0.0, 0.0, -1.0),
            rotation: Quat::IDENTITY,
            width: 1.0,
        };
        let (t, local) = hit(
            Ray {
                origin: Vec3::ZERO,
                dir: -Vec3::Z,
            },
            &p,
            0.5,
        )
        .unwrap();
        assert!((t - 1.0).abs() < 1e-5 && local.length() < 1e-5);
        assert!(
            hit(
                Ray {
                    origin: Vec3::ZERO,
                    dir: Vec3::Z
                },
                &p,
                0.5
            )
            .is_none()
        );
    }

    #[test]
    fn cinema_shows_one_monitor() {
        // Row order: portrait in the middle.
        let monitors = [(0, 2560, 1440), (2, 1440, 2560), (1, 3840, 2160)];
        let out = cinema(&monitors, Some(2));
        assert_eq!(out.keys().copied().collect::<Vec<_>>(), vec![2], "picked");
        assert!(out[&2].position.z < -2.0, "further away than the desk");
        // Nothing picked (or a stale pick): the largest landscape monitor, not a portrait one.
        assert_eq!(cinema_choice(&monitors, None), Some(1));
        assert_eq!(cinema_choice(&monitors, Some(9)), Some(1));
        assert_eq!(cinema_choice(&[(4, 1440, 2560)], None), Some(4));
        assert!(cinema::<u32>(&[], None).is_empty());
    }

    #[test]
    fn mirror_hangs_from_the_monitor_above() {
        let monitors = [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)];
        for primary in [0, 1] {
            let out = arrange(
                Preset::Arc,
                &monitors,
                Some(primary),
                Some((100, 2880, 1800)),
                None,
            );
            let above = out[&primary];
            let h_above =
                monitors.iter().find(|m| m.0 == primary).unwrap().2 as f32 * METRES_PER_PIXEL;
            let mirror = out[&100];
            let h = 1800.0
                * METRES_PER_PIXEL
                * (circle::MIRROR_MAX_WIDTH / (2880.0 * METRES_PER_PIXEL));
            let top_edge = mirror.position + mirror.rotation * Vec3::new(0.0, h / 2.0, 0.0);
            let bottom_edge = above.position - Vec3::Y * h_above / 2.0;
            // The portrait one in the middle hangs lowest; under it there is no gap.
            if primary == 1 {
                assert!(
                    top_edge.distance(bottom_edge) < 1e-3,
                    "{top_edge} vs {bottom_edge}"
                );
            }
            assert!(
                top_edge.y <= bottom_edge.y + 1e-3,
                "never above the monitor's bottom"
            );
            // Tilted back: its normal points up towards the eyes.
            assert!((mirror.rotation * Vec3::Z).y > 0.05);
        }
    }

    #[test]
    fn curved_mirror_continues_the_cylinder() {
        let monitors = [(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)];
        let radius = RADIUS;
        let out = arrange(
            Preset::Arc,
            &monitors,
            Some(1),
            Some((100, 2880, 1800)),
            Some(1.0),
        );
        let (above, mirror) = (out[&1], out[&100]);
        let a = Cylinder::new(&above, radius);
        let m = Cylinder::new(&mirror, radius);
        // Same axis (horizontally) and same radius: the edges meet along the whole width.
        assert!(Vec2::new(a.center.x - m.center.x, a.center.z - m.center.z).length() < 1e-3);
        let h_above = 1440.0 * METRES_PER_PIXEL;
        let h =
            1800.0 * METRES_PER_PIXEL * (circle::MIRROR_MAX_WIDTH / (2880.0 * METRES_PER_PIXEL));
        assert!(((above.position.y - h_above / 2.0) - (mirror.position.y + h / 2.0)).abs() < 1e-3);
    }

    /// The desktop for `placements` as the PC gets it, seen from straight ahead.
    fn desktop_of(
        placements: &HashMap<u32, Placement>,
        m: &[(u32, u32, u32)],
        preset: Preset,
        primary: Option<u32>,
    ) -> Desktop<u32> {
        Desktop::of(placements, m, (Vec3::ZERO, 0.0), preset, primary, None)
    }

    #[test]
    fn rows_follow_what_the_user_sees() {
        // An arc: one row, left to right.
        let three = [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)];
        let arc = arrange(Preset::Arc, &three, Some(1), None, None);
        assert_eq!(
            desktop_of(&arc, &three, Preset::Arc, Some(1)).rows,
            vec![vec![0, 1, 2]],
            "a portrait monitor in the arc is in the same row"
        );

        // Lift the right monitor above the middle one: two rows, it is on top.
        let mut lifted = arc.clone();
        let mid = arc[&1];
        let mut top = arc[&2];
        top.position = Vec3::new(mid.position.x, mid.position.y + 1.0, mid.position.z);
        top.rotation = facing(top.position, Vec3::ZERO);
        lifted.insert(2, top);
        let desktop = desktop_of(&lifted, &three, Preset::Arc, Some(1));
        assert_eq!(desktop.rows, vec![vec![2], vec![0, 1]]);
        // It is seen right over the middle monitor, not over the right edge.
        let rows = desktop.wire(as_is);
        let seen_at = |row: usize, i: usize| rows[row].entries[i].center.unwrap();
        let middle = seen_at(1, 1);
        assert!((seen_at(0, 0) - middle).abs() < 150, "{rows:?}");
        assert!(seen_at(1, 0) < middle - 1500, "{rows:?}");

        // A wall: one row, left to right.
        let wall = arrange(Preset::Wall, &three, None, None, None);
        assert_eq!(
            desktop_of(&wall, &three, Preset::Wall, None).rows,
            vec![vec![0, 1, 2]]
        );
    }

    const H: f32 = 1440.0 / 2560.0;

    /// How far the nearest corners of two monitors are from lying on one shared edge.
    fn seam(a: &Placement, b: &Placement) -> f32 {
        let (ca, cb) = (corners(a, a.width * H), corners(b, b.width * H));
        // Each of b's two corners nearest to a must be on a's edge line.
        let mut d: Vec<f32> = cb
            .iter()
            .map(|c| {
                ca.iter()
                    .zip(ca.iter().cycle().skip(1))
                    .map(|(p, q)| {
                        let t = ((c - p).dot(q - p) / (q - p).length_squared()).clamp(-5.0, 5.0);
                        c.distance(p + (q - p) * t)
                    })
                    .fold(f32::MAX, f32::min)
            })
            .collect();
        d.sort_by(f32::total_cmp);
        d[1]
    }

    #[test]
    fn the_magnet_needs_a_monitor_almost_against_another() {
        let two = [(0, 2560, 1440), (1, 2560, 1440)];
        let arc = arrange(Preset::Arc, &two, None, None, None);
        let moved = |by: Vec3| {
            let mut p = arc.clone();
            let m = p.get_mut(&1).unwrap();
            m.position += m.rotation * by;
            p
        };
        // Let go 3 cm off: sticks back, edge to edge.
        let p = moved(Vec3::new(0.03, 0.01, 0.0));
        let s = stick(&p, &two, Vec3::ZERO, 1).expect("sticks");
        assert!(s.across && s.side > 0.0);
        let mut after = p;
        after.insert(1, s.placement);
        assert!(seam(&after[&0], &after[&1]) < 1e-3);
        // A clear gap left on purpose (40 cm): no magnet.
        assert!(stick(&moved(Vec3::new(0.4, 0.0, 0.0)), &two, Vec3::ZERO, 1).is_none());
        // Pulled half a metre nearer on purpose: no magnet.
        assert!(stick(&moved(Vec3::new(0.0, 0.0, 0.5)), &two, Vec3::ZERO, 1).is_none());
    }

    #[test]
    fn dropped_on_a_seam_it_joins_the_nearer_monitor() {
        let three = [(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)];
        let mut p = arrange(
            Preset::Arc,
            &[(1, 2560, 1440), (2, 2560, 1440)],
            None,
            None,
            None,
        );
        let over2 = p[&2].position + p[&2].rotation * Vec3::new(0.4, 0.0, 0.0);
        p.insert(
            0,
            Placement {
                position: over2,
                rotation: facing(over2, Vec3::new(0.0, over2.y, 0.0)),
                width: p[&1].width,
            },
        );
        let s = stick(&p, &three, Vec3::ZERO, 0).expect("sticks");
        assert_eq!(s.neighbour, 2);
    }

    #[test]
    fn on_a_wall_the_magnet_stays_in_the_plane() {
        let two = [(0, 2560, 1440), (1, 2560, 1440)];
        let mut p = arrange(Preset::Wall, &two, None, None, None);
        let w = p[&0].width;
        p.get_mut(&1).unwrap().position = p[&0].position + Vec3::new(w + 0.1, 0.02, 0.05);
        let s = stick_on_wall(&p, &two, (Vec3::ZERO, 0.0), 1).expect("sticks");
        assert!((s.placement.position.z - p[&0].position.z).abs() < 1e-5);
        assert!((s.placement.position.x - (p[&0].position.x + w)).abs() < 1e-4);
        assert!((s.placement.position.y - p[&0].position.y).abs() < 1e-4);
    }

    #[test]
    fn lying_down_the_monitors_are_one_row() {
        let three = [(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)];
        let overhead = arrange(Preset::Overhead, &three, None, None, None);
        let desktop = desktop_of(&overhead, &three, Preset::Overhead, None);
        assert_eq!(desktop.rows, vec![vec![0, 1, 2]]);
    }

    /// A wall: A closes the gap and the step a hand left, in the wall's plane.
    #[test]
    fn a_wall_lines_up_in_its_plane() {
        let center = (Vec3::ZERO, 0.0);
        let two = [(0, 2560, 1440), (1, 2560, 1440)];
        let mut rough = arrange(Preset::Wall, &two, None, None, None);
        rough.get_mut(&1).unwrap().position += Vec3::new(0.06, 0.01, 0.0);
        let t = line_up(&rough, &two, center, Preset::Wall, Some(0), &[], None);
        assert!((t[&0].position.z - t[&1].position.z).abs() < 1e-4);
        assert!(t[&0].rotation.angle_between(t[&1].rotation) < 1e-4);
        assert!((t[&0].position.y - t[&1].position.y).abs() < 1e-4);
        let gap = (t[&1].position.x - t[&1].width / 2.0) - (t[&0].position.x + t[&0].width / 2.0);
        assert!(gap.abs() < 1e-3, "gap {gap}");
    }

    /// What the PC makes of `desktop` as `SetLayout` brings it (as
    /// `crates/prostir/src/displays/` does): boxes from the wire, planned again.
    fn on_the_pc(
        desktop: &Desktop<u32>,
        m: &[(u32, u32, u32)],
        primary: Option<u32>,
    ) -> Vec<Spot<u32>> {
        let size = |id: u32| {
            let &(_, w, h) = m.iter().find(|x| x.0 == id)?;
            Some(desk::Size {
                w: w as i32,
                h: h as i32,
            })
        };
        let boxes = desk::Boxes::new(
            desktop
                .wire(as_is)
                .iter()
                .flat_map(|r| &r.entries)
                .filter_map(|e| {
                    let s = size(e.id.0)?;
                    let seen = desk::SeenBox::from_wire(e.center?, e.middle?, s);
                    Some((e.id.0, seen, s))
                })
                .collect(),
        );
        let wanted = desk::Desk {
            rows: desktop.rows.clone(),
            main: primary,
            ..desk::Desk::default()
        };
        let mut plan = desk::plan(&wanted, size, &boxes);
        plan.sort_by_key(|s| s.id);
        plan
    }

    /// The PC's desktop is the map's, to the pixel.
    fn same_on_the_pc(desktop: &Desktop<u32>, m: &[(u32, u32, u32)], primary: Option<u32>) {
        let mut mine = desktop.plan.clone();
        mine.sort_by_key(|s| s.id);
        assert_eq!(on_the_pc(desktop, m, primary), mine, "{:?}", desktop.rows);
    }

    /// Whether two monitors cover each other as seen from `center`.
    fn covered(placements: &HashMap<u32, Placement>, m: &[(u32, u32, u32)]) -> bool {
        let all = placed(placements, m);
        let seen = prostir_layout::space::see(&frame((Vec3::ZERO, 0.0), Preset::Arc), &all);
        seen.iter().enumerate().any(|(i, a)| {
            seen[i + 1..]
                .iter()
                .any(|b| prostir_layout::space::overlaps(a, b, 0.01))
        })
    }

    /// Every row of `desktop` lies on one line (tops, middles or bottoms), side by side.
    fn rows_on_a_line(desktop: &Desktop<u32>) {
        for row in &desktop.rows {
            let spots: Vec<Spot<u32>> = row
                .iter()
                .filter_map(|id| desktop.plan.iter().find(|s| s.id == *id).copied())
                .collect();
            let line = |f: &dyn Fn(&Spot<u32>) -> i32| {
                spots.iter().all(|s| (f(s) - f(&spots[0])).abs() <= 1)
            };
            assert!(
                line(&|s| s.y) || line(&|s| s.bottom()) || line(&|s| 2 * s.y + s.h),
                "one line: {:?}",
                desktop.plan
            );
            for w in spots.windows(2) {
                assert_eq!(w[0].right(), w[1].x, "side by side: {:?}", desktop.plan);
            }
        }
    }

    /// The six monitors as the headset had them on 2026-11-19 (one row on a 1.1 m
    /// circle, overlapping): the desktop the PC makes of them has no staircase (the
    /// old one spread over -1720..7600 with neighbours meeting at corners). Laid out
    /// again by the preset now: two rows on a wider circle, nothing overlapping.
    #[test]
    fn six_overlapping_monitors_make_no_staircase() {
        let m = [
            (0, 3440, 1440),
            (1, 2560, 1440),
            (2, 1440, 2560),
            (3, 3440, 1440),
            (4, 2560, 1440),
            (5, 3440, 1440),
        ];
        // The old preset: angles cut at asin(0.95), so they overlapped.
        let angle = |w: f32| 2.0 * (w / 2.0 / RADIUS).min(0.95).asin();
        let widths: Vec<f32> = m.iter().map(|x| x.1 as f32 * METRES_PER_PIXEL).collect();
        let total: f32 = widths.iter().map(|w| angle(*w)).sum();
        let mut yaw = -total / 2.0;
        let mut p = HashMap::new();
        for (x, w) in m.iter().zip(&widths) {
            let a = angle(*w);
            let half = (w / 2.0 / RADIUS).min(0.95).asin();
            let r = RADIUS * half.cos();
            let at = yaw + a / 2.0;
            let position = Vec3::new(r * at.sin(), -0.08, -r * at.cos());
            p.insert(
                x.0,
                Placement {
                    position,
                    rotation: facing(position, Vec3::new(0.0, -0.08, 0.0)),
                    width: *w,
                },
            );
            yaw += a;
        }
        assert!(covered(&p, &m), "they did overlap");
        let desktop = desktop_of(&p, &m, Preset::Arc, Some(2));
        rows_on_a_line(&desktop);
        same_on_the_pc(&desktop, &m, Some(2));
        let fresh = arrange(Preset::Arc, &m, Some(2), None, None);
        assert!(!covered(&fresh, &m));
        assert!(preset_radius(Preset::Arc, &m) > RADIUS);
        let desktop = desktop_of(&fresh, &m, Preset::Arc, Some(2));
        assert_eq!(desktop.rows.len(), 2, "{:?}", desktop.rows);
        rows_on_a_line(&desktop);
        same_on_the_pc(&desktop, &m, Some(2));
    }

    /// Two rows made by hand live through everything that happens to them: a monitor
    /// added and removed, the headset app restarted (layout.txt), "Line up", a profile
    /// and back. Every time the same rows, and the same desktop for the PC.
    #[test]
    fn two_rows_survive_everything() {
        let center = (Vec3::ZERO, 0.0);
        let m = [
            (0, 2560, 1440),
            (1, 2560, 1440),
            (2, 1920, 1080),
            (3, 1920, 1080),
            (4, 1440, 2560),
        ];
        let four = &m[..4];
        let new = |p: &HashMap<u32, Placement>, of: &[(u32, u32, u32)], gaze: Vec3| {
            let id = of[of.len() - 1];
            let others = &of[..of.len() - 1];
            place_new(
                p,
                others,
                (id.1, id.2),
                center,
                Preset::Arc,
                gaze,
                &[],
                RADIUS,
            )
        };
        // Two side by side, two more put over them by hand.
        let mut p = arrange(Preset::Arc, &m[..2], Some(0), None, None);
        p.extend(raised(
            &arrange(Preset::Arc, &m[2..4], None, None, None),
            &p,
            &m,
        ));
        let p = line_up(&p, four, center, Preset::Arc, Some(0), &[], None);
        let desktop = desktop_of(&p, four, Preset::Arc, Some(0));
        assert_eq!(desktop.rows.len(), 2, "{:?}", desktop.rows);
        same_on_the_pc(&desktop, four, Some(0));
        let same = |q: &HashMap<u32, Placement>, what: &str| {
            let again = Desktop::of(q, four, center, Preset::Arc, Some(0), Some(&desktop));
            assert_eq!(again, desktop, "{what}");
        };
        // A monitor added: the others stay, and so do their rows.
        let mut with = p.clone();
        with.insert(4, new(&p, &m, -Vec3::Z));
        for (id, place) in &p {
            assert_eq!(with[id], *place, "{id} moved");
        }
        assert!(!covered(&with, &m), "the new one covers nothing");
        let five = Desktop::of(&with, &m, center, Preset::Arc, Some(0), Some(&desktop));
        let kept: Vec<Vec<u32>> = five
            .rows
            .iter()
            .map(|r| r.iter().copied().filter(|id| *id != 4).collect::<Vec<_>>())
            .filter(|r| !r.is_empty())
            .collect();
        assert_eq!(
            kept, desktop.rows,
            "a new monitor joins, nobody leaves a row"
        );
        same_on_the_pc(&five, &m, Some(0));
        // And removed again.
        with.remove(&4);
        same(&with, "removed");
        // The headset app restarted.
        let mut desk = Desk::default();
        desk.keep(keyed(p.clone()), &HashMap::new(), (center, RADIUS), false);
        same(&numbered(&reread(&desk).saved.placements), "restarted");
        // Lined up again.
        same(
            &line_up(&p, four, center, Preset::Arc, Some(0), &desktop.rows, None),
            "lined up",
        );
        // Saved in a profile, another profile, and back: the places by their order.
        let profile = saved_profile(four, Some(0), &p, center, &desktop.rows);
        let (back, _) = switched_to(&profile, four, center);
        same(&back, "profile and back");
    }

    /// `top`, moved up as a whole to stand over `below` with a hand's gap.
    fn raised(
        top: &HashMap<u32, Placement>,
        below: &HashMap<u32, Placement>,
        m: &[(u32, u32, u32)],
    ) -> HashMap<u32, Placement> {
        let height = |p: &HashMap<u32, Placement>, id: u32| {
            let (_, w, h) = m.iter().find(|x| x.0 == id).copied().unwrap();
            p[&id].width * h as f32 / w as f32
        };
        let tallest =
            |p: &HashMap<u32, Placement>| p.keys().map(|id| height(p, *id)).fold(0.0f32, f32::max);
        let lift = (tallest(top) + tallest(below)) / 2.0 + 0.05;
        let mut out = top.clone();
        for id in top.keys() {
            out = moved(&out, *id, 0.0, lift);
        }
        out
    }

    /// Rows of `p` as the PC gets them, and the row that holds `id`.
    fn row_of(rows: &[Vec<u32>], id: u32) -> usize {
        rows.iter().position(|r| r.contains(&id)).unwrap()
    }

    /// 2026-11-20, the owner's report: a new monitor went into a row of its own over
    /// the others, and the next one over that. Four monitors in two rows and the
    /// laptop's panel under them: whether the user looks at the menu, at a monitor or
    /// up, a new one joins a row, beside the others.
    #[test]
    fn a_new_monitor_joins_a_row() {
        let center = (Vec3::ZERO, 0.0);
        let m = [
            (3, 2560, 1440),
            (1, 2560, 1440),
            (100, 2880, 1800),
            (0, 2560, 1440),
            (2, 2560, 1440),
            (104, 2560, 1440),
        ];
        let mut p = arrange(Preset::Arc, &m[..2], Some(1), Some(m[2]), None);
        p.extend(raised(
            &arrange(Preset::Arc, &m[3..5], None, None, None),
            &p,
            &m,
        ));
        let rows = desktop_of(&p, &m[..5], Preset::Arc, Some(1)).rows;
        assert!(!covered(&p, &m[..5]));
        assert_eq!(rows.len(), 3, "{rows:?}");
        for gaze in [
            Vec3::new(0.0, -0.5, -1.0).normalize(),
            p[&0].position.normalize(),
            (p[&2].position + Vec3::Y).normalize(),
            Vec3::Y,
        ] {
            let mut with = p.clone();
            with.insert(
                104,
                place_new(
                    &p,
                    &m[..5],
                    (m[5].1, m[5].2),
                    center,
                    Preset::Arc,
                    gaze,
                    &rows,
                    RADIUS,
                ),
            );
            assert!(!covered(&with, &m), "it covers nothing");
            let after = desktop_of(&with, &m, Preset::Arc, Some(1)).rows;
            assert_eq!(after.len(), rows.len(), "no new row: {after:?}");
            let row = &after[row_of(&after, 104)];
            assert_eq!(row.len(), 3, "beside two others: {after:?}");
        }
    }

    /// Three big monitors fill the row around the user (210 degrees) with the laptop's
    /// panel under them: a new one has room only over them, and the one after it
    /// joins it there instead of starting yet another row.
    #[test]
    fn with_a_full_row_new_monitors_share_one_row_over_it() {
        let center = (Vec3::ZERO, 0.0);
        let m = [
            (3, 2560, 1440),
            (1, 2560, 1440),
            (0, 2560, 1440),
            (100, 2880, 1800),
            (104, 2560, 1440),
            (105, 2560, 1440),
        ];
        let mut p = arrange(Preset::Arc, &m[..3], Some(1), Some(m[3]), None);
        let ahead = Vec3::new(0.0, -0.3, -1.0).normalize();
        let mut rows = desktop_of(&p, &m[..4], Preset::Arc, Some(1)).rows;
        assert_eq!(rows.len(), 2, "{rows:?}");
        for n in [4, 5] {
            let at = place_new(
                &p,
                &m[..n],
                (m[n].1, m[n].2),
                center,
                Preset::Arc,
                ahead,
                &rows,
                RADIUS,
            );
            p.insert(m[n].0, at);
            assert!(!covered(&p, &m[..=n]), "it covers nothing");
            rows = desktop_of(&p, &m[..=n], Preset::Arc, Some(1)).rows;
        }
        assert_eq!(rows.len(), 3, "one row more, not two: {rows:?}");
        assert_eq!(row_of(&rows, 104), row_of(&rows, 105), "{rows:?}");
    }

    /// A small seeded generator, so a failing case comes back the same.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        /// `lo..hi`.
        fn float(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (hi - lo) * (self.next() as f32 / (1u64 << 31) as f32)
        }
    }

    /// `placements` with monitor `id` turned `right` radians around the eye (to the
    /// right when positive) and moved `up` metres.
    fn moved(
        placements: &HashMap<u32, Placement>,
        id: u32,
        right: f32,
        up: f32,
    ) -> HashMap<u32, Placement> {
        let turn = Quat::from_rotation_y(-right);
        let mut out = placements.clone();
        let p = out.get_mut(&id).unwrap();
        p.position = turn * p.position + Vec3::Y * up;
        p.rotation = (turn * p.rotation).normalize();
        out
    }

    /// 2026-11-20, the owner's drag: 2560x1440 | 1440x2560 primary | 2560x1440 in one
    /// row, each moved by hand up to 8 cm up and down and 1.5° into its neighbour or
    /// away, with a 2 mm tremor. In every frame, as the headset sends it (rows after
    /// the rows sent before), one row: no flipping, and the PC puts the portrait
    /// between the two, side by side, as the map shows.
    #[test]
    fn a_portrait_between_two_stays_in_their_row_while_they_move() {
        let center = (Vec3::ZERO, 0.0);
        let three = [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)];
        let start = arrange(Preset::Arc, &three, Some(1), None, None);
        let mut rng = Rng(0x0f11);
        let mut at: HashMap<u32, (f32, f32)> = HashMap::new();
        let mut sent: Option<Desktop<u32>> = None;
        let mut flips = 0;
        for drag in 0..60 {
            let id = drag % 3;
            let from = at.get(&id).copied().unwrap_or_default();
            let to = (rng.float(-1.5, 1.5).to_radians(), rng.float(-0.08, 0.08));
            for f in 1..=20 {
                let t = f as f32 / 20.0;
                let now = (
                    from.0 + (to.0 - from.0) * t + rng.float(-0.002, 0.002) / 1.1,
                    from.1 + (to.1 - from.1) * t + rng.float(-0.002, 0.002),
                );
                at.insert(id, now);
                let mut p = start.clone();
                for (&k, &(r, u)) in &at {
                    p = moved(&p, k, r, u);
                }
                let desktop = Desktop::of(&p, &three, center, Preset::Arc, Some(1), sent.as_ref());
                assert_eq!(desktop.rows, vec![vec![0, 1, 2]], "drag {drag} frame {f}");
                if sent.as_ref().is_some_and(|s| s.rows != desktop.rows) {
                    flips += 1;
                }
                let pc = on_the_pc(&desktop, &three, Some(1));
                let spot = |id: u32| pc.iter().find(|s| s.id == id).copied().unwrap();
                assert_eq!(
                    spot(0).crossing(&spot(1)),
                    Some(prostir_layout::space::Side::Right)
                );
                assert_eq!(
                    spot(1).crossing(&spot(2)),
                    Some(prostir_layout::space::Side::Right)
                );
                same_on_the_pc(&desktop, &three, Some(1));
                sent = Some(desktop);
            }
        }
        assert_eq!(flips, 0);
    }

    /// `placements` turned by `turn` about the eye at `eye`.
    fn turned_about(
        placements: &HashMap<u32, Placement>,
        eye: Vec3,
        turn: Quat,
    ) -> HashMap<u32, Placement> {
        placements
            .iter()
            .map(|(id, p)| {
                let q = Placement {
                    position: eye + turn * (p.position - eye),
                    rotation: (turn * p.rotation).normalize(),
                    width: p.width,
                };
                (*id, q)
            })
            .collect()
    }

    /// The owner's rows of 2026-11-20 tilted back as a whole by 10°, 20° and 35° for a
    /// reclined pose: "Line up" keeps them tilted where they are, and after a hand
    /// disturbed one (a gap, a step, a monitor turned or hanging crooked) gives the
    /// tilted row back, seams closed, the tilt kept, one row on the desktop.
    #[test]
    fn line_up_keeps_a_row_tilted_back() {
        let eye = Vec3::new(0.3, 1.2, -0.4);
        let center = (eye, 0.4);
        for m in [
            [(0u32, 2560u32, 1440u32), (1, 2560, 1440), (2, 2560, 1440)],
            [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)],
        ] {
            let upright = {
                let turn = Quat::from_rotation_y(center.1);
                turned_about(
                    &arrange(Preset::Arc, &m, Some(1), None, None),
                    Vec3::ZERO,
                    turn,
                )
                .into_iter()
                .map(|(id, mut p)| {
                    p.position += eye;
                    (id, p)
                })
                .collect::<HashMap<u32, Placement>>()
            };
            for back in [10.0f32, 20.0, 35.0] {
                let yaw = Quat::from_rotation_y(center.1);
                let turn = yaw * Quat::from_rotation_x(-back.to_radians()) * yaw.inverse();
                let tilted = turned_about(&upright, eye, turn);
                let out = line_up(&tilted, &m, center, Preset::Arc, Some(1), &[], None);
                for (id, p) in &tilted {
                    assert!(
                        out[id].position.distance(p.position) < 1e-3,
                        "{back}°: {id} moved"
                    );
                }
                let mut hand = tilted;
                let axis = turn * (yaw * Vec3::Y);
                let left = turn * (yaw * Vec3::NEG_X);
                hand.get_mut(&0).unwrap().position += left * 0.03;
                let p = hand.get_mut(&2).unwrap();
                p.position += axis * 0.04;
                p.rotation =
                    (Quat::from_axis_angle(axis, 2f32.to_radians()) * p.rotation).normalize();
                let p = hand.get_mut(&1).unwrap();
                p.rotation = (Quat::from_axis_angle(p.rotation * Vec3::Z, 5f32.to_radians())
                    * p.rotation)
                    .normalize();
                let out = line_up(&hand, &m, center, Preset::Arc, Some(1), &[], None);
                for id in [0u32, 1, 2] {
                    let up = out[&id].rotation * Vec3::Y;
                    assert!(
                        up.angle_between(axis).to_degrees() < 0.1,
                        "{back}°: {id} leans"
                    );
                }
                for (a, b) in [(0u32, 1u32), (1, 2)] {
                    let h = |id: u32| {
                        let s = m.iter().find(|x| x.0 == id).unwrap();
                        out[&id].width * s.2 as f32 / s.1 as f32
                    };
                    let (ca, cb) = (corners(&out[&a], h(a)), corners(&out[&b], h(b)));
                    // `corners`: bottom left, bottom right, top right, top left.
                    let right_of_a = (ca[1] + ca[2]) / 2.0;
                    let left_of_b = (cb[0] + cb[3]) / 2.0;
                    assert!(
                        right_of_a.distance(left_of_b) < 1e-3,
                        "{back}°: seam {a}-{b}"
                    );
                }
                let desktop = Desktop::of(&out, &m, center, Preset::Arc, Some(1), None);
                assert_eq!(desktop.rows, vec![vec![0, 1, 2]], "{back}°");
                rows_on_a_line(&desktop);
                same_on_the_pc(&desktop, &m, Some(1));
            }
        }
    }

    /// `p` mirrored in the vertical plane through the origin at azimuth `at` (0 ahead,
    /// positive right).
    fn mirrored(p: &Placement, at: f32) -> Placement {
        let n = Vec3::new(at.cos(), 0.0, at.sin());
        let m = |v: Vec3| v - 2.0 * v.dot(n) * n;
        let right = -m(p.rotation * Vec3::X);
        let up = m(p.rotation * Vec3::Y);
        let face = m(p.rotation * Vec3::Z);
        Placement {
            position: m(p.position),
            rotation: Quat::from_mat3(&glam::Mat3::from_cols(right, up, face)).normalize(),
            width: p.width,
        }
    }

    /// Two 3440 monitors standing mirror-symmetric (each a little off and crooked the
    /// same way, mirrored), no primary: lined up they are still mirror images. Step 4
    /// kept the one nearest straight ahead and moved the other, up to 3.8 m.
    #[test]
    fn line_up_keeps_a_symmetric_pair_symmetric() {
        let center = (Vec3::ZERO, 0.0);
        let two = [(0u32, 3440u32, 1440u32), (1, 3440, 1440)];
        for at in [0.0f32, 0.5, -2.84] {
            let plane = Quat::from_rotation_y(-at);
            let start = arrange(Preset::Arc, &two, None, None, None);
            // The left one turned towards the plane's direction, then disturbed.
            let mut a = start[&0];
            a.position = plane * a.position + Vec3::new(0.0, 0.03, 0.0);
            a.rotation = (Quat::from_rotation_z(0.04) * plane * a.rotation).normalize();
            a.position += plane * Vec3::new(-0.02, 0.0, 0.01);
            let placements = HashMap::from([(0, a), (1, mirrored(&a, at))]);
            let out = line_up(&placements, &two, center, Preset::Arc, None, &[], None);
            let back = mirrored(&out[&0], at);
            assert!(
                back.position.distance(out[&1].position) < 1e-3,
                "{at}: {back:?} {:?}",
                out[&1]
            );
            assert!(back.rotation.angle_between(out[&1].rotation) < 1e-3, "{at}");
        }
    }

    /// One monitor of a row hanging crooked (rolled 6°): "Line up" straightens it about
    /// its own centre and leaves the others where they are.
    #[test]
    fn line_up_straightens_a_crooked_monitor_in_place() {
        let center = (Vec3::ZERO, 0.0);
        let three = [(0u32, 2560u32, 1440u32), (1, 2560, 1440), (2, 2560, 1440)];
        let start = arrange(Preset::Arc, &three, Some(1), None, None);
        let mut crooked = start.clone();
        let p = crooked.get_mut(&2).unwrap();
        p.rotation = (Quat::from_axis_angle(p.rotation * Vec3::Z, 6f32.to_radians()) * p.rotation)
            .normalize();
        let out = line_up(&crooked, &three, center, Preset::Arc, Some(1), &[], None);
        for (id, p) in &start {
            assert!(out[id].position.distance(p.position) < 1e-3, "{id} moved");
            assert!(
                out[id].rotation.angle_between(p.rotation) < 1e-3,
                "{id} turned"
            );
        }
    }

    /// The memo gives what `Desktop::of` gives, and follows a moved monitor.
    #[test]
    fn the_desktop_memo_follows_the_arrangement() {
        let center = (Vec3::ZERO, 0.0);
        let three = [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)];
        let memo = DesktopMemo::default();
        let mut p = arrange(Preset::Arc, &three, Some(1), None, None);
        let of = |p: &HashMap<u32, Placement>| {
            Desktop::of(p, &three, center, Preset::Arc, Some(1), None)
        };
        let get =
            |p: &HashMap<u32, Placement>| memo.get(p, &three, center, Preset::Arc, Some(1), None);
        assert_eq!(get(&p), of(&p));
        assert_eq!(get(&p), of(&p));
        p.get_mut(&2).unwrap().position += Vec3::new(0.0, 0.3, 0.0);
        assert_eq!(get(&p), of(&p));
    }

    /// `desk` as the app finds it after a restart: written to its file and read back.
    fn reread(desk: &Desk) -> Desk {
        let mut text = String::new();
        desk.write_text(&mut text);
        Desk::parse(&text).unwrap()
    }

    /// `profiles` written to their file and read back.
    fn reread_profiles(profiles: &crate::profiles::Slots) -> crate::profiles::Slots {
        let mut text = String::new();
        crate::profiles::write_text(&mut text, profiles);
        crate::profiles::parse(&text).unwrap()
    }

    /// `m` (number, width, height) as a profile takes the monitors: at 100 %.
    fn modes<K: Copy>(m: &[(K, u32, u32)]) -> Vec<(K, prostir_proto::state::Mode)> {
        m.iter()
            .map(|&(k, w, h)| (k, prostir_proto::state::Mode::new(w, h)))
            .collect()
    }

    /// The cinema view never replaces the desk arrangement: leaving cinema finds the
    /// monitors where the user put them.
    #[test]
    fn cinema_view_keeps_the_desk_arrangement() {
        let mut desk = Desk::default();
        let two = [(0, 2560, 1440), (1, 1920, 1080)];
        let sizes = HashMap::from([(0, (2560, 1440)), (1, (1920, 1080))]);
        let center = (Vec3::ZERO, 0.0);
        let sizes = keyed(sizes);
        let mut arranged = arrange(Preset::Arc, &two, Some(0), None, None);
        arranged.get_mut(&1).unwrap().position += Vec3::new(0.1, 0.3, 0.0);
        assert!(desk.keep(keyed(arranged.clone()), &sizes, (center, RADIUS), false));
        assert!(!desk.keep(keyed(cinema(&two, None)), &sizes, (center, RADIUS), true));
        for kept in [desk.arrangement(), reread(&desk).arrangement()] {
            assert_eq!(kept.placements.len(), 2);
            for (id, p) in &arranged {
                assert!(kept.placements[&k(*id)].position.distance(p.position) < 1e-5);
            }
        }
    }

    /// Cinema on, the cinema view moved, cinema off, all in one pass of the loop, with
    /// no write of `layout.txt` done yet (they go on a thread of their own): the desk
    /// is the one from before cinema, as nothing reads the file back. And a profile
    /// saved in cinema takes that desk, not the cinema view.
    #[test]
    fn cinema_in_one_pass_needs_no_file() {
        let two = [(0, 2560, 1440), (1, 1920, 1080)];
        let sizes = keyed(HashMap::from([(0, (2560, 1440)), (1, (1920, 1080))]));
        let center = (Vec3::ZERO, 0.0);
        // Started with an older desk on disk.
        let mut desk = reread(&{
            let mut was = Desk::default();
            was.keep(
                keyed(arrange(Preset::Wall, &two, Some(0), None, None)),
                &sizes,
                (center, RADIUS),
                false,
            );
            was
        });
        let mut at_desk = arrange(Preset::Arc, &two, Some(0), None, None);
        at_desk.get_mut(&0).unwrap().position += Vec3::new(-0.2, 0.1, 0.0);
        // Cinema on: the desk is kept first (`Scene::set_cinema`).
        assert!(desk.keep(keyed(at_desk.clone()), &sizes, (center, RADIUS), false));
        // The cinema view, moved by hand, kept as cinema is not.
        let mut view = keyed(cinema(&two, None));
        for p in view.values_mut() {
            p.position += Vec3::new(0.3, 0.0, -0.4);
        }
        assert!(!desk.keep(view.clone(), &sizes, (center, RADIUS), true));
        // Saved as a profile while in cinema.
        let monitors = modes(&[(k(0), 2560, 1440), (k(1), 1920, 1080)]);
        let live = Arrangement {
            placements: view,
            sizes: sizes.clone(),
            center: Some(center),
            radius: Some(RADIUS),
        };
        let profile = crate::profiles::Profile::of(
            &monitors,
            Some(k(0)),
            Vec::new(),
            &live,
            Some(desk.arrangement()),
            0.0,
            &[],
        );
        assert_eq!(profile.places.len(), 2);
        // Cinema off: what `Scene::adopt_desk` takes.
        let back = desk.arrangement();
        for (i, id) in [0, 1].iter().enumerate() {
            assert!(
                back.placements[&k(*id)]
                    .position
                    .distance(at_desk[id].position)
                    < 1e-6
            );
            assert!(profile.places[i].position.distance(at_desk[id].position) < 1e-6);
        }
    }

    /// The stream sizes and the way the arrangement faces come back as saved; a size
    /// without a placement is not kept (the file has no line for it).
    #[test]
    fn desk_keeps_sizes_and_center() {
        let mut desk = Desk::default();
        let two = [(0, 2560, 1440), (1, 1440, 2560)];
        let arranged = arrange(Preset::Arc, &two, Some(0), None, None);
        let sizes = keyed(HashMap::from([
            (0, (2560, 1440)),
            (1, (1440, 2560)),
            (7, (800, 600)),
        ]));
        let center = (Vec3::new(0.1, 0.0, -0.2), 0.7);
        desk.keep(keyed(arranged), &sizes, (center, RADIUS), false);
        let reread = reread(&desk);
        for saved in [desk.arrangement(), reread.arrangement()] {
            assert_eq!(saved.sizes.len(), 2);
            assert_eq!(saved.sizes[&k(1)], (1440, 2560));
            assert_eq!(saved.radius, Some(RADIUS));
            let (eye, yaw) = saved.center.unwrap();
            assert!(eye.distance(center.0) < 1e-6 && (yaw - center.1).abs() < 1e-6);
            assert_eq!(saved.placements.len(), 2);
        }
        // Written in order of the numbers, the same text each time.
        let mut text = String::new();
        desk.write_text(&mut text);
        let mut again = String::new();
        reread.write_text(&mut again);
        assert_eq!(text, again);
        let ids: Vec<&str> = text
            .lines()
            .skip(3)
            .filter_map(|l| l.split(' ').next())
            .collect();
        assert_eq!(ids, [k(0).to_string(), k(1).to_string()]);
    }

    /// The numbers of a map from the smallest up, each once.
    #[test]
    fn ascending_numbers() {
        let map = HashMap::from([(9, ()), (0, ()), (4, ()), (u32::MAX, ()), (2, ())]);
        assert_eq!(ascending(&map).collect::<Vec<_>>(), [0, 2, 4, 9, u32::MAX]);
        assert_eq!(ascending(&HashMap::<u32, ()>::new()).count(), 0);
    }

    /// Where the keyboard window lies comes back as saved.
    #[test]
    fn keyboard_roundtrip() {
        let place = Placement {
            position: Vec3::new(0.1, 1.2, -0.4),
            rotation: Quat::from_rotation_x(-1.2),
            width: 0.5,
        };
        let mut text = String::new();
        write_keyboard(&mut text, &place);
        let back = parse_keyboard(&text).unwrap();
        assert!(back.position.distance(place.position) < 1e-6);
        assert!(back.rotation.angle_between(place.rotation) < 1e-4);
        assert!((back.width - place.width).abs() < 1e-6);
        assert_eq!(text.lines().next(), Some(persist::FORMAT));
        assert_eq!(parse_keyboard(""), Err(LoadError::Format));
        assert_eq!(
            parse_keyboard("format 2\ngarbage\n"),
            Err(LoadError::Line(2))
        );
        assert_eq!(parse_keyboard("format 2\n"), Err(LoadError::Line(2)));
        let twice = format!("{text}{}", text.lines().nth(1).unwrap());
        assert_eq!(
            parse_keyboard(&format!("{twice}\n")),
            Err(LoadError::Line(3))
        );
        // As 0.1 wrote it.
        assert_eq!(
            parse_keyboard("0 0.1 1.2 -0.4 -0.56 0 0 0.83 0.5\n"),
            Err(LoadError::Format)
        );
    }

    /// What `layout.txt` holds in format 2 comes back; a file of 0.1 (lines of a
    /// number and eight more) is not read, nor one with a line it does not write: a
    /// display twice, a number without its PC, a center or a radius twice, a size
    /// that is not one, a width of nothing, a turn of nothing, words after the size.
    #[test]
    fn desk_format_2() {
        let text = "format 2\ncenter 0 1.6 0 0.25\nradius 1.3\ncinema 0123456789abcdef:12\n\
                    0123456789abcdef:3 -0.5 1.5 -0.9 0 0.2 0 0.98 1.38 2560x1440\n\
                    0123456789abcdef:12 0.5 1.5 -0.9 0 -0.2 0 0.98 1.03\n";
        let desk = Desk::parse(text).unwrap();
        let saved = desk.arrangement();
        let pc = PcId::parse("0123456789abcdef").unwrap();
        let three = DisplayKey::new(pc, DisplayId(3));
        let twelve = DisplayKey::new(pc, DisplayId(12));
        assert_eq!(saved.placements.len(), 2);
        assert_eq!(saved.sizes.get(&three), Some(&(2560, 1440)));
        assert_eq!(saved.sizes.get(&twelve), None);
        assert!((saved.placements[&twelve].width - 1.03).abs() < 1e-6);
        assert_eq!(saved.radius, Some(1.3));
        assert_eq!(saved.center, Some((Vec3::new(0.0, 1.6, 0.0), 0.25)));
        assert_eq!(desk.cinema(), Some(twelve));
        let mut again = String::new();
        desk.write_text(&mut again);
        let back = Desk::parse(&again).unwrap();
        for (key, p) in &saved.placements {
            let q = back.arrangement().placements[key];
            assert!(q.position.distance(p.position) < 1e-6 && (q.width - p.width).abs() < 1e-6);
            assert!(q.rotation.angle_between(p.rotation) < 1e-4);
        }
        assert!(again.starts_with(
            "format 2\ncenter 0 1.6 0 0.25\nradius 1.3\ncinema 0123456789abcdef:12\n"
        ));
        assert_eq!(back.cinema(), Some(twelve));
        // A display gone takes the cinema's pick with it.
        let mut desk = back;
        assert!(desk.forget(&[twelve]));
        assert_eq!(desk.cinema(), None);
        assert!(!desk.forget(&[twelve]));
        assert!(desk.pick_cinema(Some(three)) && !desk.pick_cinema(Some(three)));
        let old = "0 -0.5 -0.08 -0.9 0 0.2 0 0.98 1.38\n1 0.5 -0.08 -0.9 0 -0.2 0 0.98 1.03\n";
        assert_eq!(Desk::parse(old).err(), Some(LoadError::Format));
        let place = "-0.5 1.5 -0.9 0 0.2 0 0.98 1.38";
        for (bad, line) in [
            (
                format!("0123456789abcdef:3 {place}\n0123456789abcdef:3 {place}\n"),
                3,
            ),
            (format!("3 {place}\n"), 2),
            ("center 0 1.6 0 0\ncenter 0 1.6 0 0\n".to_owned(), 3),
            ("radius 1.3\nradius 1.3\n".to_owned(), 3),
            ("radius 0\n".to_owned(), 2),
            ("cinema 12\n".to_owned(), 2),
            (
                "cinema 0123456789abcdef:3\ncinema 0123456789abcdef:3\n".to_owned(),
                3,
            ),
            ("center 0 1.6 0\n".to_owned(), 2),
            (format!("0123456789abcdef:3 {place} 2560\n"), 2),
            ("0123456789abcdef:3 0 0 -1 0 0 0 1 0\n".to_owned(), 2),
            ("0123456789abcdef:3 0 0 -1 0 0 0 0 1\n".to_owned(), 2),
            (format!("0123456789abcdef:3 {place} 2560x1440 more\n"), 2),
            ("mirror on\n".to_owned(), 2),
            ("\n".to_owned(), 2),
        ] {
            let text = format!("format 2\n{bad}");
            assert_eq!(
                Desk::parse(&text).err(),
                Some(LoadError::Line(line)),
                "{bad:?}"
            );
        }
        // Nothing kept yet: only the format line.
        let mut empty = String::new();
        Desk::default().write_text(&mut empty);
        assert_eq!(empty, "format 2\n");
        assert!(
            Desk::parse(&empty)
                .unwrap()
                .arrangement()
                .placements
                .is_empty()
        );
    }

    /// A monitor that comes back with another resolution keeps the size of a pixel.
    #[test]
    fn refit_keeps_the_size_of_a_pixel() {
        let landscape = arrange(Preset::Arc, &[(0, 2560, 1440)], Some(0), None, None)[&0];
        let rotated = refit(landscape, (2560, 1440), (1440, 2560));
        let density = |p: &Placement, w: u32| p.width / w as f32;
        assert!((density(&rotated, 1440) - density(&landscape, 2560)).abs() < 1e-9);
        assert_eq!(rotated.position, landscape.position);
        let unchanged = refit(landscape, (2560, 1440), (2560, 1440));
        assert_eq!(unchanged.width, landscape.width);
    }

    /// An arrangement laid out facing `yaw` reads back as facing `yaw`.
    #[test]
    fn facing_yaw_of_an_arrangement() {
        let row = arrange(
            Preset::Arc,
            &[(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)],
            Some(1),
            None,
            None,
        );
        let yaw = 0.8f32;
        let turn = Quat::from_rotation_y(yaw);
        let turned: HashMap<u32, Placement> = row
            .iter()
            .map(|(id, p)| {
                (
                    *id,
                    Placement {
                        position: turn * p.position,
                        rotation: turn * p.rotation,
                        width: p.width,
                    },
                )
            })
            .collect();
        let got = facing_yaw(&turned, Vec3::ZERO).unwrap();
        assert!((got - yaw).abs() < 1e-4, "got {got}");
    }

    /// `p` with every monitor carried from `from` to `to` (see [`carried`]).
    fn carried_all(
        p: &HashMap<u32, Placement>,
        from: (Vec3, f32),
        to: (Vec3, f32),
    ) -> HashMap<u32, Placement> {
        p.iter().map(|(k, q)| (*k, carried(q, from, to))).collect()
    }

    /// How far the corners of `a` seen from the eye of `ca` are from those of `b` seen
    /// from the eye of `cb`, each turned so the first monitor of `m` is straight ahead
    /// (metres): 0 when the user sees the one arrangement from the one eye as the other
    /// from the other.
    fn seen_apart(
        a: &HashMap<u32, Placement>,
        ca: (Vec3, f32),
        b: &HashMap<u32, Placement>,
        cb: (Vec3, f32),
        m: &[(u32, u32, u32)],
    ) -> f32 {
        let origin = (Vec3::ZERO, 0.0);
        let first = Some(m[0].0);
        let ca = (ca.0, ahead(a, &[], first, ca.0).unwrap());
        let cb = (cb.0, ahead(b, &[], first, cb.0).unwrap());
        m.iter()
            .map(|&(id, w, h)| {
                let (p, q) = (carried(&a[&id], ca, origin), carried(&b[&id], cb, origin));
                let tall = |x: &Placement| x.width * h as f32 / w as f32;
                corners(&p, tall(&p))
                    .iter()
                    .zip(corners(&q, tall(&q)))
                    .map(|(x, y)| x.distance(y))
                    .fold(0.0f32, f32::max)
            })
            .fold(0.0f32, f32::max)
    }

    /// "In front" as the headset does it: the arrangement turned about the eye of
    /// `center` so the primary monitor is straight ahead of a head at `head` looking
    /// along `yaw`, and carried to it; the new center.
    fn in_front(
        p: &HashMap<u32, Placement>,
        m: &[(u32, u32, u32)],
        primary: Option<u32>,
        center: (Vec3, f32),
        head: (Vec3, f32),
    ) -> (HashMap<u32, Placement>, (Vec3, f32)) {
        let ids: Vec<u32> = m.iter().map(|x| x.0).collect();
        let ahead = ahead(p, &ids, primary, center.0).unwrap_or(center.1);
        (carried_all(p, (center.0, ahead), head), head)
    }

    /// `id` let go by the magnet over (`up` 1) or under (-1) `under`, its middle `along`
    /// metres to the right of `under`'s, a hand's 3 cm off: glued edge to edge.
    fn glued(
        p: &mut HashMap<u32, Placement>,
        m: &[(u32, u32, u32)],
        id: u32,
        under: u32,
        up: f32,
        along: f32,
    ) {
        let tall = |id: u32, width: f32| {
            let &(_, w, h) = m.iter().find(|x| x.0 == id).unwrap();
            width * h as f32 / w as f32
        };
        let &(_, w, _) = m.iter().find(|x| x.0 == id).unwrap();
        let width = w as f32 * METRES_PER_PIXEL;
        let n = p[&under];
        let lift = (tall(under, n.width) + tall(id, width)) / 2.0 + 0.03;
        let at = n.position + n.rotation * Vec3::new(along, up * lift, 0.0);
        p.insert(
            id,
            Placement {
                position: at,
                rotation: facing(at, Vec3::new(0.0, at.y, 0.0)),
                width,
            },
        );
        let s = stick(p, m, Vec3::ZERO, id).expect("the magnet takes it");
        assert!(
            !s.across,
            "{id} glued over or under {}, not beside",
            s.neighbour
        );
        p.insert(id, s.placement);
    }

    /// The desktops of `a` seen from `ca` and of `b` seen from `cb` are the same: the
    /// same rows, and every monitor within a few pixels of the same spot (the eye carried
    /// along leaves the last bits of the floats).
    fn same_desktop(
        a: &HashMap<u32, Placement>,
        ca: (Vec3, f32),
        b: &HashMap<u32, Placement>,
        cb: (Vec3, f32),
        m: &[(u32, u32, u32)],
        primary: Option<u32>,
        what: &str,
    ) -> Desktop<u32> {
        let (da, db) = (
            Desktop::of(a, m, ca, Preset::Arc, primary, None),
            Desktop::of(b, m, cb, Preset::Arc, primary, None),
        );
        assert_eq!(da.rows, db.rows, "{what}: rows");
        for s in &da.plan {
            let t = db.plan.iter().find(|t| t.id == s.id).unwrap();
            assert!(
                (s.x - t.x).abs() <= 2 && (s.y - t.y).abs() <= 2,
                "{what}: {} at {},{} instead of {},{}",
                s.id,
                s.x,
                s.y,
                t.x,
                t.y
            );
        }
        da
    }

    /// Where each monitor stands.
    type Placements = HashMap<u32, Placement>;

    /// The owner's monitors of 2026-11-21: 1440x2560 | 2560x1440 primary | 2560x1440 |
    /// 1440x2560 by the preset, and the 3840x2160 let go by the magnet over the seam of
    /// the two landscape ones.
    fn owners_stack() -> (Vec<(u32, u32, u32)>, Placements) {
        let owner = vec![
            (2, 1440, 2560),
            (1, 2560, 1440),
            (3, 2560, 1440),
            (4, 1440, 2560),
            (6, 3840, 2160),
        ];
        let mut stack = arrange(Preset::Arc, &owner[..4], Some(1), None, None);
        let seam = stack[&1].width / 2.0;
        glued(&mut stack, &owner, 6, 1, 1.0, seam);
        (owner, stack)
    }

    /// The profile of `placements` seen from `center` with the rows the PC has
    /// (`Scene::current_profile`), written to its file and read back.
    fn saved_profile(
        m: &[(u32, u32, u32)],
        primary: Option<u32>,
        placements: &HashMap<u32, Placement>,
        center: (Vec3, f32),
        rows: &[Vec<u32>],
    ) -> crate::profiles::Profile {
        let live = Arrangement {
            placements: placements.clone(),
            sizes: HashMap::new(),
            center: Some(center),
            radius: Some(RADIUS),
        };
        let profile =
            crate::profiles::Profile::of(&modes(m), primary, Vec::new(), &live, None, 0.0, rows);
        let mut all = crate::profiles::empty();
        all[0] = Some(profile);
        let [back, ..] = reread_profiles(&all);
        back.unwrap()
    }

    /// `profile` switched to with the user's center at `now`, the PC giving its
    /// monitors the numbers they had (as it does for monitors of the same size): the
    /// placements and the rows the headset then has (`Scene::profile_made`).
    fn switched_to(
        profile: &crate::profiles::Profile,
        m: &[(u32, u32, u32)],
        now: (Vec3, f32),
    ) -> (HashMap<u32, Placement>, Option<Desktop<u32>>) {
        let made: Vec<(u32, Placement)> = m
            .iter()
            .map(|x| x.0)
            .zip(profile.places.iter().copied())
            .collect();
        let (placed, rows) = crate::profiles::standing(&made, profile.tilt, &profile.rows, now);
        (placed.into_iter().collect(), Desktop::history(rows))
    }

    /// 2026-11-21, the owner (dev build 74dfe1d): after switching to a profile, "Line
    /// up" or "In front" sent monitors glued one over another flying, while monitors
    /// glued side by side held. A profile kept its places in the room but not the eye
    /// they were laid out around: once the user had turned the arrangement in front
    /// of them elsewhere (or the headset's room was set anew), everything worked out as
    /// seen from the eye (the rows the PC gets, "Line up", "In front") saw the profile
    /// from another place. Rows side by side close up whatever the eye, rows one over
    /// another are kept apart as seen and so came apart.
    ///
    /// What this covers: a profile switched to is seen from the user's eye as it was
    /// from the eye it was saved around, so what lining up and "In front" do to it after
    /// the switch is what they did to it before (in either order), and the PC gets the
    /// desktop it had. For stacks, rows side by side, portraits among landscapes and a
    /// 3840x2160 over two monitors, each lined up once before it is saved. What the
    /// first "Line up" does to a monitor the magnet glued is not covered here (see
    /// `report_first_line_up_of_a_3840_over_two`).
    #[test]
    fn a_profile_switched_to_lines_up_and_turns_as_it_did_before() {
        // Where the user sat when saving, and later (turned in front elsewhere, or
        // the headset's room set anew): 35 cm aside, lower, further back, turned 75°.
        let saved_at = (Vec3::new(0.2, 1.15, -0.1), 0.4);
        let later = (Vec3::new(0.55, 1.03, 0.15), 0.4 + 1.3);
        let head = |c: (Vec3, f32)| (c.0 + Vec3::new(0.04, 0.02, -0.03), c.1 - 0.6);
        let (owner, stack) = owners_stack();
        // Side by side only.
        let row = [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)];
        let side = arrange(Preset::Arc, &row, Some(1), None, None);
        // Mixed: two landscape side by side, a 1920x1080 glued over the left one and a
        // 2560x1440 under the right one.
        let mixed_m = [
            (0, 2560, 1440),
            (1, 2560, 1440),
            (5, 1920, 1080),
            (7, 2560, 1440),
        ];
        let mut mixed = arrange(Preset::Arc, &mixed_m[..2], Some(0), None, None);
        glued(&mut mixed, &mixed_m, 5, 0, 1.0, 0.0);
        glued(&mut mixed, &mixed_m, 7, 1, -1.0, -0.1);
        // Two rows by the preset (six do not fit one).
        let six = [
            (0, 2560, 1440),
            (1, 2560, 1440),
            (2, 1440, 2560),
            (3, 2560, 1440),
            (4, 2560, 1440),
            (5, 3840, 2160),
        ];
        let two_rows = arrange(Preset::Arc, &six, Some(2), None, None);
        let scenes = [
            ("a 3840x2160 over two", &owner[..], 1, stack),
            ("side by side", &row[..], 1, side),
            ("over and under", &mixed_m[..], 0, mixed),
            ("two rows of a preset", &six[..], 2, two_rows),
        ];
        for (name, m, primary, by_hand) in scenes {
            let primary = Some(primary);
            let origin = (Vec3::ZERO, 0.0);
            // Lined up once, as the user keeps it, where they sat.
            let kept = carried_all(
                &line_up(&by_hand, m, origin, Preset::Arc, primary, &[], None),
                origin,
                saved_at,
            );
            let desktop = same_desktop(&kept, saved_at, &kept, saved_at, m, primary, name);
            if name == "a 3840x2160 over two" {
                // Over the two, touching both on the desktop.
                assert_eq!(desktop.rows, vec![vec![6], vec![2, 1, 3, 4]]);
                let spot = |id| desktop.plan.iter().find(|s| s.id == id).copied().unwrap();
                for below in [1, 3] {
                    assert_eq!(
                        spot(6).crossing(&spot(below)),
                        Some(prostir_layout::space::Side::Below),
                        "{name}: 6 over {below}"
                    );
                }
            }
            let check = |p: &HashMap<u32, Placement>, at: (Vec3, f32), what: &str| {
                let off = seen_apart(p, at, &kept, saved_at, m);
                assert!(off < 2e-3, "{name}, {what}: {off} m from how it was");
                same_desktop(
                    p,
                    at,
                    &kept,
                    saved_at,
                    m,
                    primary,
                    &format!("{name}, {what}"),
                );
            };
            let profile = saved_profile(m, primary, &kept, saved_at, &desktop.rows);
            for now in [saved_at, later] {
                let (back, sent) = switched_to(&profile, m, now);
                assert_eq!(
                    sent.as_ref().map(|d| &d.rows),
                    Some(&desktop.rows),
                    "{name}: its rows"
                );
                check(&back, now, "switched to");
                // Then "Line up".
                let rows = Desktop::of(&back, m, now, Preset::Arc, primary, sent.as_ref()).rows;
                let lined = line_up(&back, m, now, Preset::Arc, primary, &rows, None);
                check(&lined, now, "switched to, lined up");
                // Then "In front", and "Line up" after it.
                let (front, at) = in_front(&back, m, primary, now, head(now));
                check(&front, at, "switched to, in front");
                let rows = Desktop::of(&front, m, at, Preset::Arc, primary, sent.as_ref()).rows;
                let lined = line_up(&front, m, at, Preset::Arc, primary, &rows, None);
                check(&lined, at, "switched to, in front, lined up");
                // The other order: "In front" and "Line up" first, saved, and back.
                let (front, at) = in_front(&kept, m, primary, saved_at, head(now));
                let rows = Desktop::of(&front, m, at, Preset::Arc, primary, None).rows;
                let lined = line_up(&front, m, at, Preset::Arc, primary, &rows, None);
                let again = saved_profile(m, primary, &lined, at, &rows);
                let (back, _) = switched_to(&again, m, later);
                check(&back, later, "in front, lined up, saved, switched to");
            }
        }
    }

    /// In cinema the profile keeps the desk arrangement saved before it (with its
    /// center and circle), not the cinema view; otherwise the arrangement as it stands.
    #[test]
    fn in_cinema_a_profile_keeps_the_desk_saved_before_it() {
        let three = [(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)];
        let at_desk = arrange(Preset::Arc, &three, Some(1), None, None);
        let desk_center = (Vec3::new(0.1, 1.2, 0.3), 0.5);
        let desk = Arrangement {
            placements: at_desk.clone(),
            sizes: HashMap::new(),
            center: Some(desk_center),
            radius: Some(1.3),
        };
        let view = cinema(&three, None);
        let live = |placements: &HashMap<u32, Placement>| Arrangement {
            placements: placements.clone(),
            sizes: HashMap::new(),
            center: Some((Vec3::ZERO, 0.0)),
            radius: Some(RADIUS),
        };
        let rows = vec![vec![0, 1, 2]];
        let m = modes(&three);
        let of = |live: &Arrangement<u32>, desk: Option<&Arrangement<u32>>| {
            crate::profiles::Profile::of(&m, Some(1), Vec::new(), live, desk, 0.0, &rows)
        };
        let profile = of(&live(&view), Some(&desk));
        assert_eq!(profile.places.len(), 3);
        let frame = Frame::upright(desk_center.0, desk_center.1);
        for (x, p) in three.iter().zip(&profile.places) {
            let saved = frame.unplace(at_desk[&x.0]);
            assert!(
                p.position.distance(saved.position) < 1e-5
                    && p.rotation.abs_diff_eq(saved.rotation, 1e-5),
                "{}",
                x.0
            );
        }
        assert_eq!(profile.radius, 1.3);
        assert_eq!(profile.rows, vec![vec![0, 1, 2]]);
        // Not in cinema: as it stands.
        let profile = of(&live(&at_desk), None);
        assert_eq!(profile.radius, RADIUS);
        assert!(profile.places[1].position.distance(at_desk[&1].position) < 1e-5);
        // Not every monitor placed: no places, no rows.
        let profile = of(&live(&view), None);
        assert!(profile.places.is_empty() && profile.rows.is_empty());
    }

    /// The rows of a profile switched to are its own. The PC gives a monitor of the same
    /// size the number it had in the profile before, so the rows sent then would keep
    /// a pair of neighbours of that arrangement a pair in this one: the 3840x2160 over
    /// the landscape ones, its edge beside the top of the portrait on the right, taken
    /// into the portrait's row. With the rows it was saved with (or, saved without
    /// them, none) it is over the others, as it was.
    #[test]
    fn a_profile_switched_to_keeps_its_own_rows() {
        let center = (Vec3::ZERO, 0.0);
        let owner = [
            (2, 1440, 2560),
            (1, 2560, 1440),
            (3, 2560, 1440),
            (4, 1440, 2560),
            (6, 3840, 2160),
        ];
        let mut p = arrange(Preset::Arc, &owner[..4], Some(1), None, None);
        // Over 3, its right edge where 3's is, its bottom on 3's top.
        let under = p[&3];
        let (w, h) = (
            3840.0 * METRES_PER_PIXEL,
            3840.0 * METRES_PER_PIXEL * 2160.0 / 3840.0,
        );
        let at = under.position
            + under.rotation
                * Vec3::new(
                    (under.width - w) / 2.0,
                    (under.width * 1440.0 / 2560.0 + h) / 2.0 + 0.002,
                    0.0,
                );
        p.insert(
            6,
            Placement {
                position: at,
                rotation: under.rotation,
                width: w,
            },
        );
        let fresh = Desktop::of(&p, &owner, center, Preset::Arc, Some(1), None);
        assert_eq!(fresh.rows, vec![vec![6], vec![2, 1, 3, 4]]);
        // The rows sent for the profile before, where 6 was the portrait's neighbour.
        let before = Desktop::history(vec![vec![2, 1, 3, 6, 4]]);
        let stale = Desktop::of(&p, &owner, center, Preset::Arc, Some(1), before.as_ref());
        assert_ne!(stale.rows, fresh.rows, "the history of another arrangement");
        // Saved with its rows, switched to: its rows.
        let profile = saved_profile(&owner, Some(1), &p, center, &fresh.rows);
        let (back, sent) = switched_to(&profile, &owner, center);
        let own = Desktop::of(&back, &owner, center, Preset::Arc, Some(1), sent.as_ref());
        assert_eq!(own.rows, fresh.rows);
        assert_eq!(own.plan, fresh.plan, "the plan made afresh");
        // Saved without rows: they start afresh.
        let mut without = profile;
        without.rows.clear();
        let (back, sent) = switched_to(&without, &owner, center);
        assert!(sent.is_none());
        assert_eq!(
            Desktop::of(&back, &owner, center, Preset::Arc, Some(1), None).rows,
            fresh.rows
        );
    }

    /// The gaps as seen between the owner's 3840x2160 (6) and each of the two landscape
    /// monitors under it (1, 3), in the middle of the edge each shares with it, as a
    /// share of that one's height (negative where they overlap).
    fn seams_of_6(p: &Placements, owner: &[(u32, u32, u32)]) -> Vec<f64> {
        use prostir_layout::desk::View;
        let all = placed(p, owner);
        let room = Room::new(&frame((Vec3::ZERO, 0.0), Preset::Arc), &all);
        [1, 3]
            .iter()
            .map(|&below| {
                room.relate(below, 6, prostir_layout::space::Side::Above)
                    .filter(|r| r.shared > 0.0)
                    .expect("6 over it")
                    .apart
            })
            .collect()
    }

    /// 2026-11-21, the owner's decision: a monitor glued by the magnet over or under a
    /// row stays glued when "Line up" straightens it. The owner's 3840x2160 over the
    /// two landscape monitors of an arc, let go as the hand leaves it (turned to face the
    /// axis) and stood upright on their tops, each time glued by the magnet: the first
    /// "Line up" leaves the middle of the wider seam closed and the other overlapping a
    /// little (before, let go by hand, it lifted it 13 cm and opened both by 3.8 and 5 %
    /// of their height), the ends of the flat monitor overlapping the two a little as seen, and
    /// a second changes nothing. Stood upright on their tops without the magnet it is not
    /// glued, and is pushed clear of them as before, a gap in the middle of both seams.
    #[test]
    fn line_up_keeps_a_monitor_glued_over_two_glued() {
        let center = (Vec3::ZERO, 0.0);
        let (owner, by_hand) = owners_stack();
        let row = arrange(Preset::Arc, &owner[..4], Some(1), None, None);
        let mut upright = row.clone();
        {
            let (one, three) = (row[&1], row[&3]);
            let shelf = one.position.y + one.width * 1440.0 / 2560.0 / 2.0;
            let mid = (one.position + three.position) / 2.0;
            let h = 3840.0 * METRES_PER_PIXEL * 2160.0 / 3840.0;
            let at = Vec3::new(mid.x, shelf + h / 2.0, mid.z);
            upright.insert(
                6,
                Placement {
                    position: at,
                    rotation: facing(at, Vec3::new(0.0, at.y, 0.0)),
                    width: 3840.0 * METRES_PER_PIXEL,
                },
            );
        }
        let mut glued_upright = upright.clone();
        let s = stick(&glued_upright, &owner, Vec3::ZERO, 6).expect("sticks");
        assert!(!s.across);
        glued_upright.insert(6, s.placement);
        let lined = |p: &Placements| {
            let rows = Desktop::of(p, &owner, center, Preset::Arc, Some(1), None).rows;
            assert_eq!(rows, vec![vec![6], vec![2, 1, 3, 4]]);
            line_up(p, &owner, center, Preset::Arc, Some(1), &rows, None)
        };
        for (name, p) in [("let go by hand", by_hand), ("upright", glued_upright)] {
            let once = lined(&p);
            let seams = seams_of_6(&once, &owner);
            let widest = seams.iter().copied().fold(f64::MIN, f64::max);
            assert!(widest.abs() < 1e-3, "{name}: seams {seams:?}");
            assert!(seams.iter().all(|s| *s > -0.03), "{name}: seams {seams:?}");
            let rise = once[&6].position.y - p[&6].position.y;
            // Only as far as closing the seam takes it (the magnet left it over the right
            // one, as seen): it was 13 cm, with the seams open.
            assert!(rise.abs() < 0.1, "{name}: rose {rise} m");
            for id in [2, 1, 3, 4] {
                assert!(once[&id].position.distance(p[&id].position) < 1e-4);
            }
            let twice = lined(&once);
            assert!(
                twice[&6].position.distance(once[&6].position) < 1e-4,
                "{name}"
            );
            // Its ends overlap the two as seen, by no more than OVERLAP_MAX.
            let all = placed(&once, &owner);
            let of = |id: u32| *all.iter().find(|m| m.id == id).unwrap();
            let deepest = [1, 3]
                .iter()
                .flat_map(|&below| {
                    [
                        prostir_layout::view::reach_pair(
                            &of(below),
                            &of(6),
                            prostir_layout::space::Side::Above,
                            17,
                        ),
                        prostir_layout::view::reach_pair(
                            &of(6),
                            &of(below),
                            prostir_layout::space::Side::Below,
                            17,
                        ),
                    ]
                })
                .flatten()
                .fold(0.0f64, f64::max);
            assert!(
                deepest <= f64::from(tidy::OVERLAP_MAX),
                "{name}: overlaps {deepest}"
            );
            eprintln!(
                "{name}: rose {rise:.3} m, seams {seams:.4?}, overlapping {:.1} % at most",
                deepest * 100.0
            );
        }
        // Not glued: pushed clear as before, covering neither.
        let once = lined(&upright);
        let corners = |p: &Placements, id: u32| {
            let &(_, w, h) = owner.iter().find(|m| m.0 == id).unwrap();
            let q = p[&id];
            Placement {
                position: q.position,
                rotation: q.rotation,
                width: q.width,
            }
            .corners(q.width * h as f32 / w as f32)
        };
        for below in [1, 3] {
            assert!(!prostir_layout::space::covers(
                &corners(&once, below),
                &corners(&once, 6),
                0.001
            ));
        }
        assert!(
            once[&6].position.y > upright[&6].position.y,
            "pushed up clear"
        );
        let seams = seams_of_6(&once, &owner);
        assert!(
            seams.iter().all(|s| *s > 0.0),
            "a gap in the middle: {seams:?}"
        );
    }

    /// The farthest any corner of `a` is from the same corner in `b` (metres).
    fn farthest_corner(a: &Placements, b: &Placements, m: &[(u32, u32, u32)]) -> f32 {
        m.iter()
            .map(|&(id, w, h)| {
                let (p, q) = (a[&id], b[&id]);
                corners(&p, p.width * h as f32 / w as f32)
                    .iter()
                    .zip(corners(&q, q.width * h as f32 / w as f32))
                    .map(|(x, y)| x.distance(y))
                    .fold(0.0f32, f32::max)
            })
            .fold(0.0f32, f32::max)
    }

    /// The draw order keeps two monitors at about one distance from the head in the
    /// order they had (a monitor glued over a row and that row): the head moving so
    /// their distances cross back and forth within DRAW_HOLD never swaps them, so where
    /// they overlap never flickers; one clearly nearer goes last; one held goes last of
    /// all, and new ones go by distance.
    #[test]
    fn the_draw_order_holds_for_monitors_at_one_distance() {
        let mut before: Vec<u32> = Vec::new();
        let mut order: Vec<(f32, u32)> = Vec::new();
        let mut draw = |d: &[(f32, u32)], before: &mut Vec<u32>| {
            order.clear();
            order.extend_from_slice(d);
            draw_order(&mut order, before);
            before.clear();
            before.extend(order.iter().map(|o| o.1));
            before.clone()
        };
        // New: far to near.
        assert_eq!(
            draw(&[(0.8, 6), (1.2, 1), (1.0, 3)], &mut before),
            vec![1, 3, 6]
        );
        // 6 and 3 crossing by up to 4 cm each way, frame after frame: no swap.
        for f in 0..200 {
            let wobble = if f % 2 == 0 { 0.02 } else { -0.02 };
            let got = draw(
                &[(1.0 + wobble, 6), (1.2, 1), (1.0 - wobble, 3)],
                &mut before,
            );
            assert_eq!(got, vec![1, 3, 6], "frame {f}");
        }
        // 3 clearly nearer: it goes last.
        assert_eq!(
            draw(&[(1.0, 6), (1.2, 1), (0.9, 3)], &mut before),
            vec![1, 6, 3]
        );
        // Held in a hand: last of all.
        assert_eq!(
            draw(&[(1.0, 6), (-1.0, 1), (0.9, 3)], &mut before),
            vec![6, 3, 1]
        );
        // A new one far away: first.
        assert_eq!(
            draw(&[(1.0, 6), (1.2, 1), (0.9, 3), (2.0, 9)], &mut before),
            vec![9, 1, 6, 3]
        );
    }

    /// Follow mode carries the arrangement after the head a little every frame: after
    /// a long while of it (60 000 frames, about 14 minutes at 72 Hz, turning and moving
    /// back and forth) the owner's 3840x2160 glued over the two is still seen glued, and
    /// "Line up" moves nothing, the last bits of the floats notwithstanding.
    #[test]
    fn a_long_follow_leaves_a_glued_seam_glued() {
        let origin = (Vec3::ZERO, 0.0);
        let (owner, by_hand) = owners_stack();
        let rows = Desktop::of(&by_hand, &owner, origin, Preset::Arc, Some(1), None).rows;
        let kept = line_up(&by_hand, &owner, origin, Preset::Arc, Some(1), &rows, None);
        let (mut p, mut center) = (kept.clone(), origin);
        let mut rng = Rng(0xf011);
        for _ in 0..60_000 {
            // As `Scene::follow_head` steps: a turn and a shift of a frame's easing.
            let turn = rng.float(-0.01, 0.01);
            let shift = Vec3::new(rng.float(-0.003, 0.003), 0.0, rng.float(-0.003, 0.003));
            let next = (center.0 + shift, center.1 + turn);
            for q in p.values_mut() {
                *q = carried(q, (center.0, 0.0), (next.0, turn));
            }
            center = next;
        }
        assert!(
            seen_apart(&p, center, &kept, origin, &owner) < 1e-3,
            "followed as one piece"
        );
        let lined = line_up(&p, &owner, center, Preset::Arc, Some(1), &rows, None);
        let moved = farthest_corner(&p, &lined, &owner);
        assert!(moved < 1e-4, "Line up after following moves {moved} m");
    }
}
