//! Where monitors float in the room, how controllers move them, and persistence.

use std::collections::HashMap;
use std::path::Path;

use glam::{Quat, Vec2, Vec3};

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
/// `(id, width_px, height_px)`; Quest monitors go left to right by id, like in KDE,
/// and the laptop mirror goes under the primary monitor.
pub fn arrange(
    preset: Preset,
    monitors: &[(u8, u32, u32)],
    primary: Option<u8>,
    mirror: Option<(u8, u32, u32)>,
) -> HashMap<u8, Placement> {
    let mut sorted: Vec<(u8, f32, f32)> = monitors
        .iter()
        .map(|&(id, w, h)| (id, w as f32 * METRES_PER_PIXEL, h as f32 * METRES_PER_PIXEL))
        .collect();
    sorted.sort_by_key(|m| m.0);
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
        // Below the lowest monitor, so it never covers another one.
        let lowest = sorted
            .iter()
            .filter_map(|m| out.get(&m.0).map(|p| p.position.y - m.2 / 2.0))
            .fold(f32::INFINITY, f32::min);
        let bottom = if lowest.is_finite() {
            lowest
        } else {
            -EYE_DROP
        };
        // Tilted back towards the eyes like a laptop lid, since it sits below them.
        let mut p = on_cylinder(yaw, bottom - h / 2.0, w);
        p.rotation = facing(p.position, Vec3::ZERO);
        out.insert(id, p);
    }

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
}

impl Grab {
    pub fn start(id: u8, distance: f32, local_hit: Vec2, placement: &Placement) -> Self {
        Self {
            id,
            distance,
            anchor: local_hit / placement.width,
        }
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
        self.distance = (self.distance * (1.0 + dead(stick.y) * 1.5 * dt)).clamp(0.3, 6.0);
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
    if let Err(e) = std::fs::write(path, text) {
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
}
