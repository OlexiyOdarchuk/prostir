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

/// Lines up monitors the user placed by hand, keeping their arrangement: monitors at
/// about the same height stay a row, and each row becomes tidy, side by side without
/// gaps or overlaps, at one distance and height, facing the user, centred where the row
/// was. Rows that overlap or nearly touch are stacked so they just touch. `center` is
/// where the layout faces from (position, yaw).
pub fn tidy(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    center: (Vec3, f32),
) -> HashMap<u8, Placement> {
    struct Item {
        id: u8,
        yaw: f32,
        y: f32,
        distance: f32,
        width: f32,
        height: f32,
    }
    let unturn = Quat::from_rotation_y(-center.1);
    let turn = Quat::from_rotation_y(center.1);
    let mut items: Vec<Item> = monitors
        .iter()
        .filter_map(|&(id, w, h)| {
            let p = placements.get(&id)?;
            let local = unturn * (p.position - center.0);
            Some(Item {
                id,
                yaw: local.x.atan2(-local.z),
                y: local.y,
                distance: Vec2::new(local.x, local.z).length().max(0.3),
                width: p.width,
                height: p.width * h as f32 / w.max(1) as f32,
            })
        })
        .collect();
    items.sort_by(|a, b| b.y.total_cmp(&a.y));
    // Rows: centres closer in height than most of a monitor.
    let mut rows: Vec<Vec<Item>> = Vec::new();
    for it in items {
        match rows.iter_mut().find(|r| {
            let first = &r[0];
            (first.y - it.y).abs() < 0.5 * first.height.max(it.height)
        }) {
            Some(r) => r.push(it),
            None => rows.push(vec![it]),
        }
    }
    // Each row: one height (the average), one distance, side by side.
    struct Row {
        y: f32,
        height: f32,
        distance: f32,
        items: Vec<Item>,
    }
    let mut rows: Vec<Row> = rows
        .into_iter()
        .map(|mut r| {
            r.sort_by(|a, b| a.yaw.total_cmp(&b.yaw));
            let n = r.len() as f32;
            Row {
                y: r.iter().map(|m| m.y).sum::<f32>() / n,
                height: r.iter().map(|m| m.height).fold(0.0, f32::max),
                distance: r.iter().map(|m| m.distance).sum::<f32>() / n,
                items: r,
            }
        })
        .collect();
    // Top to bottom: a row that overlaps the one above, or almost touches it, moves
    // down to touch it.
    rows.sort_by(|a, b| b.y.total_cmp(&a.y));
    for i in 1..rows.len() {
        let above_bottom = rows[i - 1].y - rows[i - 1].height / 2.0;
        let top = rows[i].y + rows[i].height / 2.0;
        let gap = above_bottom - top;
        if gap < 0.15 * rows[i].height {
            rows[i].y = above_bottom - rows[i].height / 2.0;
        }
    }
    let mut out = HashMap::new();
    for row in &rows {
        let half_angle = |w: f32| (w / 2.0 / row.distance).min(0.95).asin();
        // Centred where the row's edges were.
        let first = &row.items[0];
        let last = &row.items[row.items.len() - 1];
        let mid =
            ((first.yaw - half_angle(first.width)) + (last.yaw + half_angle(last.width))) / 2.0;
        let total: f32 = row.items.iter().map(|m| 2.0 * half_angle(m.width)).sum();
        let mut yaw = mid - total / 2.0;
        for m in &row.items {
            let half = half_angle(m.width);
            let a = yaw + half;
            let r = row.distance * half.cos();
            let local = Vec3::new(r * a.sin(), row.y, -r * a.cos());
            let position = center.0 + turn * local;
            out.insert(
                m.id,
                Placement {
                    position,
                    rotation: turn * facing(local, Vec3::new(0.0, row.y, 0.0)),
                    width: m.width,
                },
            );
            yaw += 2.0 * half;
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
        Preset::Grid if sorted.len() > 2 => {
            let split = sorted.len().div_ceil(2);
            let (bottom, top) = sorted.split_at(split);
            let hb = bottom.iter().map(|m| m.2).fold(0.0, f32::max);
            let ht = top.iter().map(|m| m.2).fold(0.0, f32::max);
            // The seam between the rows sits at eye height.
            row(bottom, -hb / 2.0, &mut out);
            row(top, ht / 2.0, &mut out);
        }
        Preset::Grid => row(&sorted, -EYE_DROP, &mut out),
        Preset::Column => {
            let total: f32 = sorted.iter().map(|m| m.2).sum();
            let mut top = total / 2.0 - EYE_DROP;
            for &(id, w, h) in &sorted {
                out.insert(id, on_cylinder(0.0, top - h / 2.0, w));
                top -= h;
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
        for preset in [Preset::Arc, Preset::Grid, Preset::Column, Preset::Overhead] {
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

        // A grid of four: two rows of two.
        let four = [
            (0, 2560, 1440),
            (1, 2560, 1440),
            (2, 2560, 1440),
            (3, 2560, 1440),
        ];
        let grid = arrange(Preset::Grid, &four, None, None);
        let rows = rows_by_position(&grid, &four, center);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.ids.len() == 2));
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
        let tidy = tidy(&rough, &three, center);
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
}
