//! Where monitors float in the room, how controllers move them, and persistence.

use std::collections::HashMap;
use std::path::Path;

use glam::{Quat, Vec2, Vec3};

/// Physical width of a 2560 px wide monitor at the default layout, metres.
const DEFAULT_WIDTH_PER_2560PX: f32 = 1.25;
const DEFAULT_RADIUS: f32 = 1.1;
const DEFAULT_HEIGHT: f32 = -0.08;
const GAP_RAD: f32 = 0.025;

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

/// Monitors side by side on an arc around the user, ordered by id from left to right.
/// `monitors` holds `(id, width_px, height_px)`.
pub fn default_layout(monitors: &[(u8, u32, u32)]) -> HashMap<u8, Placement> {
    let mut sorted = monitors.to_vec();
    sorted.sort_by_key(|m| m.0);
    let widths: Vec<f32> = sorted
        .iter()
        .map(|m| m.1 as f32 / 2560.0 * DEFAULT_WIDTH_PER_2560PX)
        .collect();
    let angles: Vec<f32> = widths
        .iter()
        .map(|w| 2.0 * (w / 2.0 / DEFAULT_RADIUS).min(0.95).asin())
        .collect();
    let total: f32 = angles.iter().sum::<f32>() + GAP_RAD * (angles.len().saturating_sub(1)) as f32;
    let mut theta = -total / 2.0;
    let mut out = HashMap::new();
    for ((m, w), a) in sorted.iter().zip(&widths).zip(&angles) {
        let center = theta + a / 2.0;
        // Pull the centre in so the edges, not the middle, sit on the arc.
        let r = DEFAULT_RADIUS * (a / 2.0).cos();
        let position = Vec3::new(r * center.sin(), DEFAULT_HEIGHT, -r * center.cos());
        let rotation = facing(position, Vec3::new(0.0, DEFAULT_HEIGHT, 0.0));
        out.insert(
            m.0,
            Placement {
                position,
                rotation,
                width: *w,
            },
        );
        theta += a + GAP_RAD;
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
    fn default_layout_faces_user() {
        let l = default_layout(&[(0, 2560, 1440), (1, 1920, 1080)]);
        for p in l.values() {
            let normal = p.rotation * Vec3::Z;
            let to_user = (Vec3::new(0.0, DEFAULT_HEIGHT, 0.0) - p.position).normalize();
            assert!(normal.dot(to_user) > 0.999);
        }
        assert!(l[&0].position.x < l[&1].position.x);
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
