//! Where monitors float in the room, how controllers move them, and persistence.

use std::collections::HashMap;
use std::path::Path;

use glam::{Quat, Vec2, Vec3};
use prostir_proto::LayoutRow;

use crate::settings::Preset;

/// Monitor size in metres per stream pixel: every monitor gets the same pixel
/// density, so neighbours line up like one big screen (2560 px ≈ 1.25 m).
pub const METRES_PER_PIXEL: f32 = 1.25 / 2560.0;
/// Distance from the head to the monitors' edges.
pub const RADIUS: f32 = 1.1;
/// The main row sits a little below eye height.
const EYE_DROP: f32 = 0.08;
/// How far up the "overhead" layout is tilted, for lying down.
const OVERHEAD_PITCH: f32 = 0.95;
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
) -> Vec<LayoutRow> {
    struct Seen {
        id: u8,
        yaw: f32,
        elevation: f32,
        half_width: f32,
        half_height: f32,
        width_px: u32,
    }
    let unturn = Quat::from_rotation_y(-center.1);
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
    rows.into_iter()
        .map(|mut r| {
            r.sort_by(|a, b| a.yaw.total_cmp(&b.yaw));
            LayoutRow {
                ids: r.iter().map(|m| m.id).collect(),
                // Rounded to 8 px: a hand's tremor must not rearrange the desktop.
                centers: r
                    .iter()
                    .map(|m| ((m.yaw * px_per_rad) / 8.0).round() as i32 * 8)
                    .collect(),
            }
        })
        .collect()
}

/// A monitor as seen from the eye: its centre's direction (yaw to the right of the
/// anchor monitor, pitch up), its distance, and its size in metres.
#[derive(Debug, Clone, Copy)]
struct Seen {
    id: u8,
    yaw: f32,
    pitch: f32,
    distance: f32,
    width: f32,
    height: f32,
    /// Width in pixels.
    pixels: f32,
}

impl Seen {
    fn of(id: u8, p: &Placement, w: u32, h: u32, eye: Vec3) -> Self {
        let d = p.position - eye;
        Seen {
            id,
            yaw: d.x.atan2(-d.z),
            pitch: d.y.atan2(Vec2::new(d.x, d.z).length()),
            distance: d.length().max(0.3),
            width: p.width,
            height: p.width * h as f32 / w.max(1) as f32,
            pixels: w.max(1) as f32,
        }
    }

    /// The same size per pixel as `other`: text the same size on both.
    fn match_density(&mut self, other: &Seen) {
        let aspect = self.height / self.width;
        self.width = other.width / other.pixels * self.pixels;
        self.height = self.width * aspect;
    }

    /// Back to a placement facing the eye (`base`: the yaw that was taken off).
    fn placement(&self, base: f32, eye: Vec3) -> Placement {
        let yaw = self.yaw + base;
        let dir = Vec3::new(
            self.pitch.cos() * yaw.sin(),
            self.pitch.sin(),
            -self.pitch.cos() * yaw.cos(),
        );
        let position = eye + dir * self.distance;
        Placement {
            position,
            rotation: facing(position, eye),
            width: self.width,
        }
    }

    /// Half its width and height as angles.
    fn half(&self) -> (f32, f32) {
        (
            (self.width / 2.0 / self.distance).atan(),
            (self.height / 2.0 / self.distance).atan(),
        )
    }

    /// Angular gaps to `other` across and up (negative: the ranges overlap).
    fn gaps(&self, other: &Seen) -> (f32, f32) {
        let ((aw, ah), (bw, bh)) = (self.half(), other.half());
        (
            (self.yaw - other.yaw).abs() - (aw + bw),
            (self.pitch - other.pitch).abs() - (ah + bh),
        )
    }

    /// How far apart the two are: the larger gap, or nothing when they overlap.
    fn separation(&self, other: &Seen) -> f32 {
        let (gx, gy) = self.gaps(other);
        gx.max(gy).max(0.0)
    }

    fn overlaps(&self, other: &Seen) -> bool {
        let (gx, gy) = self.gaps(other);
        gx < -1e-3 && gy < -1e-3
    }
}

/// Lines `value` (half size `half`) up with a neighbour at `other` (half size
/// `other_half`): centres, first edges or last edges, whichever is nearest, when it
/// is a small move (under a third of its size); otherwise it stays.
fn align(value: f32, half: f32, other: f32, other_half: f32) -> f32 {
    let options = [other, other + other_half - half, other - other_half + half];
    let best = options
        .into_iter()
        .min_by(|a, b| (a - value).abs().total_cmp(&(b - value).abs()))
        .unwrap_or(value);
    if (best - value).abs() <= 2.0 * half / 3.0 {
        best
    } else {
        value
    }
}

/// The A button: the user's own arrangement, with the seams taken out. Seen from
/// `eye`, monitors that overlap or almost touch (a gap under half a monitor) are moved
/// to meet edge to edge, at the same distance as the neighbour they join and lined up
/// with it when nearly so; monitors put clearly apart stay where they are. Every
/// monitor turns to face the eye. `anchor` (the primary monitor) does not move.
pub fn snap(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    eye: Vec3,
    anchor: Option<u8>,
) -> HashMap<u8, Placement> {
    let mut rest: Vec<Seen> = monitors
        .iter()
        .filter_map(|&(id, w, h)| Some(Seen::of(id, placements.get(&id)?, w, h, eye)))
        .collect();
    if rest.is_empty() {
        return HashMap::new();
    }
    let first = rest.iter().position(|m| Some(m.id) == anchor).unwrap_or(0);
    let mut placed = vec![rest.remove(first)];
    // Yaw around the anchor, so nothing wraps around behind the user.
    let base = placed[0].yaw;
    let wrap = |a: f32| {
        (a + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
    };
    placed[0].yaw = 0.0;
    for m in &mut rest {
        m.yaw = wrap(m.yaw - base);
    }
    // Outwards from the anchor: always the monitor closest to one already placed.
    while !rest.is_empty() {
        let (ri, pi, sep) = rest
            .iter()
            .enumerate()
            .flat_map(|(ri, m)| {
                placed
                    .iter()
                    .enumerate()
                    .map(move |(pi, n)| (ri, pi, m.separation(n)))
            })
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .expect("both lists are non-empty");
        let mut m = rest.remove(ri);
        let n = placed[pi];
        let (mw, _) = m.half();
        let (nw, _) = n.half();
        if sep < mw.min(nw) {
            m.match_density(&n);
            attach(&mut m, &n, &placed);
        }
        placed.push(m);
    }
    placed
        .into_iter()
        .map(|m| (m.id, m.placement(base, eye)))
        .collect()
}

/// Moves `m` to meet `n` edge to edge (see [`snap`]), clear of everything `placed`.
/// Returns whether they are side by side (else one above the other) and on which side
/// of `n` it went (1: right or above, -1: left or below).
fn attach(m: &mut Seen, n: &Seen, placed: &[Seen]) -> (bool, f32) {
    let (gx, gy) = m.gaps(n);
    // Side by side when the gap across is the larger one, else above or below.
    let across = gx > gy;
    m.distance = n.distance;
    let ((mw, mh), (nw, nh)) = (m.half(), n.half());
    let side = |a: f32, b: f32| if a >= b { 1.0 } else { -1.0 };
    let dir = if across {
        side(m.yaw, n.yaw)
    } else {
        side(m.pitch, n.pitch)
    };
    if across {
        m.yaw = n.yaw + dir * (nw + mw);
        m.pitch = align(m.pitch, mh, n.pitch, nh);
    } else {
        m.pitch = n.pitch + dir * (nh + mh);
        m.yaw = align(m.yaw, mw, n.yaw, nw);
    }
    // Still on top of another one: past it, the same way.
    for _ in 0..placed.len() {
        let Some(o) = placed
            .iter()
            .find(|o| o.id != m.id && m.overlaps(o))
            .copied()
        else {
            break;
        };
        let ((mw, mh), (ow, oh)) = (m.half(), o.half());
        if across {
            m.yaw = o.yaw + dir * (ow + mw);
        } else {
            m.pitch = o.pitch + dir * (oh + mh);
        }
    }
    (across, dir)
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

/// The magnet for one monitor: if `id`, seen from `eye`, overlaps another monitor or
/// is within half a monitor of one, where it would stick to the nearest (see
/// [`snap`]); `None` when it is clearly apart.
pub fn stick(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    eye: Vec3,
    id: u8,
) -> Option<Stick> {
    let &(_, w, h) = monitors.iter().find(|m| m.0 == id)?;
    let mut m = Seen::of(id, placements.get(&id)?, w, h, eye);
    let base = m.yaw;
    m.yaw = 0.0;
    let wrap = |a: f32| {
        (a + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
    };
    let others: Vec<Seen> = monitors
        .iter()
        .filter(|o| o.0 != id)
        .filter_map(|&(oid, w, h)| {
            let mut o = Seen::of(oid, placements.get(&oid)?, w, h, eye);
            o.yaw = wrap(o.yaw - base);
            Some(o)
        })
        .collect();
    let n = *others
        .iter()
        .min_by(|a, b| m.separation(a).total_cmp(&m.separation(b)))?;
    if m.separation(&n) >= m.half().0.min(n.half().0) {
        return None;
    }
    let (across, side) = attach(&mut m, &n, &others);
    Some(Stick {
        placement: m.placement(base, eye),
        neighbour: n.id,
        across,
        side,
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
}

impl Grab {
    pub fn start(id: u8, distance: f32, local_hit: Vec2, placement: &Placement) -> Self {
        Self {
            id,
            distance,
            anchor: local_hit / placement.width,
            reach: None,
        }
    }

    /// Distance follows how far the hand reaches from the head (no thumbstick on hands).
    pub fn with_reach(mut self, hand: Vec3, head: Vec3) -> Self {
        self.reach = Some((self.distance, hand.distance(head).max(0.05)));
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
        let rows = rows_by_position(&arc, &three, center);
        assert_eq!(
            rows.len(),
            1,
            "a portrait monitor in the arc is in the same row"
        );
        assert_eq!(rows[0].ids, vec![0, 1, 2]);

        // Lift the right monitor above the middle one: two rows, it is on top.
        let mut lifted = arc.clone();
        let mid = arc[&1];
        let mut top = arc[&2];
        top.position = Vec3::new(mid.position.x, mid.position.y + 1.0, mid.position.z);
        top.rotation = facing(top.position, Vec3::ZERO);
        lifted.insert(2, top);
        let rows = rows_by_position(&lifted, &three, center);
        assert_eq!(
            rows.iter().map(|r| r.ids.clone()).collect::<Vec<_>>(),
            vec![vec![2], vec![0, 1]]
        );
        // It is seen right over the middle monitor, not over the right edge.
        let middle = rows[1].centers[1];
        assert!((rows[0].centers[0] - middle).abs() < 100, "{rows:?}");
        assert!(rows[1].centers[0] < middle - 1500, "{rows:?}");

        // A wall: one row, left to right.
        let wall = arrange(Preset::Wall, &three, None, None);
        let rows = rows_by_position(&wall, &three, center);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ids, vec![0, 1, 2]);
    }

    /// Monitors placed roughly by hand: A keeps two rows (one monitor above the middle)
    /// and makes each row tidy instead of throwing everything into one arc.
    #[test]
    fn tidy_keeps_the_arrangement() {
        let center = (Vec3::ZERO, 0.0);
        let three = [(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)];
        let mut rough = arrange(Preset::Arc, &[(0, 2560, 1440), (1, 2560, 1440)], None, None);
        // Pull them apart a little and tilt one: what hands leave behind.
        rough.get_mut(&0).unwrap().position += Vec3::new(-0.08, 0.03, 0.05);
        let mid = rough[&1].position;
        let up = mid + Vec3::new(0.05, 0.9, 0.02);
        rough.insert(
            2,
            Placement {
                position: up,
                rotation: facing(up, Vec3::ZERO),
                width: rough[&1].width,
            },
        );
        let tidy = snap(&rough, &three, center.0, Some(1));
        assert_eq!(tidy.len(), 3);
        // Still two rows: monitor 2 above the others.
        assert!(tidy[&2].position.y > tidy[&1].position.y + 0.5);
        // The bottom row: same height, and the two monitors touch (no gap).
        assert!((tidy[&0].position.y - tidy[&1].position.y).abs() < 1e-4);
        // Neighbours touch: the right edge of one is the left edge of the next.
        let edge =
            |p: &Placement, side: f32| p.position + p.rotation * Vec3::X * (side * p.width / 2.0);
        let gap = edge(&tidy[&0], 1.0).distance(edge(&tidy[&1], -1.0));
        assert!(gap < 0.01, "gap {gap} m");
    }

    fn snap_case(
        positions: &[(u8, Vec3, f32)],
    ) -> (
        HashMap<u8, Placement>,
        HashMap<u8, Placement>,
        Vec<(u8, u32, u32)>,
    ) {
        let sizes: Vec<(u8, u32, u32)> = positions.iter().map(|p| (p.0, 2560, 1440)).collect();
        let before: HashMap<u8, Placement> = positions
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
        let after = snap(&before, &sizes, Vec3::ZERO, Some(positions[0].0));
        (before, after, sizes)
    }

    fn seen_of(p: &Placement, w: f32) -> Seen {
        let d = p.position;
        Seen {
            id: 0,
            yaw: d.x.atan2(-d.z),
            pitch: d.y.atan2(Vec2::new(d.x, d.z).length()),
            distance: d.length(),
            width: w,
            height: w * 1440.0 / 2560.0,
            pixels: 2560.0,
        }
    }

    #[test]
    fn snap_closes_seams_and_overlaps() {
        // The anchor straight ahead; one overlapping it on the right, a little lower
        // and nearer; one leaving a small gap on the left.
        let (before, after, _) = snap_case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            (1, Vec3::new(1.0, -0.05, -0.8), 1.2),
            (2, Vec3::new(-1.35, 0.02, -0.9), 1.2),
        ]);
        // The anchor stays.
        assert!(after[&0].position.distance(before[&0].position) < 1e-4);
        let a = seen_of(&after[&0], 1.2);
        for id in [1, 2] {
            let m = seen_of(&after[&id], 1.2);
            let (gx, _) = m.gaps(&a);
            assert!(gx.abs() < 1e-3, "monitor {id}: gap {gx}");
            // Same distance and lined up.
            assert!((m.distance - a.distance).abs() < 1e-3);
            assert!((m.pitch - a.pitch).abs() < 1e-3);
        }
    }

    #[test]
    fn snap_makes_joined_monitors_the_same_density() {
        // Same pixels, the right one made bigger by hand.
        let (_, after, _) = snap_case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            (1, Vec3::new(1.3, 0.0, -0.9), 1.6),
        ]);
        assert!((after[&1].width - 1.2).abs() < 1e-4);
    }

    #[test]
    fn snap_leaves_monitors_put_apart() {
        // Far to the right, on purpose.
        let (before, after, _) = snap_case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            (1, Vec3::new(1.1, 0.0, 0.0), 1.2),
        ]);
        assert!(after[&1].position.distance(before[&1].position) < 1e-3);
    }

    #[test]
    fn a_monitor_let_go_nearby_sticks() {
        let (before, _, sizes) = snap_case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            (1, Vec3::new(1.15, 0.03, -0.9), 1.2),
        ]);
        let s = stick(&before, &sizes, Vec3::ZERO, 1).expect("sticks");
        assert_eq!(s.neighbour, 0);
        assert!(s.across && s.side > 0.0);
        let mut after = before.clone();
        after.insert(1, s.placement);
        let (gx, _) = seen_of(&after[&1], 1.2).gaps(&seen_of(&after[&0], 1.2));
        assert!(gx.abs() < 1e-3);
        // Far away: no magnet.
        let (before, _, sizes) = snap_case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            (1, Vec3::new(1.1, 0.0, 0.0), 1.2),
        ]);
        assert!(stick(&before, &sizes, Vec3::ZERO, 1).is_none());
    }

    #[test]
    fn snap_stacks_rows_without_overlap() {
        // One above the anchor, sunk into it.
        let (_, after, _) = snap_case(&[
            (0, Vec3::new(0.0, 0.0, -1.1), 1.2),
            (1, Vec3::new(0.05, 0.5, -1.0), 1.2),
        ]);
        let a = seen_of(&after[&0], 1.2);
        let m = seen_of(&after[&1], 1.2);
        let (gx, gy) = m.gaps(&a);
        assert!(gy.abs() < 1e-3, "vertical gap {gy}");
        assert!(gx < 0.0);
        // Lined up above it.
        assert!(m.yaw.abs() < 1e-3);
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
}
