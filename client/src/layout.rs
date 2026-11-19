//! Where monitors float in the room, how controllers move them, and persistence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use glam::{Quat, Vec2, Vec3};
use prostir_proto::{LayoutEntry, LayoutRow, is_window_panel};

use crate::settings::Preset;

/// Monitor size in metres per stream pixel: every monitor gets the same pixel
/// density, so neighbours line up like one big screen (2560 px ≈ 1.25 m).
pub const METRES_PER_PIXEL: f32 = 1.25 / 2560.0;
/// Distance from the head to the monitors' edges.
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

/// An upright monitor on the cylinder around the head, its edges on the cylinder,
/// centred at `yaw` (0 = straight ahead, positive = right) and height `y`.
fn on_cylinder(yaw: f32, y: f32, width: f32) -> Placement {
    let half = (width / 2.0 / RADIUS).min(0.95).asin();
    let r = RADIUS * half.cos();
    let position = Vec3::new(r * yaw.sin(), y, -r * yaw.cos());
    Placement {
        position,
        rotation: facing(position, Vec3::new(0.0, y, 0.0)),
        width,
    }
}

fn angular_width(width: f32) -> f32 {
    2.0 * (width / 2.0 / RADIUS).min(0.95).asin()
}

/// `(id, width_m, height_m)` side by side without gaps, centred straight ahead.
fn row(items: &[(u8, f32, f32)], y: f32, out: &mut HashMap<u8, Placement>) {
    let total: f32 = items.iter().map(|m| angular_width(m.1)).sum();
    let mut yaw = -total / 2.0;
    for &(id, w, _) in items {
        let a = angular_width(w);
        out.insert(id, on_cylinder(yaw + a / 2.0, y, w));
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

/// Rows of monitors as the user sees them, for the desktop's arrangement: monitors
/// whose centres are at about the same height form a row; rows go top to bottom,
/// monitors left to right. With each monitor goes where its centre is seen, in pixels
/// of one scale for all of them (from the angles they take up), so the PC can put a
/// monitor hung above another above that one. `center` is where the layout faces from
/// (position, yaw).
pub fn rows_by_position(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    center: (Vec3, f32),
    preset: Preset,
) -> Vec<LayoutRow> {
    struct Seen {
        id: u8,
        yaw: f32,
        elevation: f32,
        half_width: f32,
        half_height: f32,
        width_px: u32,
    }
    // Lying down, the monitors are a band tilted up overhead: looked at along the tilt,
    // so a row there stays a row (not a "V" the mouse cannot cross sideways).
    let tilt = if preset == Preset::Overhead {
        OVERHEAD_PITCH
    } else {
        0.0
    };
    let unturn = Quat::from_rotation_x(-tilt) * Quat::from_rotation_y(-center.1);
    let mut seen: Vec<Seen> = monitors
        .iter()
        .filter_map(|&(id, w, h)| {
            let p = placements.get(&id)?;
            let local = unturn * (p.position - center.0);
            let flat = Vec2::new(local.x, local.z).length().max(0.1);
            let height = p.width * h as f32 / w.max(1) as f32;
            Some(Seen {
                id,
                yaw: local.x.atan2(-local.z),
                elevation: local.y.atan2(flat),
                half_width: (p.width / 2.0 / flat).atan(),
                half_height: (height / 2.0 / flat).atan(),
                width_px: w,
            })
        })
        .collect();
    if seen.is_empty() {
        return Vec::new();
    }
    // Pixels per radian, from the monitors themselves.
    let px_per_rad = seen
        .iter()
        .map(|m| m.width_px as f32 / (2.0 * m.half_width).max(1e-3))
        .sum::<f32>()
        / seen.len() as f32;
    seen.sort_by(|a, b| b.elevation.total_cmp(&a.elevation));
    let mut rows: Vec<Vec<Seen>> = Vec::new();
    for m in seen {
        let same_row = rows.iter_mut().find(|r| {
            let first = &r[0];
            (first.elevation - m.elevation).abs() < 0.75 * first.half_height.max(m.half_height)
        });
        match same_row {
            Some(r) => r.push(m),
            None => rows.push(vec![m]),
        }
    }
    // Where on the desktop, from how the monitors stand in space (the angles above
    // only group them into rows). Starting from the one straight ahead.
    let ahead = rows
        .iter()
        .flatten()
        .min_by(|a, b| {
            (a.yaw.abs() + a.elevation.abs()).total_cmp(&(b.yaw.abs() + b.elevation.abs()))
        })
        .map(|m| m.id);
    let rects = desktop_rects(placements, monitors, ahead);
    // Older PCs place by angles when a monitor has no spot here (none is left out).
    let fallback = |m: &Seen| {
        (
            ((m.yaw * px_per_rad) / 8.0).round() as i32 * 8,
            ((-m.elevation * px_per_rad) / 8.0).round() as i32 * 8,
        )
    };
    rows.into_iter()
        .map(|mut r| {
            r.sort_by(|a, b| a.yaw.total_cmp(&b.yaw));
            LayoutRow {
                entries: r
                    .iter()
                    .map(|m| {
                        let (center, middle) =
                            rects.get(&m.id).copied().unwrap_or_else(|| fallback(m));
                        LayoutEntry {
                            id: m.id,
                            center: Some(center),
                            middle: Some(middle),
                        }
                    })
                    .collect(),
            }
        })
        .collect()
}

/// The monitors for the map in the menu: as they are put on the desktop (see
/// [`desktop_rects`]), centre and half size in pixels, up positive. The same
/// wherever the user's head is, at the monitors' own sizes.
pub fn desktop_spots(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    first: Option<u8>,
) -> HashMap<u8, [f32; 4]> {
    desktop_rects(placements, monitors, first)
        .into_iter()
        .filter_map(|(id, (x, y))| {
            let &(_, w, h) = monitors.iter().find(|m| m.0 == id)?;
            Some((id, [x as f32, -y as f32, w as f32 / 2.0, h as f32 / 2.0]))
        })
        .collect()
}

/// Desktop centres (pixels) from where the monitors stand in space: starting from
/// `first`, each monitor joins the nearest one already placed, on the side where it
/// stands next to it, shifted along that edge by as much as it is shifted in space
/// (measured in the placed monitor's own plane and pixels). A window across an edge
/// the monitors share in the headset then continues where it does there, wherever
/// the user's head is. Rounded to 8 px, so a hand's tremor does not rearrange the
/// desktop.
fn desktop_rects(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    first: Option<u8>,
) -> HashMap<u8, (i32, i32)> {
    struct Screen {
        id: u8,
        at: Vec3,
        right: Vec3,
        up: Vec3,
        out: Vec3,
        half: Vec2,
        px: (i32, i32),
        metres_per_px: f32,
    }
    let mut rest: Vec<Screen> = monitors
        .iter()
        .filter_map(|&(id, w, h)| {
            let p = placements.get(&id)?;
            let (w, h) = (w.max(1), h.max(1));
            let height = p.width * h as f32 / w as f32;
            Some(Screen {
                id,
                at: p.position,
                right: p.rotation * Vec3::X,
                up: p.rotation * Vec3::Y,
                out: p.rotation * Vec3::Z,
                half: Vec2::new(p.width, height) / 2.0,
                px: (w as i32, h as i32),
                metres_per_px: p.width.max(1e-3) / w as f32,
            })
        })
        .collect();
    let mut out = HashMap::new();
    let Some(start) = rest
        .iter()
        .position(|s| Some(s.id) == first)
        .or_else(|| (!rest.is_empty()).then_some(0))
    else {
        return out;
    };
    let snap = |v: f32| (v / 8.0).round() as i32 * 8;
    let s0 = rest.remove(start);
    // Top-left corners.
    let mut placed: Vec<(Screen, i32, i32)> = vec![(s0, 0, 0)];
    // How `m` stands next to `n`, in `n`'s plane: offset of the centres and the gap
    // between their edges (negative when they overlap), across and up.
    let relative = |m: &Screen, n: &Screen| {
        let d = m.at - n.at;
        let (x, y) = (d.dot(n.right), d.dot(n.up));
        let reach = n.half + m.half;
        (
            x,
            y,
            Vec2::new(x.abs() - reach.x, y.abs() - reach.y),
            d.dot(n.out).abs(),
        )
    };
    while !rest.is_empty() {
        let (ri, pi) = (0..rest.len())
            .flat_map(|ri| (0..placed.len()).map(move |pi| (ri, pi)))
            .min_by(|&(a, b), &(c, d)| {
                let key = |ri: usize, pi: usize| {
                    let (_, _, gap, depth) = relative(&rest[ri], &placed[pi].0);
                    gap.x.max(gap.y).max(0.0) + 0.5 * depth
                };
                key(a, b).total_cmp(&key(c, d))
            })
            .expect("not empty");
        let m = rest.remove(ri);
        let (n, nx, ny) = &placed[pi];
        let (x, y, gap, _) = relative(&m, n);
        let to_px = |metres: f32| snap(metres / n.metres_per_px);
        let (mx, my) = if gap.x > gap.y {
            // Beside it: level with where its top edge is in `n`'s plane.
            let top = (m.at + m.up * m.half.y - n.at).dot(n.up);
            let x = if x > 0.0 { nx + n.px.0 } else { nx - m.px.0 };
            (x, ny + to_px(n.half.y - top))
        } else {
            // Above or below it: as far across as its left edge is.
            let left = (m.at - m.right * m.half.x - n.at).dot(n.right);
            let y = if y > 0.0 { ny - m.px.1 } else { ny + n.px.1 };
            (nx + to_px(left + n.half.x), y)
        };
        placed.push((m, mx, my));
    }
    for (s, x, y) in &placed {
        out.insert(s.id, (x + s.px.0 / 2, y + s.px.1 / 2));
    }
    out
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

/// The A button: the user's own arrangement with the seams taken out. Starting from
/// `anchor` (the primary monitor, which stays), each monitor within a monitor's width
/// of one already done is joined to it edge to edge (see [`hinge`]), where along that
/// edge the user put it, at the size the user gave it. Monitors put clearly apart stay
/// as they are. `monitors` may include the laptop screen; `eye` is where the user sits.
pub fn snap(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    eye: Vec3,
    anchor: Option<u8>,
) -> HashMap<u8, Placement> {
    let mut rest = parts(placements, monitors);
    if rest.is_empty() {
        return HashMap::new();
    }
    // Upright and facing the user first: a monitor turned askew (held in a hand) would
    // otherwise be joined by whichever corner it happens to reach out with.
    for m in &mut rest {
        m.1.rotation = facing(m.1.position, eye);
    }
    // The monitors first, the laptop screen after: it joins them, not they it.
    let laptop = rest
        .iter()
        .position(|m| m.0 == prostir_proto::MIRROR_ID)
        .filter(|_| rest.len() > 1)
        .map(|i| rest.remove(i));
    let first = rest.iter().position(|m| Some(m.0) == anchor).unwrap_or(0);
    let mut placed = vec![rest.remove(first)];
    // Outwards from the anchor: always the monitor closest to one already placed.
    loop {
        if rest.is_empty() {
            match laptop {
                Some(l) if !placed.iter().any(|m| m.0 == l.0) => rest.push(l),
                _ => break,
            }
        }
        let (ri, joined) = rest
            .iter()
            .enumerate()
            .map(|(ri, m)| (ri, nearest(m, &placed, eye)))
            .min_by(|a, b| {
                let gap = |j: &Option<(Part, Beside)>| j.map_or(f32::MAX, |(_, b)| b.gap);
                gap(&a.1).total_cmp(&gap(&b.1))
            })
            .expect("not empty");
        let mut m = rest.remove(ri);
        if let Some((n, b)) = joined
            && b.gap < m.1.width.min(n.1.width)
        {
            // Sizes stay as the user made them: only the seams go.
            let (mw, mh) = (m.1.width, m.2);
            let b = Beside {
                along: settle_along(&b, &n, mw, mh),
                ..b
            };
            m.1 = hinge(&n.1, n.2, mw, mh, &b, eye);
        }
        placed.push(m);
    }
    placed.into_iter().map(|m| (m.0, m.1)).collect()
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
    let mut items: Vec<(u8, Vec3, f32, f32)> = monitors
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
    items.sort_by(|a, b| b.1.y.total_cmp(&a.1.y));
    let mut rows: Vec<Vec<(u8, Vec3, f32, f32)>> = Vec::new();
    for it in items {
        match rows
            .iter_mut()
            .find(|r| (r[0].1.y - it.1.y).abs() < 0.5 * r[0].3.max(it.3))
        {
            Some(r) => r.push(it),
            None => rows.push(vec![it]),
        }
    }
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
    match preset {
        Preset::Arc | Preset::Overhead => {
            let y = if preset == Preset::Arc {
                -EYE_DROP
            } else {
                0.0
            };
            row(&sorted, y, &mut out);
        }
        Preset::Wall => {
            // Side by side in one plane straight ahead, all facing the same way.
            let total: f32 = sorted.iter().map(|m| m.1).sum();
            let mut x = -total / 2.0;
            for &(id, w, _) in &sorted {
                out.insert(
                    id,
                    Placement {
                        position: Vec3::new(x + w / 2.0, -EYE_DROP, -RADIUS),
                        rotation: Quat::IDENTITY,
                        width: w,
                    },
                );
                x += w;
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
        let reach = angular_width(w) / 2.0;
        let bottom = sorted
            .iter()
            .filter_map(|m| {
                let p = out.get(&m.0)?;
                let overlaps = (yaw_of(p) - yaw).abs() < reach + angular_width(m.1) / 2.0;
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
            None => on_cylinder(yaw, bottom, w).position,
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

impl Desk {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The saved arrangement. Window panels are placed afresh when they appear.
    pub fn load(&self) -> HashMap<u8, Placement> {
        load(&self.path)
            .into_iter()
            .filter(|(id, _)| !is_window_panel(*id))
            .collect()
    }

    /// Keeps `placements` as the desk arrangement, unless they are the cinema view.
    pub fn save(&self, placements: &HashMap<u8, Placement>, cinema: bool) {
        if !cinema {
            save(&self.path, placements);
        }
    }
}

pub fn load(path: &Path) -> HashMap<u8, Placement> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let v: Vec<f32> = line
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if v.len() != 9 {
                return None;
            }
            let rotation = Quat::from_xyzw(v[4], v[5], v[6], v[7]).normalize();
            Some((
                v[0] as u8,
                Placement {
                    position: Vec3::new(v[1], v[2], v[3]),
                    rotation,
                    width: v[8],
                },
            ))
        })
        .filter(|(_, p)| p.position.is_finite() && p.rotation.is_finite() && p.width.is_finite())
        .collect()
}

pub fn save(path: &Path, placements: &HashMap<u8, Placement>) {
    let mut ids: Vec<_> = placements.keys().copied().collect();
    ids.sort();
    let text: String = ids
        .iter()
        .map(|id| {
            let p = &placements[id];
            let (pos, q) = (p.position, p.rotation);
            format!(
                "{id} {} {} {} {} {} {} {} {}\n",
                pos.x, pos.y, pos.z, q.x, q.y, q.z, q.w, p.width
            )
        })
        .collect();
    if let Err(e) = prostir_proto::secure::write_atomic(path, text.as_bytes()) {
        log::warn!("cannot save layout to {}: {e}", path.display());
    }
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
        // An arc: one row, left to right.
        let three = [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)];
        let arc = arrange(Preset::Arc, &three, Some(1), None);
        let rows = rows_by_position(&arc, &three, center, Preset::Arc);
        assert_eq!(
            rows.len(),
            1,
            "a portrait monitor in the arc is in the same row"
        );
        assert_eq!(rows[0].ids().collect::<Vec<_>>(), vec![0, 1, 2]);

        // Lift the right monitor above the middle one: two rows, it is on top.
        let mut lifted = arc.clone();
        let mid = arc[&1];
        let mut top = arc[&2];
        top.position = Vec3::new(mid.position.x, mid.position.y + 1.0, mid.position.z);
        top.rotation = facing(top.position, Vec3::ZERO);
        lifted.insert(2, top);
        let rows = rows_by_position(&lifted, &three, center, Preset::Arc);
        assert_eq!(
            rows.iter()
                .map(|r| r.ids().collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            vec![vec![2], vec![0, 1]]
        );
        // It is seen right over the middle monitor, not over the right edge.
        let seen_at = |row: usize, i: usize| rows[row].entries[i].center.unwrap();
        let middle = seen_at(1, 1);
        assert!((seen_at(0, 0) - middle).abs() < 100, "{rows:?}");
        assert!(seen_at(1, 0) < middle - 1500, "{rows:?}");

        // A wall: one row, left to right.
        let wall = arrange(Preset::Wall, &three, None, None);
        let rows = rows_by_position(&wall, &three, center, Preset::Arc);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ids().collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn the_desktop_follows_the_edges_in_space_not_the_head() {
        let three = [(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)];
        let mut arc = arrange(Preset::Arc, &three, Some(1), None);
        // The right monitor hung 20 cm lower, still against the middle one.
        arc.get_mut(&2).unwrap().position.y -= 0.2;
        let from = |eye: Vec3| rows_by_position(&arc, &three, (eye, 0.0), Preset::Arc);
        let spot = |rows: &[LayoutRow], id: u8| {
            rows.iter()
                .find_map(|r| {
                    let e = r.entries.iter().find(|e| e.id == id)?;
                    Some((e.center.unwrap(), e.middle.unwrap()))
                })
                .unwrap()
        };
        let here = from(Vec3::ZERO);
        let stood_up = from(Vec3::new(0.3, 0.4, 0.2));
        for id in [0, 2] {
            let (a, b) = (spot(&here, id), spot(&stood_up, id));
            let (ma, mb) = (spot(&here, 1), spot(&stood_up, 1));
            assert_eq!(
                (a.0 - ma.0, a.1 - ma.1),
                (b.0 - mb.0, b.1 - mb.1),
                "monitor {id}"
            );
        }
        // 20 cm lower is 20 cm worth of pixels lower on the desktop, edge to edge.
        let (right, mid) = (spot(&here, 2), spot(&here, 1));
        let expected = (0.2 / METRES_PER_PIXEL / 8.0).round() as i32 * 8;
        assert!(((right.1 - mid.1) - expected).abs() <= 8, "{here:?}");
        assert_eq!(right.0 - mid.0, (1440 + 2560) / 2, "{here:?}");
    }

    #[test]
    fn line_up_straightens_and_joins_monitors_before_the_laptop_screen() {
        let mirror = prostir_proto::MIRROR_ID;
        let sizes = [(1, 1440, 2560), (2, 2560, 1440), (mirror, 1920, 1080)];
        let eye = Vec3::ZERO;
        let mut p = arrange(
            Preset::Arc,
            &sizes[..2],
            Some(1),
            Some((mirror, 1920, 1080)),
        );
        // The right monitor turned askew and dropped low, its corner by the laptop
        // screen's edge.
        let r = p.get_mut(&2).unwrap();
        r.position.y -= 0.55;
        r.rotation = Quat::from_rotation_z(0.6) * r.rotation;
        let done = snap(&p, &sizes, eye, Some(1));
        let (right, primary) = (done[&2], done[&1]);
        // Upright again, beside the primary (its left edge against the primary's right
        // edge), not under the laptop screen.
        let up = right.rotation * Vec3::Y;
        assert!(up.y > 0.99, "{up:?}");
        // Its left edge lies on the primary's right edge (in the primary's plane).
        let right_left = right.position - right.rotation * Vec3::X * right.width / 2.0;
        let local = primary.rotation.inverse() * (right_left - primary.position);
        assert!((local.x - primary.width / 2.0).abs() < 0.01, "{local:?}");
        assert!(local.z.abs() < 0.01, "{local:?}");
    }

    /// Placements and the monitors' pixel sizes, for the A and magnet tests.
    type Case = (HashMap<u8, Placement>, Vec<(u8, u32, u32)>);

    fn case(positions: &[(u8, Vec3, f32)]) -> Case {
        let sizes: Vec<(u8, u32, u32)> = positions.iter().map(|p| (p.0, 2560, 1440)).collect();
        let placements = positions
            .iter()
            .map(|&(id, at, width)| {
                (
                    id,
                    Placement {
                        position: at,
                        rotation: facing(at, Vec3::ZERO),
                        width,
                    },
                )
            })
            .collect();
        (placements, sizes)
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
    fn a_joins_monitors_edge_to_edge() {
        let (p, sizes) = case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            // Overlapping on the right, a little lower and nearer.
            (1, Vec3::new(1.0, -0.05, -0.8), 1.2),
            // A small gap on the left.
            (2, Vec3::new(-1.35, 0.02, -0.9), 1.2),
        ]);
        let t = snap(&p, &sizes, Vec3::ZERO, Some(0));
        assert!(
            t[&0].position.distance(p[&0].position) < 1e-5,
            "the anchor stays"
        );
        for id in [1, 2] {
            let s = seam(&t[&0], &t[&id]);
            assert!(s < 1e-3, "monitor {id}: corners {s} m off the shared edge");
        }
    }

    #[test]
    fn a_monitor_above_tilts_towards_the_user_where_it_was_put() {
        // Above the anchor, off to the right on purpose, sunk into it.
        let (p, sizes) = case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            (1, Vec3::new(0.35, 0.55, -1.0), 1.2),
        ]);
        let t = snap(&p, &sizes, Vec3::ZERO, Some(0));
        assert!(seam(&t[&0], &t[&1]) < 1e-3);
        // Still off to the right (not centred into a row).
        let x = (t[&0].rotation.inverse() * (t[&1].position - t[&0].position)).x;
        assert!((x - 0.35).abs() < 0.08, "moved along the edge to {x}");
        // Tilted down towards the eye, not a vertical wall.
        let normal = t[&1].rotation * Vec3::Z;
        assert!(normal.y < -0.1, "normal {normal}");
    }

    #[test]
    fn a_leaves_monitors_put_apart() {
        let (p, sizes) = case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            (1, Vec3::new(2.5, 0.0, 0.5), 1.2),
        ]);
        let t = snap(&p, &sizes, Vec3::ZERO, Some(0));
        assert!(t[&1].position.distance(p[&1].position) < 1e-5);
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
        let rows = rows_by_position(&overhead, &three, center, Preset::Overhead);
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

    /// The cinema view never replaces the desk arrangement: leaving cinema finds the
    /// monitors where the user put them.
    #[test]
    fn cinema_view_keeps_the_desk_arrangement() {
        let dir = std::env::temp_dir().join(format!("prostir-desk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let desk = Desk::new(dir.join("layout.txt"));
        let two = [(0, 2560, 1440), (1, 1920, 1080)];
        let mut arranged = arrange(Preset::Arc, &two, Some(0), None);
        arranged.get_mut(&1).unwrap().position += Vec3::new(0.1, 0.3, 0.0);
        desk.save(&arranged, false);
        desk.save(&cinema(&two, None), true);
        let kept = desk.load();
        assert_eq!(kept.len(), 2);
        for (id, p) in &arranged {
            assert!(kept[id].position.distance(p.position) < 1e-5);
        }
        // Window panels are placed afresh, not kept.
        let mut with_panel = arranged.clone();
        with_panel.insert(prostir_proto::WINDOW_IDS.start, arranged[&0]);
        desk.save(&with_panel, false);
        assert_eq!(desk.load().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
