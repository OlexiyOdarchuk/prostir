//! How a preset splits monitors into rows, and how far away they stand: the circle
//! grows before a second row starts.
//!
//! A monitor `w` metres wide takes `2 asin(w / 2R)` of a circle of radius `R` around
//! the head. A row fits while its monitors take at most [`MAX_ARC`]; the sum shrinks
//! as `R` grows, so a row that does not fit at [`RADIUS`] moves away until it does.
//! Past [`MAX_RADIUS`] text gets too small, and the monitors split into two rows,
//! then three, keeping their order, at the split that needs the smallest circle. All
//! rows stand on the one circle (the largest any of them needs), one above the other
//! like one curved wall. Only with more monitors than three rows hold at
//! [`MAX_RADIUS`] does the circle grow further: harder to read, but nothing overlaps.
//! A wall is the same, with the width of a row in place of its arc.

use std::ops::Range;

use super::RADIUS;
use crate::settings::Preset;

/// The most of the circle around the head one row takes (past this, behind the back).
pub const MAX_ARC: f32 = 210.0 * std::f32::consts::PI / 180.0;
/// The farthest the monitors stand before a second row starts.
pub const MAX_RADIUS: f32 = 1.5;
/// Most rows a preset makes; more monitors than that only move further away.
const MAX_ROWS: usize = 3;

/// The widest row of a wall: what spans 150 degrees seen from [`RADIUS`] away.
pub fn wall_width() -> f32 {
    2.0 * RADIUS * 75f32.to_radians().tan()
}

/// The angle a monitor `width` metres wide takes on a circle of `radius` around the
/// head (its edges on the circle).
pub fn angular_width(width: f32, radius: f32) -> f32 {
    2.0 * (width / 2.0 / radius).min(1.0).asin()
}

/// The smallest circle, not nearer than [`RADIUS`], on which monitors `widths` wide
/// take at most [`MAX_ARC`] side by side.
pub fn arc_radius(widths: &[f32]) -> f32 {
    let arc = |r: f32| widths.iter().map(|w| angular_width(*w, r)).sum::<f32>();
    let widest = widths.iter().copied().fold(0.0, f32::max);
    if widest / 2.0 <= RADIUS && arc(RADIUS) <= MAX_ARC {
        return RADIUS;
    }
    // x <= asin x <= pi x / 2 bounds the sum by sum / R and pi sum / 2R.
    let sum: f32 = widths.iter().sum();
    let mut lo = (sum / MAX_ARC).max(widest / 2.0);
    let mut hi = (std::f32::consts::FRAC_PI_2 * sum / MAX_ARC).max(widest / 2.0);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if arc(mid) <= MAX_ARC {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi.max(RADIUS)
}

/// Monitors split into rows: ranges of the order they were given in, which of them
/// holds the main monitor, and the circle they stand on.
#[derive(Debug, Clone, PartialEq)]
pub struct Rows {
    pub groups: Vec<Range<usize>>,
    pub main: usize,
    pub radius: f32,
}

/// How `preset` splits monitors `widths` wide (metres, in order) into rows; `primary`
/// is the index of the main monitor.
pub fn split(preset: Preset, widths: &[f32], primary: Option<usize>) -> Rows {
    // What a row needs, and whether that fits.
    let need = |r: &Range<usize>| match preset {
        Preset::Wall => widths[r.clone()].iter().sum(),
        Preset::Arc | Preset::Overhead => arc_radius(&widths[r.clone()]),
    };
    let fits = |n: f32| match preset {
        Preset::Wall => n <= wall_width(),
        Preset::Arc | Preset::Overhead => n <= MAX_RADIUS,
    };
    let worst = |groups: &[Range<usize>]| groups.iter().map(need).fold(0.0, f32::max);
    let mut best: Vec<Range<usize>> = std::iter::once(0..widths.len()).collect();
    for k in 2..=MAX_ROWS.min(widths.len()) {
        if fits(worst(&best)) {
            break;
        }
        best = splits(widths.len(), k)
            .min_by(|a, b| worst(a).total_cmp(&worst(b)))
            .unwrap_or(best);
    }
    let radius = match preset {
        Preset::Wall => RADIUS,
        Preset::Arc | Preset::Overhead => best
            .iter()
            .map(|r| arc_radius(&widths[r.clone()]))
            .fold(RADIUS, f32::max),
    };
    let main = primary
        .and_then(|p| best.iter().position(|r| r.contains(&p)))
        .unwrap_or(0);
    Rows {
        groups: best,
        main,
        radius,
    }
}

/// Every way to cut `n` monitors, in order, into `k` rows none of them empty.
fn splits(n: usize, k: usize) -> impl Iterator<Item = Vec<Range<usize>>> {
    let mut cuts: Option<Vec<usize>> = (k >= 1 && k <= n).then(|| (1..k).collect());
    std::iter::from_fn(move || {
        let current = cuts.clone()?;
        let bounds: Vec<usize> = std::iter::once(0)
            .chain(current.iter().copied())
            .chain(std::iter::once(n))
            .collect();
        // The next cuts: the last one that can move moves, the ones after it follow.
        cuts = (0..current.len())
            .rev()
            .find(|&i| current[i] < n - (current.len() - i))
            .map(|i| {
                let mut next = current.clone();
                next[i] += 1;
                for j in i + 1..next.len() {
                    next[j] = next[j - 1] + 1;
                }
                next
            });
        Some(bounds.windows(2).map(|b| b[0]..b[1]).collect())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::METRES_PER_PIXEL;

    fn widths(px: &[u32]) -> Vec<f32> {
        px.iter().map(|p| *p as f32 * METRES_PER_PIXEL).collect()
    }

    /// Three 2560 monitors stay as they were: one row, 208 degrees, 1.1 m.
    #[test]
    fn three_ordinary_monitors_stay_as_they_were() {
        let w = widths(&[2560, 2560, 2560]);
        let rows = split(Preset::Arc, &w, Some(1));
        assert_eq!(rows.groups, vec![0..3]);
        assert_eq!(rows.radius, RADIUS);
        let arc: f32 = w.iter().map(|w| angular_width(*w, RADIUS)).sum();
        assert!(
            (arc.to_degrees() - 208.0).abs() < 1.0,
            "{}",
            arc.to_degrees()
        );
    }

    /// The six monitors of 2026-11-19, in their order then: one row would need
    /// 2.29 m, so two rows of three, at about 1.35 m.
    #[test]
    fn six_big_monitors_make_two_rows() {
        let w = widths(&[3440, 2560, 1440, 3440, 2560, 3440]);
        assert!((arc_radius(&w) - 2.29).abs() < 0.02, "{}", arc_radius(&w));
        let rows = split(Preset::Arc, &w, Some(2));
        assert_eq!(rows.groups.len(), 2, "{rows:?}");
        assert_eq!(rows.groups, vec![0..3, 3..6]);
        assert!((rows.radius - 1.35).abs() < 0.02, "{rows:?}");
        assert_eq!(rows.main, 0);
    }

    /// Four 3440 (one row would need 1.9 m) and five 2560 (1.74 m): two rows each,
    /// near enough to stay at 1.1 m.
    #[test]
    fn four_wide_or_five_ordinary_make_two_rows() {
        for px in [&[3440; 4][..], &[2560; 5][..]] {
            let w = widths(px);
            assert!(arc_radius(&w) > MAX_RADIUS);
            let rows = split(Preset::Arc, &w, None);
            assert_eq!(rows.groups.len(), 2, "{rows:?}");
            assert_eq!(rows.radius, RADIUS, "{rows:?}");
        }
    }

    /// Every split once, in order.
    #[test]
    fn every_split_once() {
        let all: Vec<Vec<Range<usize>>> = splits(4, 3).collect();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], vec![0..1, 1..2, 2..4]);
        assert_eq!(splits(3, 1).count(), 1);
    }

    /// Whatever the monitors, each row takes at most the arc, and the circle stays
    /// within reach when three rows are enough.
    #[test]
    fn rows_fit_the_circle() {
        let sizes = [1920, 2560, 3440, 5120, 3840, 1440];
        let mut seed = 0x5eed_u64;
        for _ in 0..3000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let n = 1 + (seed >> 33) as usize % 14;
            let px: Vec<u32> = (0..n)
                .map(|i| sizes[(seed >> (i % 20 + 3)) as usize % sizes.len()])
                .collect();
            let w = widths(&px);
            let rows = split(Preset::Arc, &w, Some(0));
            assert!(rows.groups.len() <= MAX_ROWS);
            assert_eq!(rows.groups.iter().map(|r| r.len()).sum::<usize>(), n);
            for g in &rows.groups {
                let arc: f32 = w[g.clone()]
                    .iter()
                    .map(|w| angular_width(*w, rows.radius))
                    .sum();
                assert!(arc <= MAX_ARC + 1e-3, "{px:?}: {rows:?}");
            }
            // Within reach whenever some split into at most three rows is.
            let reachable = (1..=MAX_ROWS.min(n)).any(|k| {
                splits(n, k).any(|g| g.iter().all(|r| arc_radius(&w[r.clone()]) <= MAX_RADIUS))
            });
            if reachable {
                assert!(rows.radius <= MAX_RADIUS + 1e-3, "{px:?}: {rows:?}");
            }
        }
    }
}
