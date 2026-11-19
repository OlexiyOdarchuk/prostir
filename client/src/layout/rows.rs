//! Rows as the eyes see them, and the rules that keep them: what the PC lays its
//! desktop out by, what "Line up" straightens, where a new monitor goes.
//!
//! One function says which monitors make a row ([`rows_of`]): those whose heights, as
//! seen from where the arrangement faces, overlap by at least half the shorter one
//! (and any joined through such a pair). Everything else goes by it.

use std::collections::HashMap;

use glam::{Quat, Vec2, Vec3};
use prostir_layout::desk;
use prostir_proto::{LayoutEntry, LayoutRow, MIRROR_ID};

use super::{
    Beside, MAX_ARC, OVERHEAD_PITCH, Placement, angular_width, facing, hinge, on_cylinder,
    wall_width,
};
use crate::settings::Preset;

/// A monitor as seen from where the arrangement faces: the middle of it across and up
/// (radians) and half its size there.
#[derive(Debug, Clone, Copy)]
pub struct Seen {
    pub id: u8,
    pub yaw: f32,
    pub elevation: f32,
    pub half_width: f32,
    pub half_height: f32,
    /// Stream size in pixels.
    pub px: (u32, u32),
}

impl Seen {
    /// Whether the two cover each other as seen (more than a hair).
    pub fn overlaps(&self, o: &Seen) -> bool {
        const HAIR: f32 = 1e-3;
        (self.yaw - o.yaw).abs() < self.half_width + o.half_width - HAIR
            && (self.elevation - o.elevation).abs() < self.half_height + o.half_height - HAIR
    }

    /// How much of the shorter one's height the two share.
    fn shared_height(&self, o: &Seen) -> f32 {
        let top = (self.elevation + self.half_height).min(o.elevation + o.half_height);
        let bottom = (self.elevation - self.half_height).max(o.elevation - o.half_height);
        (top - bottom) / (2.0 * self.half_height.min(o.half_height)).max(1e-6)
    }
}

/// How the arrangement is looked at: from `center` (eye, yaw), lying down along the
/// band tilted up overhead (seen from below, left and right would make no sense).
fn view(center: (Vec3, f32), preset: Preset) -> Quat {
    let tilt = if preset == Preset::Overhead {
        OVERHEAD_PITCH
    } else {
        0.0
    };
    Quat::from_rotation_x(-tilt) * Quat::from_rotation_y(-center.1)
}

/// Every monitor of `monitors` (id, pixel size) that has a placement, as seen.
pub fn seen(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    center: (Vec3, f32),
    preset: Preset,
) -> Vec<Seen> {
    let unturn = view(center, preset);
    monitors
        .iter()
        .filter_map(|&(id, w, h)| {
            let p = placements.get(&id)?;
            let local = unturn * (p.position - center.0);
            let flat = Vec2::new(local.x, local.z).length().max(0.1);
            let height = p.width * h as f32 / w.max(1) as f32;
            // Its top and bottom edges, as far up as they are seen at its side edges
            // (tilted towards the eyes or not). Not at its middle: on a circle a wide
            // monitor's middle is nearer and would look taller than its neighbours,
            // though their edges meet.
            let up = unturn * (p.rotation * Vec3::Y) * height / 2.0;
            let side = unturn * (p.rotation * Vec3::X) * p.width / 2.0;
            let away = (Vec2::new(local.x + side.x, local.z + side.z).length()
                + Vec2::new(local.x - side.x, local.z - side.z).length())
                / 2.0;
            let edge = |e: Vec3| (local.y + e.y).atan2(away.max(0.1));
            let (top, bottom) = (edge(up), edge(-up));
            Some(Seen {
                id,
                yaw: local.x.atan2(-local.z),
                elevation: (top + bottom) / 2.0,
                half_width: (p.width / 2.0 / flat).atan(),
                half_height: ((top - bottom) / 2.0).abs(),
                px: (w, h),
            })
        })
        .collect()
}

/// The rows: monitors whose heights as seen overlap by half the shorter one at least,
/// and those joined through them, but never two over one another (overlapping
/// across). Pairs that share more of their height are joined first. Rows from the
/// top, monitors left to right.
pub fn rows_of(seen: &[Seen]) -> Vec<Vec<Seen>> {
    let across = |a: &Seen, b: &Seen| (a.yaw - b.yaw).abs() < a.half_width + b.half_width - 1e-3;
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for i in 0..seen.len() {
        for j in i + 1..seen.len() {
            let shared = seen[i].shared_height(&seen[j]);
            if shared >= 0.5 && !across(&seen[i], &seen[j]) {
                pairs.push((shared, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then((a.1, a.2).cmp(&(b.1, b.2))));
    // Each monitor's row, by the index of one of its monitors.
    let mut row: Vec<usize> = (0..seen.len()).collect();
    for (_, i, j) in pairs {
        let (ri, rj) = (row[i], row[j]);
        if ri == rj {
            continue;
        }
        let clash = (0..seen.len()).any(|a| {
            row[a] == ri && (0..seen.len()).any(|b| row[b] == rj && across(&seen[a], &seen[b]))
        });
        if !clash {
            for r in &mut row {
                if *r == rj {
                    *r = ri;
                }
            }
        }
    }
    let mut rows: Vec<Vec<Seen>> = Vec::new();
    let mut keys: Vec<usize> = Vec::new();
    for (i, m) in seen.iter().enumerate() {
        match keys.iter().position(|k| *k == row[i]) {
            Some(k) => rows[k].push(*m),
            None => {
                keys.push(row[i]);
                rows.push(vec![*m]);
            }
        }
    }
    let middle = |r: &Vec<Seen>| r.iter().map(|m| m.elevation).sum::<f32>() / r.len() as f32;
    rows.sort_by(|a, b| middle(b).total_cmp(&middle(a)));
    for r in &mut rows {
        r.sort_by(|a, b| a.yaw.total_cmp(&b.yaw));
    }
    rows
}

/// Pixels of the streams per radian as seen, the same for all monitors.
fn px_per_rad(seen: &[Seen]) -> f32 {
    seen.iter()
        .map(|m| m.px.0 as f32 / (2.0 * m.half_width).max(1e-3))
        .sum::<f32>()
        / seen.len().max(1) as f32
}

/// The rows for the PC's desktop (see `PROTOCOL.md`, "Layout"), where each monitor is
/// seen in pixels, rounded to 8 px so a hand's tremor does not rearrange the desktop.
///
/// Across, every row is unrolled side by side in its monitors' own pixels (as the PC
/// lays a row out), and lies over the row next to it inwards (towards the row with
/// `primary`) where it is seen over it: a monitor's angle across that row maps to the
/// pixel at that angle on the monitor there. Seen on a circle, a wide monitor takes
/// fewer degrees for its pixels than a narrow one, so one scale for all would put a
/// row above off its place. Down, the angles times pixels per radian.
pub fn layout_rows(seen: &[Seen], primary: Option<u8>) -> Vec<LayoutRow> {
    let scale = px_per_rad(seen);
    let snap = |v: f32| (v / 8.0).round() as i32 * 8;
    let rows = rows_of(seen);
    let main = rows
        .iter()
        .position(|r| r.iter().any(|m| Some(m.id) == primary))
        .unwrap_or(0);
    // Left edge in pixels of each monitor, row by row.
    let mut lefts: Vec<Vec<f32>> = vec![Vec::new(); rows.len()];
    let unrolled = |row: &[Seen], first: f32| -> Vec<f32> {
        let mut x = first;
        row.iter()
            .map(|m| {
                let left = x;
                x += m.px.0 as f32;
                left
            })
            .collect()
    };
    lefts[main] = unrolled(
        &rows[main],
        (rows[main][0].yaw - rows[main][0].half_width) * scale,
    );
    // Inner rows first: each lies over the one placed before it.
    let order: Vec<(usize, usize)> = (0..main)
        .rev()
        .map(|r| (r, r + 1))
        .chain((main + 1..rows.len()).map(|r| (r, r - 1)))
        .collect();
    for (r, inward) in order {
        let (under, at) = (&rows[inward], &lefts[inward]);
        // The pixel at angle `yaw` along the inward row.
        let px_at = |yaw: f32| {
            let spans: Vec<(f32, f32, f32, f32)> = under
                .iter()
                .zip(at)
                .map(|(m, l)| {
                    (
                        m.yaw - m.half_width,
                        m.yaw + m.half_width,
                        *l,
                        m.px.0 as f32,
                    )
                })
                .collect();
            if let Some(&(a, b, l, w)) = spans.iter().find(|s| (s.0..=s.1).contains(&yaw)) {
                return l + (yaw - a) / (b - a).max(1e-6) * w;
            }
            // Past an end or in a gap: from the nearest edge, at the common scale.
            spans
                .iter()
                .flat_map(|&(a, b, l, w)| [(a, l), (b, l + w)])
                .min_by(|x, y| (x.0 - yaw).abs().total_cmp(&(y.0 - yaw).abs()))
                .map_or(yaw * scale, |(edge, px)| px + (yaw - edge) * scale)
        };
        // The monitor seen most over the inward row keeps its place over it.
        let over = |m: &Seen| {
            under
                .iter()
                .map(|n| {
                    ((m.yaw + m.half_width).min(n.yaw + n.half_width)
                        - (m.yaw - m.half_width).max(n.yaw - n.half_width))
                    .max(0.0)
                })
                .sum::<f32>()
        };
        let row = &rows[r];
        let anchor = (0..row.len())
            .max_by(|&a, &b| over(&row[a]).total_cmp(&over(&row[b])))
            .unwrap_or(0);
        // Its middle over the pixel seen under its middle.
        let a = &row[anchor];
        let anchor_left = px_at(a.yaw) - a.px.0 as f32 / 2.0;
        let before: f32 = row[..anchor].iter().map(|m| m.px.0 as f32).sum();
        lefts[r] = unrolled(row, anchor_left - before);
    }
    rows.iter()
        .zip(&lefts)
        .map(|(row, lefts)| LayoutRow {
            entries: row
                .iter()
                .zip(lefts)
                .map(|(m, left)| LayoutEntry {
                    id: m.id,
                    center: Some(snap(left + m.px.0 as f32 / 2.0)),
                    middle: Some(snap(-m.elevation * scale)),
                })
                .collect(),
        })
        .collect()
}

/// The monitors for the map in the menu, as the PC lays its desktop out from these
/// rows (`prostir_layout::desk`): centre and half size in pixels, up positive.
pub fn desktop_spots(seen: &[Seen], primary: Option<u8>) -> HashMap<u8, [f32; 4]> {
    let rows = layout_rows(seen, primary);
    let sizes: HashMap<u8, (u32, u32)> = seen.iter().map(|m| (m.id, m.px)).collect();
    let plan = desk::plan(
        &desk::Desk {
            rows: rows
                .iter()
                .map(|r| {
                    r.entries
                        .iter()
                        .map(|e| desk::Entry {
                            id: e.id,
                            center: e.center,
                            middle: e.middle,
                        })
                        .collect()
                })
                .collect(),
            main: primary,
            ..desk::Desk::default()
        },
        |id| {
            let (w, h) = sizes.get(&id)?;
            Some(desk::Size {
                w: *w as i32,
                h: *h as i32,
            })
        },
    );
    plan.into_iter()
        .map(|s| {
            let (x, y) = (s.x + s.w / 2, s.y + s.h / 2);
            (
                s.id,
                [x as f32, -y as f32, s.w as f32 / 2.0, s.h as f32 / 2.0],
            )
        })
        .collect()
}

/// "Line up" for monitors around the user: every row on its own, monitors side by side
/// on their row's circle at the size they have (moved out when they do not fit its
/// arc), their middles on one line, `anchor` (or the one nearest straight ahead)
/// staying where it is. Rows do not merge: one
/// that overlaps the next row or nearly touches it (within 15 % of its height)
/// touches it. A row of only the laptop screen stays as it is.
pub fn tidy(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    center: (Vec3, f32),
    anchor: Option<u8>,
) -> HashMap<u8, Placement> {
    let (eye, yaw) = center;
    let turn = Quat::from_rotation_y(yaw);
    let local = |p: &Placement| turn.inverse() * (p.position - eye);
    let rows = rows_of(&seen(placements, monitors, center, Preset::Arc));
    let mut out: HashMap<u8, Placement> = HashMap::new();
    // Each row on its circle, in the arrangement's own frame (eye at 0, facing -Z).
    // (id, placement, height, the circle its sides are on)
    let mut laid: Vec<Vec<(u8, Placement, f32, f32)>> = Vec::new();
    for row in &rows {
        if row.iter().all(|m| m.id == MIRROR_ID) {
            laid.push(Vec::new());
            continue;
        }
        let first = row
            .iter()
            .position(|m| Some(m.id) == anchor)
            .unwrap_or_else(|| {
                (0..row.len())
                    .min_by(|&a, &b| {
                        let off = |m: &Seen| m.yaw.abs() + m.elevation.abs();
                        off(&row[a]).total_cmp(&off(&row[b]))
                    })
                    .unwrap_or(0)
            });
        let width = |m: &Seen| placements[&m.id].width;
        let height = |m: &Seen| width(m) * m.px.1 as f32 / m.px.0.max(1) as f32;
        let a = local(&placements[&row[first].id]);
        let flat = Vec2::new(a.x, a.z).length();
        // Its circle, wider when the row does not fit the arc on it.
        let widths: Vec<f32> = row.iter().map(width).collect();
        let radius = (flat * flat + width(&row[first]).powi(2) / 4.0)
            .sqrt()
            .max(super::arc_radius(&widths));
        let middle = a.y;
        let mut yaws = vec![0.0; row.len()];
        yaws[first] = a.x.atan2(-a.z);
        for i in first + 1..row.len() {
            yaws[i] = yaws[i - 1]
                + (angular_width(width(&row[i - 1]), radius)
                    + angular_width(width(&row[i]), radius))
                    / 2.0;
        }
        for i in (0..first).rev() {
            yaws[i] = yaws[i + 1]
                - (angular_width(width(&row[i + 1]), radius)
                    + angular_width(width(&row[i]), radius))
                    / 2.0;
        }
        laid.push(
            row.iter()
                .zip(yaws)
                .map(|(m, yaw)| {
                    let p = on_cylinder(yaw, middle, width(m), radius);
                    (m.id, p, height(m), radius)
                })
                .collect(),
        );
    }
    // Rows touch the row next to them, outwards from the anchor's.
    let main = rows
        .iter()
        .position(|r| r.iter().any(|m| Some(m.id) == anchor))
        .unwrap_or(0);
    // As seen at the monitors' sides (see `seen`).
    let elevations = |row: &[(u8, Placement, f32, f32)]| {
        let at = |m: &(u8, Placement, f32, f32), dy: f32| (m.1.position.y + dy).atan2(m.3);
        let low = row
            .iter()
            .map(|m| at(m, -m.2 / 2.0))
            .fold(f32::MAX, f32::min);
        let high = row
            .iter()
            .map(|m| at(m, m.2 / 2.0))
            .fold(f32::MIN, f32::max);
        (low, high)
    };
    let settle = |r: usize, next_to: usize, laid: &mut Vec<Vec<(u8, Placement, f32, f32)>>| {
        if laid[r].is_empty() || laid[next_to].is_empty() {
            return;
        }
        let (low, high) = elevations(&laid[r]);
        let (n_low, n_high) = elevations(&laid[next_to]);
        let above = r < next_to;
        let (gap, edge) = if above {
            (low - n_high, n_high)
        } else {
            (n_low - high, n_low)
        };
        if gap >= 0.15 * (high - low) {
            return;
        }
        // Moved up or down so its near edge is seen on the other row's.
        for (_, p, h, radius) in &mut laid[r] {
            let near = if above { -*h / 2.0 } else { *h / 2.0 };
            p.position.y = *radius * edge.tan() - near;
            p.rotation = facing(p.position, Vec3::new(0.0, p.position.y, 0.0));
        }
    };
    for r in (0..main).rev() {
        settle(r, r + 1, &mut laid);
    }
    for r in main + 1..laid.len() {
        settle(r, r - 1, &mut laid);
    }
    for (id, p, _, _) in laid.into_iter().flatten() {
        out.insert(
            id,
            Placement {
                position: eye + turn * p.position,
                rotation: (turn * p.rotation).normalize(),
                width: p.width,
            },
        );
    }
    // The laptop screen alone in its row, and anything not in a row, as they were.
    for (id, p) in placements {
        out.entry(*id).or_insert(*p);
    }
    out
}

/// Where a new monitor `new` (id, pixel size) goes among `monitors`, the others
/// staying exactly where they are: at an end of a row (joined to its neighbour's edge,
/// on its surface), over a monitor of the top row or under one of the bottom row, or
/// in a row of its own over everything. Places that cover another monitor or take a
/// row past the arc are left out; of the rest, the one nearest the gaze (`gaze`, a
/// direction). `None` when there is nothing to place it by.
pub fn place_new(
    placements: &HashMap<u8, Placement>,
    monitors: &[(u8, u32, u32)],
    new: (u8, u32, u32),
    center: (Vec3, f32),
    preset: Preset,
    gaze: Vec3,
) -> Option<Placement> {
    let (id, w_px, h_px) = new;
    let others: Vec<(u8, u32, u32)> = monitors
        .iter()
        .copied()
        .filter(|m| m.0 != id && placements.contains_key(&m.0))
        .collect();
    let seen_now = seen(placements, &others, center, preset);
    let rows = rows_of(&seen_now);
    // The new one as wide as its neighbours' pixels are.
    let density = others
        .iter()
        .map(|m| placements[&m.0].width / m.1.max(1) as f32)
        .sum::<f32>()
        / others.len().max(1) as f32;
    if others.is_empty() {
        return None;
    }
    let (mw, mh) = (w_px as f32 * density, h_px as f32 * density);
    let eye = center.0;
    let height = |id: u8| {
        let m = others
            .iter()
            .find(|m| m.0 == id)
            .copied()
            .unwrap_or((id, 1, 1));
        placements[&id].width * m.2 as f32 / m.1.max(1) as f32
    };
    let unturn = view(center, preset);
    let turn = unturn.inverse();
    let next_to = |n: u8, across: bool, side: f32| -> Placement {
        let p = &placements[&n];
        match preset {
            Preset::Wall => {
                let shift = if across {
                    Vec3::X * side * (p.width + mw) / 2.0
                } else {
                    Vec3::Y * side * (height(n) + mh) / 2.0
                };
                Placement {
                    position: p.position + p.rotation * shift,
                    rotation: p.rotation,
                    width: mw,
                }
            }
            // Beside it: joined to its edge, turned to face the eyes.
            Preset::Arc | Preset::Overhead if across => {
                let b = Beside {
                    gap: 0.0,
                    across,
                    side,
                    along: 0.0,
                };
                hinge(p, height(n), mw, mh, &b, eye)
            }
            // Over or under it: upright on its circle, as a preset puts rows.
            Preset::Arc | Preset::Overhead => {
                let local = unturn * (p.position - eye);
                let flat = Vec2::new(local.x, local.z).length();
                let radius = (flat * flat + p.width * p.width / 4.0)
                    .sqrt()
                    .max(mw / 2.0 + 0.01);
                let y = local.y + side * (height(n) + mh) / 2.0;
                let at = on_cylinder(local.x.atan2(-local.z), y, mw, radius);
                Placement {
                    position: eye + turn * at.position,
                    rotation: (turn * at.rotation).normalize(),
                    width: mw,
                }
            }
        }
    };
    // (place, the row it joins)
    let mut candidates: Vec<(Placement, Option<usize>)> = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        if let (Some(first), Some(last)) = (row.first(), row.last()) {
            candidates.push((next_to(first.id, true, -1.0), Some(r)));
            candidates.push((next_to(last.id, true, 1.0), Some(r)));
        }
    }
    // Over the top row, under the bottom one.
    for m in rows.first().into_iter().flatten() {
        candidates.push((next_to(m.id, false, 1.0), None));
    }
    for m in rows.last().into_iter().flatten() {
        candidates.push((next_to(m.id, false, -1.0), None));
    }
    // A row of its own over everything, where the gaze is: never covers anything.
    if preset != Preset::Wall {
        let top = seen_now
            .iter()
            .map(|m| m.elevation + m.half_height)
            .fold(f32::MIN, f32::max);
        let flat_gaze = unturn * gaze;
        let yaw = flat_gaze.x.atan2(-flat_gaze.z);
        let radius = seen_now
            .iter()
            .map(|m| {
                let p = unturn * (placements[&m.id].position - eye);
                Vec2::new(p.x, p.z).length()
            })
            .fold(0.0, f32::max)
            .max(mw / 2.0 + 0.01);
        // Its bottom edge seen just over the highest top (its sides on the circle).
        let local = on_cylinder(yaw, radius * top.tan() + mh / 2.0 + 0.001, mw, radius);
        candidates.push((
            Placement {
                position: eye + turn * local.position,
                rotation: (turn * local.rotation).normalize(),
                width: mw,
            },
            None,
        ));
    }
    let fits = |(p, row): &(Placement, Option<usize>)| {
        let mut with = placements.clone();
        with.insert(id, *p);
        let mut all = others.clone();
        all.push(new);
        let seen_all = seen(&with, &all, center, preset);
        let Some(me) = seen_all.iter().find(|m| m.id == id) else {
            return false;
        };
        if seen_all.iter().any(|o| o.id != id && o.overlaps(me)) {
            return false;
        }
        // Not past the arc (on a wall, its width).
        row.is_none_or(|r| {
            let ids: Vec<u8> = rows[r].iter().map(|m| m.id).chain([id]).collect();
            match preset {
                Preset::Wall => ids.iter().map(|i| with[i].width).sum::<f32>() <= wall_width(),
                Preset::Arc | Preset::Overhead => {
                    seen_all
                        .iter()
                        .filter(|m| ids.contains(&m.id))
                        .map(|m| 2.0 * m.half_width)
                        .sum::<f32>()
                        <= MAX_ARC
                }
            }
        })
    };
    candidates
        .into_iter()
        .filter(fits)
        .map(|(p, _)| p)
        .min_by(|a, b| {
            (a.position - eye)
                .angle_between(gaze)
                .total_cmp(&(b.position - eye).angle_between(gaze))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{METRES_PER_PIXEL, RADIUS, arrange};

    const SIZES: [(u32, u32); 6] = [
        (1920, 1080),
        (2560, 1440),
        (3440, 1440),
        (5120, 1440),
        (3840, 2160),
        (1440, 2560),
    ];
    const CENTER: (Vec3, f32) = (Vec3::ZERO, 0.0);

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

        fn below(&mut self, n: usize) -> usize {
            self.next() as usize % n
        }

        /// `-1.0..1.0`.
        fn unit(&mut self) -> f32 {
            self.next() as f32 / (1u64 << 30) as f32 - 1.0
        }
    }

    fn monitors(rng: &mut Rng, n: usize) -> Vec<(u8, u32, u32)> {
        (0..n)
            .map(|i| {
                let (w, h) = SIZES[rng.below(SIZES.len())];
                (i as u8, w, h)
            })
            .collect()
    }

    fn row_ids(placements: &HashMap<u8, Placement>, m: &[(u8, u32, u32)]) -> Vec<Vec<u8>> {
        rows_of(&seen(placements, m, CENTER, Preset::Arc))
            .iter()
            .map(|r| r.iter().map(|s| s.id).collect())
            .collect()
    }

    fn overlapping(placements: &HashMap<u8, Placement>, m: &[(u8, u32, u32)]) -> Option<(u8, u8)> {
        let all = seen(placements, m, CENTER, Preset::Arc);
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                if a.overlaps(b) {
                    return Some((a.id, b.id));
                }
            }
        }
        None
    }

    /// An arrangement moved about by hand: every monitor a little off where the preset
    /// put it, nearer or further, higher or lower.
    fn by_hand(rng: &mut Rng, m: &[(u8, u32, u32)]) -> HashMap<u8, Placement> {
        let mut p = arrange(Preset::Arc, m, Some(0), None);
        for place in p.values_mut() {
            let turn = Quat::from_rotation_y(0.05 * rng.unit());
            place.position =
                turn * place.position * (1.0 + 0.1 * rng.unit()) + Vec3::Y * 0.05 * rng.unit();
            place.rotation = facing(place.position, Vec3::new(0.0, place.position.y, 0.0));
        }
        p
    }

    /// The desktop the PC makes of `placements` (as the map shows it): every two
    /// monitors that touch in the headset, side by side or one over the other, touch on
    /// the desktop the same way. Across goes by the angles they are seen at; up and
    /// down by their edges' heights in the room, since every monitor has the same size
    /// of a pixel (seen at an angle, a wide monitor's middle is nearer, and looks taller).
    fn desktop_matches_view(
        placements: &HashMap<u8, Placement>,
        m: &[(u8, u32, u32)],
        primary: Option<u8>,
    ) -> Result<(), String> {
        let all = seen(placements, m, CENTER, Preset::Arc);
        let spots = desktop_spots(&all, primary);
        let rows = rows_of(&all);
        let row_of = |id: u8| rows.iter().position(|r| r.iter().any(|s| s.id == id));
        // Left, top, right, bottom on the desktop (down positive).
        let rect = |id: u8| {
            let [x, y, hw, hh] = spots[&id];
            (x - hw, -y - hh, x + hw, -y + hh)
        };
        // Bottom and top edges in the room.
        let heights = |s: &Seen| {
            let p = &placements[&s.id];
            let h = p.width * s.px.1 as f32 / s.px.0.max(1) as f32;
            let up = (p.rotation * Vec3::Y).y * h / 2.0;
            (p.position.y - up.abs(), p.position.y + up.abs())
        };
        let share = |a0: f32, a1: f32, b0: f32, b1: f32, len: f32| (a1.min(b1) - a0.max(b0)) / len;
        for a in &all {
            for b in &all {
                if a.id == b.id {
                    continue;
                }
                let ((a_low, _), (_, b_high)) = (heights(a), heights(b));
                let (ra, rb) = (rect(a.id), rect(b.id));
                // `b` right of `a`, touching, in one row.
                if ((b.yaw - b.half_width) - (a.yaw + a.half_width)).abs() < 0.01
                    && row_of(a.id) == row_of(b.id)
                    && (ra.2 - rb.0).abs() > 0.5
                {
                    return Err(format!(
                        "{} right of {} in the headset, not on the desktop: {spots:?}",
                        b.id, a.id
                    ));
                }
                let across = share(
                    a.yaw - a.half_width,
                    a.yaw + a.half_width,
                    b.yaw - b.half_width,
                    b.yaw + b.half_width,
                    2.0 * a.half_width.min(b.half_width),
                );
                // `a` over `b`, touching, in rows next to each other.
                let next = matches!((row_of(a.id), row_of(b.id)), (Some(x), Some(y)) if x + 1 == y);
                if (a_low - b_high).abs() < 0.01
                    && across > 0.5
                    && next
                    && (ra.3 - rb.1).abs() > 0.5
                {
                    return Err(format!(
                        "{} over {} in the headset, not on the desktop: {spots:?}",
                        a.id, b.id
                    ));
                }
            }
        }
        Ok(())
    }

    /// "Line up" keeps the rows, never lets monitors overlap, and lined up once it
    /// stays lined up.
    #[test]
    fn line_up_keeps_rows_and_is_stable() {
        let mut rng = Rng(7);
        for case in 0..400 {
            let n = 1 + rng.below(8);
            let m = monitors(&mut rng, n);
            let p = by_hand(&mut rng, &m);
            let t = tidy(&p, &m, CENTER, Some(0));
            assert_eq!(
                row_ids(&t, &m),
                row_ids(&p, &m),
                "case {case}: rows merged or split"
            );
            assert_eq!(overlapping(&t, &m), None, "case {case}: {m:?}");
            let again = tidy(&t, &m, CENTER, Some(0));
            for (id, a) in &t {
                assert!(
                    a.position.distance(again[id].position) < 1e-3,
                    "case {case}: {id} moved"
                );
            }
            // The anchor stays where it is seen (further away only when its row needs it).
            let (a, b) = (p[&0].position, t[&0].position);
            assert!(
                (a.x.atan2(-a.z) - b.x.atan2(-b.z)).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4,
                "case {case}: the anchor moved"
            );
            desktop_matches_view(&t, &m, Some(0)).unwrap_or_else(|e| panic!("case {case}: {e}"));
        }
    }

    /// A new monitor goes where it covers nothing, and the others stay put; the preset
    /// and the desktop agree on who touches whom.
    #[test]
    fn a_new_monitor_moves_nobody() {
        let mut rng = Rng(11);
        for case in 0..400 {
            let n = 2 + rng.below(7);
            let m = monitors(&mut rng, n);
            let mut p = arrange(Preset::Arc, &m[..n - 1], Some(0), None);
            desktop_matches_view(&p, &m[..n - 1], Some(0))
                .unwrap_or_else(|e| panic!("case {case}: {e}"));
            let gaze = Quat::from_rotation_y(0.5 * rng.unit()) * -Vec3::Z;
            let Some(new) = place_new(&p, &m, m[n - 1], CENTER, Preset::Arc, gaze) else {
                panic!("case {case}: nowhere for {:?} among {m:?}", m[n - 1]);
            };
            p.insert(m[n - 1].0, new);
            assert_eq!(overlapping(&p, &m), None, "case {case}: {m:?}");
            desktop_matches_view(&p, &m, Some(0)).unwrap_or_else(|e| panic!("case {case}: {e}"));
        }
    }

    /// A new monitor beside three: at the end of the row nearer the gaze, joined on.
    #[test]
    fn a_new_monitor_joins_the_row_end_nearer_the_gaze() {
        let m = [
            (0, 2560, 1440),
            (1, 2560, 1440),
            (2, 2560, 1440),
            (3, 1920, 1080),
        ];
        let p = arrange(Preset::Arc, &m[..2], Some(0), None);
        // Looking at where the right end would be.
        let right = Quat::from_rotation_y(-1.5) * -Vec3::Z;
        let new = place_new(&p, &m[..3], m[2], CENTER, Preset::Arc, right).unwrap();
        assert!(new.position.x > p[&1].position.x, "on the right: {new:?}");
        let mut with = p.clone();
        with.insert(2, new);
        assert_eq!(row_ids(&with, &m[..3]), vec![vec![0, 1, 2]]);
    }

    /// The six monitors as the headset had them on 2026-11-19 (one row on a 1.1 m
    /// circle, overlapping): the desktop the PC makes of them has no staircase (the
    /// old one spread over -1720..7600 with neighbours meeting at corners).
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
        assert!(overlapping(&p, &m).is_some(), "they did overlap");
        // Those over one another as seen go to two rows; each row lies on one line,
        // side by side, the second one stacked on the first: no staircase.
        let all = seen(&p, &m, CENTER, Preset::Arc);
        let rows = rows_of(&all);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let spots = desktop_spots(&all, Some(2));
        for row in &rows {
            let mut xs: Vec<[f32; 4]> = row.iter().map(|s| spots[&s.id]).collect();
            xs.sort_by(|a, b| a[0].total_cmp(&b[0]));
            let tops: Vec<f32> = xs.iter().map(|s| s[1] + s[3]).collect();
            let bottoms: Vec<f32> = xs.iter().map(|s| s[1] - s[3]).collect();
            let line = |v: &[f32]| v.iter().all(|x| (x - v[0]).abs() <= 1.0);
            let middles: Vec<f32> = xs.iter().map(|s| s[1]).collect();
            assert!(
                line(&tops) || line(&bottoms) || line(&middles),
                "one line: {spots:?}"
            );
            for w in xs.windows(2) {
                assert!(
                    ((w[0][0] + w[0][2]) - (w[1][0] - w[1][2])).abs() <= 1.0,
                    "{spots:?}"
                );
            }
        }
        // The second row stacked on the first: touching it.
        let touch = rows[0].iter().any(|a| {
            rows[1].iter().any(|b| {
                let (a, b) = (spots[&a.id], spots[&b.id]);
                (a[1] - a[3] - (b[1] + b[3])).abs() <= 1.0
                    && (a[0] - b[0]).abs() < a[2] + b[2] - 200.0
            })
        });
        assert!(touch, "the rows touch: {spots:?}");
        // Laid out again by the preset now: two rows, nothing overlapping.
        let fresh = arrange(Preset::Arc, &m, Some(2), None);
        assert_eq!(overlapping(&fresh, &m), None);
        assert_eq!(row_ids(&fresh, &m).len(), 2);
        desktop_matches_view(&fresh, &m, Some(2)).unwrap();
    }

    /// Two rows made by hand live through everything that happens to them: a monitor
    /// added and removed, the headset app restarted (layout.txt), "Line up", a profile
    /// and back. Every time the same rows, and the same desktop for the PC.
    #[test]
    fn two_rows_survive_everything() {
        let m = [
            (0, 2560, 1440),
            (1, 2560, 1440),
            (2, 1920, 1080),
            (3, 1920, 1080),
            (4, 1440, 2560),
        ];
        let four = &m[..4];
        // Two side by side, two more put over them.
        let mut p = arrange(Preset::Arc, &m[..2], Some(0), None);
        let up = Vec3::new(0.0, 1.0, -1.0).normalize();
        let new = place_new(&p, &m[..3], m[2], CENTER, Preset::Arc, up).unwrap();
        p.insert(2, new);
        // Looking right of that one.
        let right = Quat::from_rotation_y(-0.7) * p[&2].position.normalize();
        let new = place_new(&p, four, m[3], CENTER, Preset::Arc, right).unwrap();
        p.insert(3, new);
        let p = tidy(&p, four, CENTER, Some(0));
        let rows = row_ids(&p, four);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let desk = desktop_spots(&seen(&p, four, CENTER, Preset::Arc), Some(0));
        let same = |q: &HashMap<u8, Placement>, what: &str| {
            assert_eq!(row_ids(q, four), rows, "{what}: rows");
            assert_eq!(
                desktop_spots(&seen(q, four, CENTER, Preset::Arc), Some(0)),
                desk,
                "{what}: desktop"
            );
        };
        // A monitor added: the others stay, and so do their rows.
        let mut with = p.clone();
        let new = place_new(&p, &m, m[4], CENTER, Preset::Arc, -Vec3::Z).unwrap();
        with.insert(4, new);
        for (id, place) in &p {
            assert_eq!(with[id], *place, "{id} moved");
        }
        let five = seen(&with, &m, CENTER, Preset::Arc);
        let kept: Vec<Vec<u8>> = rows_of(&five)
            .iter()
            .map(|r| {
                r.iter()
                    .map(|s| s.id)
                    .filter(|id| *id != 4)
                    .collect::<Vec<_>>()
            })
            .filter(|r| !r.is_empty())
            .collect();
        assert_eq!(kept, rows, "a new monitor joins, nobody leaves a row");
        // And removed again.
        with.remove(&4);
        same(&with, "removed");
        // The headset app restarted.
        let dir = std::env::temp_dir().join(format!("prostir-two-rows-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let desk_file = crate::layout::Desk::new(dir.join("layout.txt"));
        desk_file.save(&p, &HashMap::new(), (CENTER, RADIUS), false);
        same(&desk_file.load().placements, "restarted");
        let _ = std::fs::remove_dir_all(&dir);
        // Lined up.
        same(&tidy(&p, four, CENTER, Some(0)), "lined up");
        // Saved in a profile, another profile, and back: the places by their order.
        let mut profile = crate::profiles::Profile::default_for(0);
        profile.sizes = four.iter().map(|x| (x.1, x.2)).collect();
        profile.places = four.iter().map(|x| p[&x.0]).collect();
        let text = {
            let path =
                std::env::temp_dir().join(format!("prostir-two-rows-p-{}", std::process::id()));
            let mut all = std::array::from_fn(crate::profiles::Profile::default_for);
            all[0] = profile;
            crate::profiles::save(&path, &all);
            let back = crate::profiles::load(&path);
            let _ = std::fs::remove_file(&path);
            back
        };
        let back: HashMap<u8, Placement> = four
            .iter()
            .map(|x| x.0)
            .zip(text[0].places.clone())
            .collect();
        same(&back, "profile and back");
    }
}
