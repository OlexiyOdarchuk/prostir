//! Where monitors float in the room, how controllers move them, and persistence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use glam::{Quat, Vec2, Vec3};
use prostir_proto::is_window_panel;

use crate::settings::Preset;

mod arc;
mod rows;
pub use arc::{MAX_ARC, MAX_RADIUS, Rows, angular_width, arc_radius, split, wall_width};
pub use rows::{Seen, desktop_spots, layout_rows, place_new, rows_of, seen, tidy};

/// Monitor size in metres per stream pixel: every monitor gets the same pixel
/// density, so neighbours line up like one big screen (2560 px ≈ 1.25 m).
pub const METRES_PER_PIXEL: f32 = 1.25 / 2560.0;
/// Distance from the head to the monitors' edges, nearest (see [`arc`]).
pub const RADIUS: f32 = 1.1;
/// The main row sits a little below eye height.
const EYE_DROP: f32 = 0.08;
/// How far up the "overhead" layout is tilted, for lying down.
pub const OVERHEAD_PITCH: f32 = 0.95;
/// The laptop mirror is shown at most this wide.
const MIRROR_MAX_WIDTH: f32 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub position: Vec3,
    pub rotation: Quat,
    /// Width in metres; height follows the stream's aspect ratio.
    pub width: f32,
}

/// Rotation that makes a quad (front face along +Z) look at `target`, without roll.
pub fn facing(position: Vec3, target: Vec3) -> Quat {
    let dir = (target - position).normalize_or(Vec3::Z);
    let yaw = dir.x.atan2(dir.z);
    let pitch = dir.y.clamp(-1.0, 1.0).asin();
    Quat::from_rotation_y(yaw) * Quat::from_rotation_x(-pitch)
}

/// An upright monitor on the cylinder of `radius` around the head, its edges on the
/// cylinder, centred at `yaw` (0 = straight ahead, positive = right) and height `y`.
fn on_cylinder(yaw: f32, y: f32, width: f32, radius: f32) -> Placement {
    let half = angular_width(width, radius) / 2.0;
    let r = radius * half.cos();
    let position = Vec3::new(r * yaw.sin(), y, -r * yaw.cos());
    Placement {
        position,
        rotation: facing(position, Vec3::new(0.0, y, 0.0)),
        width,
    }
}

/// `(id, width_m, centre height)` side by side without gaps on the cylinder of
/// `radius`, centred straight ahead.
fn row(items: &[(u8, f32, f32)], radius: f32, out: &mut HashMap<u8, Placement>) {
    let total: f32 = items.iter().map(|m| angular_width(m.1, radius)).sum();
    let mut yaw = -total / 2.0;
    for &(id, w, y) in items {
        let a = angular_width(w, radius);
        out.insert(id, on_cylinder(yaw + a / 2.0, y, w, radius));
        yaw += a;
    }
}

fn yaw_of(p: &Placement) -> f32 {
    p.position.x.atan2(-p.position.z)
}

/// Places every monitor for a preset. `monitors` and `mirror` hold
/// `(id, width_px, height_px)`; Quest monitors go left to right in the given order (as in KDE),
/// and the laptop mirror goes under the primary monitor.
/// Left-to-right order of monitors as they stand around the user (by direction).
pub fn order_by_direction(placements: &HashMap<u8, Placement>, ids: &[u8]) -> Vec<u8> {
    let mut v: Vec<(f32, u8)> = ids
        .iter()
        .filter_map(|id| placements.get(id).map(|p| (yaw_of(p), *id)))
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    v.into_iter().map(|(_, id)| id).collect()
}

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
type Part = (u8, Placement, f32, f32);

fn parts(placements: &HashMap<u8, Placement>, monitors: &[(u8, u32, u32)]) -> Vec<Part> {
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
fn nearest(m: &Part, others: &[Part], eye: Vec3) -> Option<(Part, Beside)> {
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
fn settle_along(b: &Beside, n: &Part, mw: f32, mh: f32) -> f32 {
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
pub struct Stick {
    pub placement: Placement,
    pub neighbour: u8,
    /// Side by side (else above or below).
    pub across: bool,
    /// Which side of the neighbour: 1 right or above, -1 left or below.
    pub side: f32,
}

/// The magnet for one monitor: let go almost against another (within 15 % of a
/// monitor, as seen from `eye`) and at about the same distance, where it would stick
/// to it, edge to edge, where along the edge it is (see [`hinge`]); `None` otherwise.
pub fn stick(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    eye: Vec3,
    id: u8,
) -> Option<Stick> {
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
pub fn stick_on_wall(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    center: (Vec3, f32),
    id: u8,
) -> Option<Stick> {
    let unturn = Quat::from_rotation_y(-center.1);
    let turn = Quat::from_rotation_y(center.1);
    // (id, local centre, width, height)
    let all: Vec<(u8, Vec3, f32, f32)> = monitors
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
    let sep = |o: &(u8, Vec3, f32, f32)| {
        let (gx, gy) = gaps(at, mw, mh, o.1, o.2, o.3);
        (gx.max(gy).max(0.0), at.distance(o.1))
    };
    let others: Vec<(u8, Vec3, f32, f32)> = all.iter().filter(|o| o.0 != id).copied().collect();
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

/// The A button on a wall: everything into one plane (at the monitors' average
/// depth), facing the same way; rows side by side, centred where they were, stacked to
/// touch.
pub fn tidy_wall(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    center: (Vec3, f32),
) -> HashMap<u8, Placement> {
    let unturn = Quat::from_rotation_y(-center.1);
    let turn = Quat::from_rotation_y(center.1);
    // (id, local position, width, height)
    let items: Vec<(u8, Vec3, f32, f32)> = monitors
        .iter()
        .filter_map(|&(id, w, h)| {
            let p = placements.get(&id)?;
            let local = unturn * (p.position - center.0);
            Some((id, local, p.width, p.width * h as f32 / w.max(1) as f32))
        })
        .collect();
    if items.is_empty() {
        return HashMap::new();
    }
    let depth = items.iter().map(|m| m.1.z).sum::<f32>() / items.len() as f32;
    // The rows as seen (see `rows_of`), top to bottom.
    let rows: Vec<Vec<(u8, Vec3, f32, f32)>> =
        rows_of(&seen(placements, monitors, center, Preset::Wall))
            .iter()
            .map(|r| {
                r.iter()
                    .filter_map(|m| items.iter().find(|it| it.0 == m.id).copied())
                    .collect()
            })
            .collect();
    let mut ys: Vec<(f32, f32)> = rows
        .iter()
        .map(|r| {
            let y = r.iter().map(|m| m.1.y).sum::<f32>() / r.len() as f32;
            (y, r.iter().map(|m| m.3).fold(0.0, f32::max))
        })
        .collect();
    for i in 1..ys.len() {
        let above_bottom = ys[i - 1].0 - ys[i - 1].1 / 2.0;
        if above_bottom - (ys[i].0 + ys[i].1 / 2.0) < 0.15 * ys[i].1 {
            ys[i].0 = above_bottom - ys[i].1 / 2.0;
        }
    }
    let mut out = HashMap::new();
    for (mut row, (y, _)) in rows.into_iter().zip(ys) {
        row.sort_by(|a, b| a.1.x.total_cmp(&b.1.x));
        let left = row[0].1.x - row[0].2 / 2.0;
        let last = &row[row.len() - 1];
        let mid = (left + last.1.x + last.2 / 2.0) / 2.0;
        let total: f32 = row.iter().map(|m| m.2).sum();
        let mut x = mid - total / 2.0;
        for (id, _, w, _) in row {
            let local = Vec3::new(x + w / 2.0, y, depth);
            out.insert(
                id,
                Placement {
                    position: center.0 + turn * local,
                    rotation: turn,
                    width: w,
                },
            );
            x += w;
        }
    }
    out
}

/// Puts the laptop mirror right under `primary`, on its plane, keeping its own width.
pub fn under(
    primary: &Placement,
    primary_height: f32,
    mirror: &Placement,
    mirror_height: f32,
) -> Placement {
    let down = primary.rotation * Vec3::Y;
    Placement {
        position: primary.position - down * (primary_height / 2.0 + mirror_height / 2.0),
        rotation: primary.rotation,
        width: mirror.width,
    }
}

/// The monitor shown in cinema mode: the one at position `pick` in the row, else the
/// largest landscape one (films are wide), else the first.
pub fn cinema_choice(monitors: &[(u8, u32, u32)], pick: Option<u8>) -> Option<u8> {
    pick.and_then(|i| monitors.get(i as usize))
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
pub fn cinema(monitors: &[(u8, u32, u32)], pick: Option<u8>) -> HashMap<u8, Placement> {
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

/// How far from the head `preset` puts `monitors` (see [`arc`]).
pub fn preset_radius(preset: Preset, monitors: &[(u8, u32, u32)]) -> f32 {
    let widths: Vec<f32> = monitors
        .iter()
        .map(|m| m.1 as f32 * METRES_PER_PIXEL)
        .collect();
    split(preset, &widths, None).radius
}

pub fn arrange(
    preset: Preset,
    monitors: &[(u8, u32, u32)],
    primary: Option<u8>,
    mirror: Option<(u8, u32, u32)>,
) -> HashMap<u8, Placement> {
    arrange_curved(preset, monitors, primary, mirror, None)
}

/// [`arrange`] for monitors drawn curved with cylinder radius `curve` (metres): the
/// laptop mirror then continues the cylinder of the monitor above it.
pub fn arrange_curved(
    preset: Preset,
    monitors: &[(u8, u32, u32)],
    primary: Option<u8>,
    mirror: Option<(u8, u32, u32)>,
    curve: Option<f32>,
) -> HashMap<u8, Placement> {
    let sorted: Vec<(u8, f32, f32)> = monitors
        .iter()
        .map(|&(id, w, h)| (id, w as f32 * METRES_PER_PIXEL, h as f32 * METRES_PER_PIXEL))
        .collect();
    let height_of = |id: u8| sorted.iter().find(|m| m.0 == id).map_or(0.0, |m| m.2);

    let mut out = HashMap::new();
    let widths: Vec<f32> = sorted.iter().map(|m| m.1).collect();
    let rows = split(
        preset,
        &widths,
        sorted.iter().position(|m| Some(m.0) == primary),
    );
    let radius = rows.radius;
    // The main row at eye level, its middles lined up; the others above it in order,
    // bottoms lined up on the top of the tallest monitor under them.
    let main_y = if preset == Preset::Overhead {
        0.0
    } else {
        -EYE_DROP
    };
    let tallest =
        |r: &std::ops::Range<usize>| sorted[r.clone()].iter().map(|m| m.2).fold(0.0, f32::max);
    let mut levels: Vec<(std::ops::Range<usize>, Option<f32>)> = Vec::new();
    let mut line = main_y + tallest(&rows.groups[rows.main]) / 2.0;
    levels.push((rows.groups[rows.main].clone(), None));
    for (i, g) in rows.groups.iter().enumerate().rev() {
        if i != rows.main {
            levels.push((g.clone(), Some(line)));
            line += tallest(g);
        }
    }
    for (group, bottom) in levels {
        let items: Vec<(u8, f32, f32)> = sorted[group]
            .iter()
            .map(|&(id, w, h)| (id, w, bottom.map_or(main_y, |b| b + h / 2.0)))
            .collect();
        match preset {
            Preset::Arc | Preset::Overhead => row(&items, radius, &mut out),
            Preset::Wall => {
                // Side by side in one plane straight ahead, all facing the same way.
                let total: f32 = items.iter().map(|m| m.1).sum();
                let mut x = -total / 2.0;
                for &(id, w, y) in &items {
                    out.insert(
                        id,
                        Placement {
                            position: Vec3::new(x + w / 2.0, y, -radius),
                            rotation: Quat::IDENTITY,
                            width: w,
                        },
                    );
                    x += w;
                }
            }
        }
    }

    if let Some((id, w, h)) = mirror {
        let (mut w, mut h) = (w as f32 * METRES_PER_PIXEL, h as f32 * METRES_PER_PIXEL);
        if w > MIRROR_MAX_WIDTH {
            h *= MIRROR_MAX_WIDTH / w;
            w = MIRROR_MAX_WIDTH;
        }
        let anchor = primary
            .and_then(|p| out.get(&p).map(|pl| (*pl, height_of(p))))
            .or_else(|| {
                let mid = sorted.get(sorted.len().saturating_sub(1) / 2)?;
                Some((*out.get(&mid.0)?, mid.2))
            });
        let yaw = anchor.map_or(0.0, |(p, _)| yaw_of(&p));
        // Below every monitor it could cover: those whose arc overlaps its own.
        let reach = angular_width(w, radius) / 2.0;
        let bottom = sorted
            .iter()
            .filter_map(|m| {
                let p = out.get(&m.0)?;
                let overlaps = (yaw_of(p) - yaw).abs() < reach + angular_width(m.1, radius) / 2.0;
                overlaps.then_some(p.position.y - m.2 / 2.0)
            })
            .fold(f32::INFINITY, f32::min);
        let bottom = if bottom.is_finite() {
            bottom
        } else {
            -EYE_DROP
        };
        // Curved: a tilted cylinder cannot meet the curved edge above along its whole
        // width, so the mirror continues the same cylinder downwards, like the lower
        // part of one tall curved screen.
        if let (Some(radius), Some((above, _))) = (curve, anchor) {
            let normal = above.rotation * Vec3::Z;
            let half_above = (above.width / 2.0).min(radius * 0.999);
            let axis = above.position + normal * (radius * radius - half_above * half_above).sqrt();
            let half = (w / 2.0).min(radius * 0.999);
            let mut position = axis - normal * (radius * radius - half * half).sqrt();
            position.y = bottom - h / 2.0;
            out.insert(
                id,
                Placement {
                    position,
                    rotation: above.rotation,
                    width: w,
                },
            );
            return finish(preset, out);
        }
        // Flat: hinged at its top edge to the bottom edge of the monitor above (no seam)
        // and tilted back towards the eyes like a laptop lid, since it sits below them.
        let top = match anchor {
            Some((p, _)) => Vec3::new(p.position.x, bottom, p.position.z),
            None => on_cylinder(yaw, bottom, w, radius).position,
        };
        let mut position = top - Vec3::Y * h / 2.0;
        let mut rotation = facing(position, Vec3::ZERO);
        for _ in 0..4 {
            position = top + rotation * Vec3::new(0.0, -h / 2.0, 0.0);
            rotation = facing(position, Vec3::ZERO);
        }
        out.insert(
            id,
            Placement {
                position,
                rotation,
                width: w,
            },
        );
    }

    finish(preset, out)
}

/// Tilts everything up for the overhead preset.
fn finish(preset: Preset, mut out: HashMap<u8, Placement>) -> HashMap<u8, Placement> {
    if preset == Preset::Overhead {
        let tilt = Quat::from_rotation_x(OVERHEAD_PITCH);
        for p in out.values_mut() {
            p.position = tilt * p.position;
            p.rotation = tilt * p.rotation;
        }
    }
    out
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

/// A monitor held by one controller.
#[derive(Debug, Clone, Copy)]
pub struct Grab {
    pub id: u8,
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
    pub fn start(id: u8, distance: f32, local_hit: Vec2, placement: &Placement) -> Self {
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
        let mut g = Grab::start(1, 0.2, Vec2::ZERO, &p).with_reach(hand, head);
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

/// The desk arrangement kept on disk: where the user put the monitors and the laptop
/// screen. Cinema mode shows its own one-screen view instead, and that view never
/// takes the desk arrangement's place here.
pub struct Desk {
    path: PathBuf,
}

/// A stream's size in pixels, width then height.
pub type PixelSize = (u32, u32);

/// The desk arrangement as `layout.txt` keeps it.
#[derive(Default)]
pub struct Arrangement {
    pub placements: HashMap<u8, Placement>,
    /// The stream size each placement was made for. Files from 0.1.0 have none.
    pub sizes: HashMap<u8, PixelSize>,
    /// The eye and the yaw the arrangement was laid out around. Files from 0.1.0 have
    /// none.
    pub center: Option<(Vec3, f32)>,
    /// The circle it stands on. Files from before 0.2.0 have none ([`RADIUS`]).
    pub radius: Option<f32>,
}

impl Desk {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The saved arrangement. Window panels are placed afresh when they appear.
    pub fn load(&self) -> Arrangement {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Arrangement::default();
        };
        let mut saved = Arrangement::default();
        for line in text.lines() {
            if let Some(center) = parse_center(line) {
                saved.center = Some(center);
            } else if let Some(r) = line
                .strip_prefix("radius ")
                .and_then(|r| r.trim().parse::<f32>().ok())
                .filter(|r| r.is_finite() && *r > 0.1)
            {
                saved.radius = Some(r);
            } else if let Some((id, p, size)) = parse_placement(line)
                && !is_window_panel(id)
            {
                saved.placements.insert(id, p);
                if let Some(size) = size {
                    saved.sizes.insert(id, size);
                }
            }
        }
        saved
    }

    /// Keeps `placements` (made for the stream `sizes`, around `center`, on the circle
    /// of `radius`) as the desk arrangement, unless they are the cinema view. Older
    /// readers skip the `radius` line.
    pub fn save(
        &self,
        placements: &HashMap<u8, Placement>,
        sizes: &HashMap<u8, PixelSize>,
        (center, radius): ((Vec3, f32), f32),
        cinema: bool,
    ) {
        if cinema {
            return;
        }
        let (eye, yaw) = center;
        let mut text = format!(
            "center {} {} {} {yaw}\nradius {radius}\n",
            eye.x, eye.y, eye.z
        );
        let mut ids: Vec<_> = placements.keys().copied().collect();
        ids.sort();
        for id in ids {
            text += &placement_line(id, &placements[&id], sizes.get(&id).copied());
        }
        write(&self.path, &text);
    }
}

pub fn load(path: &Path) -> HashMap<u8, Placement> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(parse_placement)
        .map(|(id, p, _)| (id, p))
        .collect()
}

pub fn save(path: &Path, placements: &HashMap<u8, Placement>) {
    let mut ids: Vec<_> = placements.keys().copied().collect();
    ids.sort();
    let text: String = ids
        .iter()
        .map(|id| placement_line(*id, &placements[id], None))
        .collect();
    write(path, &text);
}

fn write(path: &Path, text: &str) {
    if let Err(e) = prostir_proto::secure::write_atomic(path, text.as_bytes()) {
        log::warn!("cannot save layout to {}: {e}", path.display());
    }
}

/// `id x y z qx qy qz qw width`, then the stream size it was placed for (`2560x1440`)
/// when known. Older readers skip the size: it is not a number.
fn placement_line(id: u8, p: &Placement, size: Option<PixelSize>) -> String {
    let (pos, q) = (p.position, p.rotation);
    let size = size.map_or(String::new(), |(w, h)| format!(" {w}x{h}"));
    format!(
        "{id} {} {} {} {} {} {} {} {}{size}\n",
        pos.x, pos.y, pos.z, q.x, q.y, q.z, q.w, p.width
    )
}

fn parse_placement(line: &str) -> Option<(u8, Placement, Option<PixelSize>)> {
    let mut words = line.split_whitespace();
    let id: u8 = words.next()?.parse().ok()?;
    let v: Vec<f32> = words
        .by_ref()
        .take(8)
        .map(|w| w.parse().ok())
        .collect::<Option<_>>()?;
    if v.len() != 8 {
        return None;
    }
    let size = match words.next() {
        None => None,
        Some(w) => {
            let (w, h) = w.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        }
    };
    if words.next().is_some() {
        return None;
    }
    let p = Placement {
        position: Vec3::new(v[0], v[1], v[2]),
        rotation: Quat::from_xyzw(v[3], v[4], v[5], v[6]).normalize(),
        width: v[7],
    };
    (p.position.is_finite() && p.rotation.is_finite() && p.width.is_finite())
        .then_some((id, p, size))
}

/// `center x y z yaw`: where the arrangement was laid out around.
fn parse_center(line: &str) -> Option<(Vec3, f32)> {
    let rest = line.strip_prefix("center ")?;
    let v: Vec<f32> = rest
        .split_whitespace()
        .map(|w| w.parse().ok())
        .collect::<Option<_>>()?;
    let [x, y, z, yaw] = v[..] else {
        return None;
    };
    let eye = Vec3::new(x, y, z);
    (eye.is_finite() && yaw.is_finite()).then_some((eye, yaw))
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
pub fn facing_yaw(placements: &HashMap<u8, Placement>, eye: Vec3) -> Option<f32> {
    let (sin, cos) = placements
        .values()
        .map(|p| {
            let d = p.position - eye;
            (-d.x).atan2(-d.z)
        })
        .fold((0.0f32, 0.0f32), |(s, c), a| (s + a.sin(), c + a.cos()));
    (sin.abs() > 1e-6 || cos.abs() > 1e-6).then(|| sin.atan2(cos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_has_no_gaps_and_faces_user() {
        let l = arrange(
            Preset::Arc,
            &[(0, 2560, 1440), (1, 2560, 1440), (2, 1920, 1080)],
            Some(1),
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
        let yaw = yaw_of(p);
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
            let mons: Vec<(u8, u32, u32)> = (0..n)
                .map(|i| {
                    let (w, h) = sizes[(seed >> (i * 3 % 30 + 2)) as usize % sizes.len()];
                    (i as u8, w, h)
                })
                .collect();
            let primary = Some(((seed >> 7) as usize % n) as u8);
            let l = arrange(Preset::Arc, &mons, primary, None);
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
            let radius = preset_radius(Preset::Arc, &mons);
            let rows = split(
                Preset::Arc,
                &mons
                    .iter()
                    .map(|m| m.1 as f32 * METRES_PER_PIXEL)
                    .collect::<Vec<_>>(),
                None,
            );
            assert_eq!(radius, rows.radius);
        }
    }

    #[test]
    fn mirror_goes_under_primary() {
        let l = arrange(
            Preset::Arc,
            &[(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)],
            Some(1),
            Some((100, 2880, 1800)),
        );
        let (m, p) = (l[&100], l[&1]);
        assert!(m.position.y < p.position.y - 0.5);
        assert!((m.position.x - p.position.x).abs() < 1e-3);
        assert!(m.width <= MIRROR_MAX_WIDTH + 1e-6);
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
            let l = arrange(preset, &mons, Some(0), Some((100, 2880, 1800)));
            assert_eq!(l.len(), 5, "{preset:?}");
            assert!(
                l.values()
                    .all(|p| p.position.is_finite() && p.rotation.is_finite())
            );
        }
    }

    #[test]
    fn strong_curve_on_arc_is_centred_on_the_head() {
        let l = arrange(Preset::Arc, &[(0, 2560, 1440), (1, 2560, 1440)], None, None);
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
        );
        assert!(l[&0].position.x < l[&2].position.x && l[&2].position.x < l[&1].position.x);
        assert!(l[&2].position.x.abs() < 1e-3, "portrait is straight ahead");
        assert_eq!(order_by_direction(&l, &[1, 0, 2]), vec![0, 2, 1]);
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
        let out = cinema(&monitors, Some(1));
        assert_eq!(
            out.keys().copied().collect::<Vec<_>>(),
            vec![2],
            "picked by position"
        );
        assert!(out[&2].position.z < -2.0, "further away than the desk");
        // Nothing picked (or a stale pick): the largest landscape monitor, not a portrait one.
        assert_eq!(cinema_choice(&monitors, None), Some(1));
        assert_eq!(cinema_choice(&monitors, Some(9)), Some(1));
        assert_eq!(cinema_choice(&[(4, 1440, 2560)], None), Some(4));
        assert!(cinema(&[], None).is_empty());
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
            );
            let above = out[&primary];
            let h_above =
                monitors.iter().find(|m| m.0 == primary).unwrap().2 as f32 * METRES_PER_PIXEL;
            let mirror = out[&100];
            let h = 1800.0 * METRES_PER_PIXEL * (MIRROR_MAX_WIDTH / (2880.0 * METRES_PER_PIXEL));
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
        let out = arrange_curved(
            Preset::Arc,
            &monitors,
            Some(1),
            Some((100, 2880, 1800)),
            Some(radius),
        );
        let (above, mirror) = (out[&1], out[&100]);
        let a = Cylinder::new(&above, radius);
        let m = Cylinder::new(&mirror, radius);
        // Same axis (horizontally) and same radius: the edges meet along the whole width.
        assert!(Vec2::new(a.center.x - m.center.x, a.center.z - m.center.z).length() < 1e-3);
        let h_above = 1440.0 * METRES_PER_PIXEL;
        let h = 1800.0 * METRES_PER_PIXEL * (MIRROR_MAX_WIDTH / (2880.0 * METRES_PER_PIXEL));
        assert!(((above.position.y - h_above / 2.0) - (mirror.position.y + h / 2.0)).abs() < 1e-3);
    }

    #[test]
    fn rows_follow_what_the_user_sees() {
        let center = (Vec3::ZERO, 0.0);
        let rows_ids = |p: &HashMap<u8, Placement>, m: &[(u8, u32, u32)]| {
            layout_rows(&seen(p, m, center, Preset::Arc), None, &[])
                .iter()
                .map(|r| r.ids().collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        // An arc: one row, left to right.
        let three = [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)];
        let arc = arrange(Preset::Arc, &three, Some(1), None);
        assert_eq!(
            rows_ids(&arc, &three),
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
        let rows = layout_rows(&seen(&lifted, &three, center, Preset::Arc), Some(1), &[]);
        assert_eq!(rows_ids(&lifted, &three), vec![vec![2], vec![0, 1]]);
        // It is seen right over the middle monitor, not over the right edge.
        let seen_at = |row: usize, i: usize| rows[row].entries[i].center.unwrap();
        let middle = seen_at(1, 1);
        assert!((seen_at(0, 0) - middle).abs() < 150, "{rows:?}");
        assert!(seen_at(1, 0) < middle - 1500, "{rows:?}");

        // A wall: one row, left to right.
        let wall = arrange(Preset::Wall, &three, None, None);
        assert_eq!(rows_ids(&wall, &three), vec![vec![0, 1, 2]]);
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
        let arc = arrange(Preset::Arc, &two, None, None);
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
        let mut after = p.clone();
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
        let mut p = arrange(Preset::Arc, &[(1, 2560, 1440), (2, 2560, 1440)], None, None);
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
        let mut p = arrange(Preset::Wall, &two, None, None);
        let w = p[&0].width;
        p.get_mut(&1).unwrap().position = p[&0].position + Vec3::new(w + 0.1, 0.02, 0.05);
        let s = stick_on_wall(&p, &two, (Vec3::ZERO, 0.0), 1).expect("sticks");
        assert!((s.placement.position.z - p[&0].position.z).abs() < 1e-5);
        assert!((s.placement.position.x - (p[&0].position.x + w)).abs() < 1e-4);
        assert!((s.placement.position.y - p[&0].position.y).abs() < 1e-4);
    }

    #[test]
    fn lying_down_the_monitors_are_one_row() {
        let center = (Vec3::ZERO, 0.0);
        let three = [(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)];
        let overhead = arrange(Preset::Overhead, &three, None, None);
        let rows = layout_rows(
            &seen(&overhead, &three, center, Preset::Overhead),
            None,
            &[],
        );
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].ids().collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    /// A wall: A brings everything into one plane, facing one way, rows touching.
    #[test]
    fn tidy_wall_is_flat() {
        let center = (Vec3::ZERO, 0.0);
        let two = [(0, 2560, 1440), (1, 2560, 1440)];
        let mut rough = arrange(Preset::Wall, &two, None, None);
        rough.get_mut(&1).unwrap().position += Vec3::new(0.2, 0.05, 0.3);
        rough.get_mut(&1).unwrap().rotation = Quat::from_rotation_y(0.4);
        let t = tidy_wall(&rough, &two, center);
        assert!((t[&0].position.z - t[&1].position.z).abs() < 1e-5);
        assert!(t[&0].rotation.angle_between(t[&1].rotation) < 1e-5);
        let gap = (t[&1].position.x - t[&1].width / 2.0) - (t[&0].position.x + t[&0].width / 2.0);
        assert!(gap.abs() < 1e-4, "gap {gap}");
    }

    fn temp_desk(name: &str) -> (PathBuf, Desk) {
        let dir = std::env::temp_dir().join(format!("prostir-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let desk = Desk::new(dir.join("layout.txt"));
        (dir, desk)
    }

    /// The cinema view never replaces the desk arrangement: leaving cinema finds the
    /// monitors where the user put them.
    #[test]
    fn cinema_view_keeps_the_desk_arrangement() {
        let (dir, desk) = temp_desk("desk");
        let two = [(0, 2560, 1440), (1, 1920, 1080)];
        let sizes = HashMap::from([(0, (2560, 1440)), (1, (1920, 1080))]);
        let center = (Vec3::ZERO, 0.0);
        let mut arranged = arrange(Preset::Arc, &two, Some(0), None);
        arranged.get_mut(&1).unwrap().position += Vec3::new(0.1, 0.3, 0.0);
        desk.save(&arranged, &sizes, (center, RADIUS), false);
        desk.save(&cinema(&two, None), &sizes, (center, RADIUS), true);
        let kept = desk.load().placements;
        assert_eq!(kept.len(), 2);
        for (id, p) in &arranged {
            assert!(kept[id].position.distance(p.position) < 1e-5);
        }
        // Window panels are placed afresh, not kept.
        let mut with_panel = arranged.clone();
        with_panel.insert(prostir_proto::WINDOW_IDS.start, arranged[&0]);
        desk.save(&with_panel, &sizes, (center, RADIUS), false);
        assert_eq!(desk.load().placements.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stream sizes and the way the arrangement faces come back as saved.
    #[test]
    fn desk_keeps_sizes_and_center() {
        let (dir, desk) = temp_desk("desk-sizes");
        let two = [(0, 2560, 1440), (1, 1440, 2560)];
        let arranged = arrange(Preset::Arc, &two, Some(0), None);
        let sizes = HashMap::from([(0, (2560, 1440)), (1, (1440, 2560))]);
        let center = (Vec3::new(0.1, 0.0, -0.2), 0.7);
        desk.save(&arranged, &sizes, (center, RADIUS), false);
        let saved = desk.load();
        assert_eq!(saved.sizes, sizes);
        assert_eq!(saved.radius, Some(RADIUS));
        let (eye, yaw) = saved.center.unwrap();
        assert!(eye.distance(center.0) < 1e-6 && (yaw - center.1).abs() < 1e-6);
        assert_eq!(saved.placements.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file written by 0.1.0 (no sizes, no center) still reads; and a 0.1.0 reader,
    /// which keeps lines of exactly nine numbers, still reads ours.
    #[test]
    fn desk_reads_and_is_read_by_0_1_0() {
        let (dir, desk) = temp_desk("desk-old");
        std::fs::write(
            dir.join("layout.txt"),
            "0 -0.5 -0.08 -0.9 0 0.2 0 0.98 1.38\n1 0.5 -0.08 -0.9 0 -0.2 0 0.98 1.03\n",
        )
        .unwrap();
        let saved = desk.load();
        assert_eq!(saved.placements.len(), 2);
        assert!(saved.sizes.is_empty() && saved.center.is_none());
        assert!((saved.placements[&1].width - 1.03).abs() < 1e-6);

        let sizes = HashMap::from([(0, (2560, 1440))]);
        desk.save(&saved.placements, &sizes, ((Vec3::ZERO, 0.3), 1.3), false);
        let text = std::fs::read_to_string(dir.join("layout.txt")).unwrap();
        let old_reader = |line: &str| {
            line.split_whitespace()
                .filter_map(|w| w.parse::<f32>().ok())
                .count()
                == 9
        };
        assert_eq!(text.lines().filter(|l| old_reader(l)).count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A monitor that comes back with another resolution keeps the size of a pixel.
    #[test]
    fn refit_keeps_the_size_of_a_pixel() {
        let landscape = arrange(Preset::Arc, &[(0, 2560, 1440)], Some(0), None)[&0];
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
        );
        let yaw = 0.8f32;
        let turn = Quat::from_rotation_y(yaw);
        let turned: HashMap<u8, Placement> = row
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
}
