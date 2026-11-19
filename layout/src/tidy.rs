//! "Line up" (the A button): it takes the crookedness out of each row and nothing more.
//!
//! How a whole row stands is the user's choice and stays: tilted back for a reclined
//! pose, lying down overhead, turned or rolled a little as a whole, its monitors turned
//! to face a head that was lower. Each row is one surface fitted to it, and only each
//! monitor's own deviation from that surface goes.
//!
//! - The row's axis, a line through the eye. A monitor stands on its row without
//!   leaning when its top and bottom edges are square to the axis, so the axis is the
//!   direction square to the right edges of the most monitors, found robustly: every
//!   two monitors whose right edges point apart by [`PAIR`] or more propose the one
//!   direction square to both, and the proposal that leaves the least lean of all the
//!   monitors and the least difference of their tilts from the pair's own wins (equal
//!   ones averaged), refined over the monitors it fits. One monitor hanging crooked
//!   among three or more does not move it. A row of one or two monitors takes the axis
//!   fitted to all of them unless one of its monitors leans more than [`OWN_TILT`] off
//!   it; monitors all in one column (right edges parallel) take the median of their
//!   ups.
//! - The row's tilt: the median of how far its monitors face up or down along the axis
//!   (0 when they face it square). Monitors turned to face a lower head keep facing it.
//! - On that surface, in the row's own frame (the axis up): every monitor faces the
//!   axis, tilted by the row's tilt, without leaning, turned about its own centre; the
//!   monitors close up side by side in the order they stand around the axis, the mean
//!   of their directions where it was (the least movement of them all, and no drift
//!   towards the primary); their mid-lines on one circle, at the mean distance their
//!   side edges were from the axis; and one line up and down: tops, middles or bottoms,
//!   whichever the monitors are nearest to, where that line was on average.
//! - A flat row (monitors parallel and in one plane, a wall) stays in its plane: side
//!   by side in it, the mean of their centres where it was, one line up and down.
//!
//! The rows keep their heights. Only where a row straightened would cover a monitor of
//! the row before it (outwards from the main one), or come so near in height that the
//! two could be taken for one row, is it moved along the axis of the whole just far
//! enough not to. A gap the user left between rows stays: the desktop closes it, the
//! headset shows it as it was.
//!
//! Everything is worked out from the monitors themselves around the eye, so turning the
//! whole arrangement about the eye (any yaw, pitch or roll that leaves its rows the
//! same) and lining up gives what lining up and turning gives; a mirror-symmetric
//! arrangement stays symmetric; lining up twice changes nothing the second time.

use std::collections::BTreeMap;

use glam::{Mat3, Quat, Vec3};

use crate::rows::rows_of;
use crate::space::{Frame, Monitor, PixelSize, Placement, contiguous, covers, facing};
use crate::view::Room;

/// A row of one or two monitors keeps an axis of its own only past this angle from the
/// axis fitted to all monitors (10°); nearer, it is a monitor standing crooked.
pub const OWN_TILT: f32 = 10.0 * std::f32::consts::PI / 180.0;
/// Two monitors propose an axis when their right edges point apart by this much (10°):
/// nearer to parallel, the direction square to both is not well defined.
pub const PAIR: f32 = 10.0 * std::f32::consts::PI / 180.0;
/// Monitors whose faces and right edges point within this angle of each other (and that
/// stand in one plane) make a flat row, a wall (3°).
const FLAT: f32 = 3.0 * std::f32::consts::PI / 180.0;
/// A monitor fits an axis when it leans off it by no more than this (1°).
const FITS: f32 = std::f32::consts::PI / 180.0;

/// Every monitor lined up (see the module's description), in the room. `main`: the
/// primary monitor, whose row keeps its place while the others make room.
/// `previous_rows`: the rows last worked out (see [`rows_of`]). `lid`: the laptop
/// screen, which hangs tilted under its monitor and is not lined up; it stays where it
/// is, or goes down just enough not to be covered by a monitor lined up over it.
pub fn line_up<K: Copy + Ord>(
    all: &[Monitor<K>],
    frame: &Frame,
    main: Option<K>,
    previous_rows: Option<&[Vec<K>]>,
    lid: Option<K>,
) -> BTreeMap<K, Placement> {
    let monitors: Vec<Monitor<K>> = all.iter().filter(|m| Some(m.id) != lid).copied().collect();
    let mut lined = line_up_rows(&monitors, frame, main, previous_rows);
    if let Some(l) = all.iter().find(|m| Some(m.id) == lid) {
        let mut p = frame.unplace(l.placement);
        // Only a monitor lined up over it pushes it down: one that covered it as it
        // stood (a curved monitor's flat chord bulging over it) leaves it be.
        let covered = |m: &Placement, h: f32, p: &Placement| {
            covers(&m.corners(h), &p.corners(l.height()), 0.001)
        };
        let others: Vec<[Vec3; 4]> = monitors
            .iter()
            .filter(|m| !covered(&frame.unplace(m.placement), m.height(), &p))
            .map(|m| frame.unplace(lined[&m.id]).corners(m.height()))
            .collect();
        let down = p.rotation * Vec3::Y;
        for _ in 0..100 {
            let mine = p.corners(l.height());
            if others.iter().all(|o| !covers(o, &mine, 0.001)) {
                break;
            }
            p.position -= down * 0.01;
        }
        lined.insert(l.id, frame.place(p));
    }
    lined
}

/// A monitor's edges as the fit sees them: where its centre is (from the eye), which
/// way its right edge, its up and its face point, and its size in metres.
#[derive(Debug, Clone, Copy)]
struct Pose {
    centre: Vec3,
    right: Vec3,
    up: Vec3,
    face: Vec3,
    width: f32,
    height: f32,
}

impl Pose {
    fn of(p: &Placement, px: PixelSize) -> Self {
        Self {
            centre: p.position,
            right: p.rotation * Vec3::X,
            up: p.rotation * Vec3::Y,
            face: p.rotation * Vec3::Z,
            width: p.width,
            height: p.width * px.aspect(),
        }
    }

    /// How far it leans off `axis` (its right edge out of square), radians.
    fn lean(&self, axis: Vec3) -> f32 {
        self.right.dot(axis).clamp(-1.0, 1.0).asin()
    }

    /// How far its face looks up along `axis`, radians.
    fn tilt(&self, axis: Vec3) -> f32 {
        self.face.dot(axis).clamp(-1.0, 1.0).asin()
    }
}

/// The median of `values` (the mean of the middle two for an even count).
fn median(mut values: Vec<f32>) -> f32 {
    values.sort_by(f32::total_cmp);
    let n = values.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// The axis fitted to `poses` (see the module's description), pointing to the side of
/// `up`; `None` when no two right edges point apart by [`PAIR`].
fn fit_axis(poses: &[Pose], up: Vec3) -> Option<Vec3> {
    let orient = |a: Vec3| if a.dot(up) < 0.0 { -a } else { a };
    let proposal = |i: usize, j: usize| -> Option<Vec3> {
        let c = poses[i].right.cross(poses[j].right);
        (c.length() >= PAIR.sin()).then(|| orient(c.normalize()))
    };
    // What a proposal of the pair `i`, `j` leaves: the lean of every monitor, and how far
    // each tilts from the pair's own tilt (a monitor that leans on its own then disagrees
    // with the others in both, so it never outvotes them).
    let cost = |a: Vec3, i: usize, j: usize| -> f32 {
        let tilt = (poses[i].tilt(a) + poses[j].tilt(a)) / 2.0;
        poses
            .iter()
            .map(|p| p.lean(a).abs() + (p.tilt(a) - tilt).abs())
            .sum()
    };
    let mut proposals: Vec<(f32, Vec3)> = Vec::new();
    for i in 0..poses.len() {
        for j in i + 1..poses.len() {
            if let Some(a) = proposal(i, j) {
                proposals.push((cost(a, i, j), a));
            }
        }
    }
    let least = proposals.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
    if !least.is_finite() {
        return None;
    }
    // Equal proposals (a symmetric arrangement proposes mirror images) are averaged.
    let tied: Vec3 = proposals
        .iter()
        .filter(|p| p.0 <= least + 1e-5 * poses.len() as f32)
        .map(|p| p.1)
        .sum();
    let best = orient(tied.normalize_or(proposals[0].1));
    // Refined over the monitors it fits: every two of them, weighted by how well their
    // right edges define the direction square to both.
    let fit: Vec<&Pose> = poses
        .iter()
        .filter(|p| p.lean(best).abs() <= FITS)
        .collect();
    let mut refined = Vec3::ZERO;
    for i in 0..fit.len() {
        for j in i + 1..fit.len() {
            let c = fit[i].right.cross(fit[j].right);
            refined += if c.dot(best) < 0.0 { -c } else { c };
        }
    }
    Some(if refined.length() >= PAIR.sin() {
        orient(refined.normalize())
    } else {
        best
    })
}

/// The geometric median of unit vectors (the point least far from them all, Weiszfeld's
/// iteration from their mean), made unit: a monitor tilted on its own among three or
/// more does not move it.
fn median_direction(v: &[Vec3]) -> Vec3 {
    let mut m: Vec3 = v.iter().copied().sum::<Vec3>() / v.len().max(1) as f32;
    for _ in 0..50 {
        let (mut sum, mut weights) = (Vec3::ZERO, 0.0f32);
        for &x in v {
            let w = 1.0 / x.distance(m).max(1e-6);
            sum += x * w;
            weights += w;
        }
        let next = sum / weights.max(1e-12);
        if next.distance(m) < 1e-7 {
            m = next;
            break;
        }
        m = next;
    }
    m.normalize_or(Vec3::Y)
}

/// A row's own frame: `up` along +Y, its right as near the frame's right as can be.
/// Only directions relative to the row are used in it, so how its right is chosen does
/// not matter.
fn row_frame(up: Vec3) -> Quat {
    let y = up.normalize_or(Vec3::Y);
    let x = (Vec3::X - y * y.dot(Vec3::X))
        .try_normalize()
        .unwrap_or_else(|| (Vec3::NEG_Z - y * y.dot(Vec3::NEG_Z)).normalize());
    Quat::from_mat3(&Mat3::from_cols(x, y, x.cross(y))).normalize()
}

/// Whether the monitors of a row make a flat row: faces and right edges parallel, and
/// in one plane.
fn is_flat(poses: &[Pose]) -> bool {
    let Some(first) = poses.first() else {
        return false;
    };
    let depth = |p: &Pose| -p.centre.dot(first.face);
    poses.len() > 1
        && poses.iter().all(|p| {
            p.face.angle_between(first.face) < FLAT
                && p.right.angle_between(first.right) < FLAT
                && (depth(p) - depth(first)).abs() < 0.02
        })
}

/// [`line_up`] of the monitors without the laptop screen.
fn line_up_rows<K: Copy + Ord>(
    monitors: &[Monitor<K>],
    frame: &Frame,
    main: Option<K>,
    previous_rows: Option<&[Vec<K>]>,
) -> BTreeMap<K, Placement> {
    let room = Room::new(frame, monitors);
    let ids: Vec<K> = monitors.iter().map(|m| m.id).collect();
    let rows = rows_of(&ids, &room, previous_rows);
    // In the frame: the eye at the origin.
    let local: BTreeMap<K, (Placement, PixelSize)> = monitors
        .iter()
        .map(|m| (m.id, (frame.unplace(m.placement), m.px)))
        .collect();
    let pose = |k: K| Pose::of(&local[&k].0, local[&k].1);
    let everyone: Vec<Pose> = ids.iter().map(|&k| pose(k)).collect();
    let all_up = median_direction(&everyone.iter().map(|p| p.up).collect::<Vec<_>>());
    // Monitors all in one column (right edges parallel) say nothing about the axis by
    // their edges: then their median up, which three or more keep from a crooked one.
    let all_axis = fit_axis(&everyone, all_up).or((everyone.len() >= 3).then_some(all_up));

    let mut out: BTreeMap<K, Placement> = BTreeMap::new();
    let mut axes: Vec<Vec3> = Vec::new();
    for row in &rows {
        let poses: Vec<Pose> = row.iter().map(|&k| pose(k)).collect();
        let flat = is_flat(&poses);
        let row_up = median_direction(&poses.iter().map(|p| p.up).collect::<Vec<_>>());
        let own = if flat { None } else { fit_axis(&poses, row_up) };
        // Three or more fit their own axis; fewer take the axis of the whole unless one
        // of them leans more than OWN_TILT off it (a pair's own axis turns with either
        // monitor's lean, magnified, so it is no measure of a deliberate tilt).
        let whole = all_axis.filter(|b| poses.iter().all(|p| p.lean(*b).abs() < OWN_TILT));
        let mut axis = match (own, whole) {
            (Some(a), _) if poses.len() >= 3 => a,
            (_, Some(b)) => b,
            (Some(a), None) => a,
            (None, None) => all_axis.filter(|_| flat).unwrap_or(row_up),
        };
        if flat {
            // In the plane of the wall.
            let face = median_direction(&poses.iter().map(|p| p.face).collect::<Vec<_>>());
            axis = (axis - face * axis.dot(face)).normalize_or(row_up);
        }
        axes.push(axis);
        let turn = row_frame(axis);
        let in_row: Vec<(K, Placement, PixelSize)> = row
            .iter()
            .map(|&k| {
                let (p, px) = local[&k];
                let p = Placement {
                    position: turn.inverse() * p.position,
                    rotation: (turn.inverse() * p.rotation).normalize(),
                    width: p.width,
                };
                (k, p, px)
            })
            .collect();
        let straight = if flat {
            straighten_flat(&in_row)
        } else {
            straighten(&in_row)
        };
        for (k, p) in straight {
            let p = Placement {
                position: turn * p.position,
                rotation: (turn * p.rotation).normalize(),
                width: p.width,
            };
            out.insert(k, p);
        }
    }

    // The rows outwards from the main one, moved along the axis of the whole (else the
    // main row's) and seen in its frame, so they keep apart however the whole stands.
    let main_row = main
        .and_then(|m| rows.iter().position(|r| r.contains(&m)))
        .unwrap_or(0);
    let stack = all_axis.unwrap_or_else(|| axes.get(main_row).copied().unwrap_or(Vec3::Y));
    let turn = row_frame(stack);
    let height = |k: K, p: &Placement| p.width * local[&k].1.aspect();
    let mut inner: Vec<K> = rows.get(main_row).cloned().unwrap_or_default();
    let order: Vec<(usize, f32)> = (0..main_row)
        .rev()
        .map(|r| (r, 1.0))
        .chain((main_row + 1..rows.len()).map(|r| (r, -1.0)))
        .collect();
    for (r, sign) in order {
        let dir = stack * sign;
        let tallest = rows[r]
            .iter()
            .map(|&k| height(k, &out[&k]))
            .fold(0.0f32, f32::max);
        let ok = |shift: f32| apart(&rows[r], &inner, &out, &local, turn, dir * shift, &rows);
        let mut shift = 0.0f32;
        if !ok(0.0) {
            let step = tallest / 8.0;
            let mut hi = step;
            while !ok(hi) && hi < 64.0 * tallest {
                hi += step;
            }
            let mut lo = hi - step;
            for _ in 0..40 {
                let mid = (lo + hi) / 2.0;
                if ok(mid) {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            shift = hi;
        }
        if shift != 0.0 {
            for &k in &rows[r] {
                if let Some(p) = out.get_mut(&k) {
                    p.position += dir * shift;
                }
            }
        }
        inner.extend(rows[r].iter().copied());
    }
    out.into_iter().map(|(k, p)| (k, frame.place(p))).collect()
}

/// How a row's monitors line up when its steps go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lining {
    Top,
    Middle,
    Bottom,
}

/// The line a row's monitors are nearest to: tops, middles or bottoms, summed over
/// every two monitors of different heights (`spans` are `(top, bottom)`); middles when
/// all are of a height, and on a tie (to a tenth of a millimetre, so rounding never
/// decides it): then tops before bottoms.
fn lining(spans: &[(f32, f32)]) -> Lining {
    const TIE: f32 = 1e-4;
    let mut cost = [0.0f32; 3];
    for (i, &(ta, ba)) in spans.iter().enumerate() {
        for &(tb, bb) in &spans[i + 1..] {
            let taller = (ta - ba).max(tb - bb);
            if ((ta - ba) - (tb - bb)).abs() <= 0.02 * taller {
                continue;
            }
            cost[0] += (ta - tb).abs();
            cost[1] += ((ta + ba) - (tb + bb)).abs() / 2.0;
            cost[2] += (ba - bb).abs();
        }
    }
    if cost[1] <= cost[0] + TIE && cost[1] <= cost[2] + TIE {
        Lining::Middle
    } else if cost[0] <= cost[2] + TIE {
        Lining::Top
    } else {
        Lining::Bottom
    }
}

/// The height of every monitor's centre when the row's steps go: `spans` (top, bottom)
/// as they stand, the monitors' heights `tall` as they will stand.
fn one_line(spans: &[(f32, f32)], tall: &[f32]) -> Vec<f32> {
    let n = spans.len().max(1) as f32;
    match lining(spans) {
        Lining::Middle => {
            let line = spans.iter().map(|s| (s.0 + s.1) / 2.0).sum::<f32>() / n;
            tall.iter().map(|_| line).collect()
        }
        Lining::Top => {
            let line = spans.iter().map(|s| s.0).sum::<f32>() / n;
            tall.iter().map(|h| line - h / 2.0).collect()
        }
        Lining::Bottom => {
            let line = spans.iter().map(|s| s.1).sum::<f32>() / n;
            tall.iter().map(|h| line + h / 2.0).collect()
        }
    }
}

/// One row straightened in its own frame (its axis along +Y through the eye at the
/// origin): see the module's description.
fn straighten<K: Copy + Ord>(row: &[(K, Placement, PixelSize)]) -> Vec<(K, Placement)> {
    let poses: Vec<Pose> = row.iter().map(|(_, p, px)| Pose::of(p, *px)).collect();
    let tilt = median(poses.iter().map(|p| p.tilt(Vec3::Y)).collect());
    // The circle: where the side edges would be with the centres where they are.
    let flat_distance = |v: Vec3| Vec3::new(v.x, 0.0, v.z).length();
    let radius = {
        let r = poses
            .iter()
            .map(|p| (flat_distance(p.centre).powi(2) + p.width.powi(2) / 4.0).sqrt())
            .sum::<f32>()
            / poses.len() as f32;
        let widest = poses.iter().map(|p| p.width).fold(0.0f32, f32::max);
        r.max(widest / 2.0 * 1.0001)
    };
    let half = |w: f32| f64::from((w / 2.0 / radius).min(1.0).asin());
    // Directions around the axis, near the row's own.
    // Directions around the axis, cut behind the row.
    let az: Vec<f64> = contiguous(
        &poses
            .iter()
            .map(|p| f64::from(p.centre.x).atan2(f64::from(-p.centre.z)))
            .collect::<Vec<_>>(),
    );
    let mut order: Vec<usize> = (0..row.len()).collect();
    order.sort_by(|&a, &b| az[a].total_cmp(&az[b]).then(row[a].0.cmp(&row[b].0)));
    // Side by side from `start`, the mean direction kept.
    let mut offsets = vec![0.0f64; row.len()];
    let mut x = 0.0f64;
    for &i in &order {
        offsets[i] = x + half(poses[i].width);
        x += 2.0 * half(poses[i].width);
    }
    let n = row.len() as f64;
    let start = az.iter().sum::<f64>() / n - offsets.iter().sum::<f64>() / n;
    // Up and down: one line, the heights as they will stand.
    let tall: Vec<f32> = poses.iter().map(|p| p.height * tilt.cos()).collect();
    let spans: Vec<(f32, f32)> = poses
        .iter()
        .zip(&tall)
        .map(|(p, h)| (p.centre.y + h / 2.0, p.centre.y - h / 2.0))
        .collect();
    let ys = one_line(&spans, &tall);
    let lean_back = Quat::from_rotation_x(-tilt);
    (0..row.len())
        .map(|i| {
            let yaw = (start + offsets[i]) as f32;
            let r = radius * half(poses[i].width).cos() as f32;
            let position = Vec3::new(r * yaw.sin(), ys[i], -r * yaw.cos());
            // Facing the axis; one that faced away from it (the surface does not suit
            // it) keeps facing its own way.
            let inward = Vec3::new(-position.x, 0.0, -position.z);
            let own = Vec3::new(poses[i].face.x, 0.0, poses[i].face.z);
            let towards = if own.dot(inward) < 0.0 {
                position + own
            } else {
                Vec3::new(0.0, ys[i], 0.0)
            };
            let upright = facing(position, towards);
            let p = Placement {
                position,
                rotation: (upright * lean_back).normalize(),
                width: poses[i].width,
            };
            (row[i].0, p)
        })
        .collect()
}

/// A flat row straightened in its own frame (its axis along +Y, the wall upright):
/// side by side in the wall's plane, the mean of the centres where it was, one line up
/// and down.
fn straighten_flat<K: Copy + Ord>(row: &[(K, Placement, PixelSize)]) -> Vec<(K, Placement)> {
    let poses: Vec<Pose> = row.iter().map(|(_, p, px)| Pose::of(p, *px)).collect();
    let face = {
        let f = median_direction(&poses.iter().map(|p| p.face).collect::<Vec<_>>());
        Vec3::new(f.x, 0.0, f.z).normalize_or(Vec3::Z)
    };
    let right = Vec3::Y.cross(face);
    let along: Vec<f32> = poses.iter().map(|p| p.centre.dot(right)).collect();
    let depth = poses.iter().map(|p| -p.centre.dot(face)).sum::<f32>() / poses.len() as f32;
    let mut order: Vec<usize> = (0..row.len()).collect();
    order.sort_by(|&a, &b| along[a].total_cmp(&along[b]).then(row[a].0.cmp(&row[b].0)));
    let mut offsets = vec![0.0f32; row.len()];
    let mut x = 0.0f32;
    for &i in &order {
        offsets[i] = x + poses[i].width / 2.0;
        x += poses[i].width;
    }
    let n = row.len() as f32;
    let start = along.iter().sum::<f32>() / n - offsets.iter().sum::<f32>() / n;
    let tall: Vec<f32> = poses.iter().map(|p| p.height).collect();
    let spans: Vec<(f32, f32)> = poses
        .iter()
        .map(|p| (p.centre.y + p.height / 2.0, p.centre.y - p.height / 2.0))
        .collect();
    let ys = one_line(&spans, &tall);
    let rotation = Quat::from_rotation_y(face.x.atan2(face.z));
    (0..row.len())
        .map(|i| {
            let position = right * (start + offsets[i]) - face * depth + Vec3::Y * ys[i];
            let p = Placement {
                position,
                rotation,
                width: poses[i].width,
            };
            (row[i].0, p)
        })
        .collect()
}

/// Whether `row` moved by `shift` keeps apart from the monitors `inner`: none covers
/// one of them as seen, and, seen in the frame `turn` (the whole's), they make the same
/// rows as in `rows`, so no two rows are taken for one. Corners and rows are taken in
/// that frame: where two monitors are too far apart around the eye to be projected
/// together, covering falls back to angles, which must be the whole's own.
fn apart<K: Copy + Ord>(
    row: &[K],
    inner: &[K],
    out: &BTreeMap<K, Placement>,
    local: &BTreeMap<K, (Placement, PixelSize)>,
    turn: Quat,
    shift: Vec3,
    rows: &[Vec<K>],
) -> bool {
    let placed = |k: K| {
        let mut p = out[&k];
        if row.contains(&k) {
            p.position += shift;
        }
        Placement {
            position: turn.inverse() * p.position,
            rotation: (turn.inverse() * p.rotation).normalize(),
            width: p.width,
        }
    };
    let corners = |k: K| placed(k).corners(placed(k).width * local[&k].1.aspect());
    let covering = row.iter().any(|&u| {
        inner
            .iter()
            .any(|&p| covers(&corners(p), &corners(u), 0.001))
    });
    if covering {
        return false;
    }
    let ids: Vec<K> = inner.iter().chain(row).copied().collect();
    let monitors: Vec<Monitor<K>> = ids
        .iter()
        .map(|&k| Monitor {
            id: k,
            px: local[&k].1,
            placement: placed(k),
        })
        .collect();
    let room = Room::new(&Frame::upright(Vec3::ZERO, 0.0), &monitors);
    let now = rows_of(&ids, &room, Some(rows));
    let partition = |rows: &[Vec<K>]| -> Vec<Vec<K>> {
        let mut p: Vec<Vec<K>> = rows
            .iter()
            .map(|r| {
                let mut r: Vec<K> = r.iter().copied().filter(|k| ids.contains(k)).collect();
                r.sort();
                r
            })
            .filter(|r| !r.is_empty())
            .collect();
        p.sort();
        p
    };
    partition(&now) == partition(rows)
}
