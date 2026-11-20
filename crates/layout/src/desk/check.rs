//! What a plan is checked and moved by: how well it keeps what is seen, whether it
//! is what this planner would make, where it starts, and where the mouse crosses.

use super::*;

/// How well a plan keeps what `view` sees: the pairs seen touching (edges at most
/// [`TOUCH_GAP`] apart, sharing what the mouse needs) that the mouse crosses between on
/// the desktop on the same side, and how far, summed over them, each such pair stands
/// from where it is seen against the other at the middle of their seam (lined up within
/// noise first). More kept is better, then less misfit.
pub fn fit<K: Copy + Ord, V: View<K>>(plan: &[Spot<K>], view: &V) -> (usize, i64) {
    let (mut kept, mut misfit) = (0usize, 0i64);
    for a in plan {
        for b in plan {
            let Some(side) = a.crossing(b) else { continue };
            // Each pair once: `b` right of or above `a`.
            if !matches!(side, Side::Right | Side::Above) {
                continue;
            }
            let Some(rel) = view.relate(a.id, b.id, side) else {
                continue;
            };
            if rel.apart > TOUCH_GAP || rel.seam <= 0.0 {
                continue;
            }
            let (actual, seen) = match side {
                Side::Right => (b.y - a.y, side_offset(Some(rel), a.h, b.h, Lining::Middle)),
                _ => {
                    let lined = [(a.w - b.w).div_euclid(2), 0, a.w - b.w];
                    (b.x - a.x, snap(rel.offset, &lined, noise(a.w.min(b.w))))
                }
            };
            kept += 1;
            misfit += i64::from((actual - seen).abs());
        }
    }
    (kept, misfit)
}

/// The headset's hysteresis: `previous` while it is as good a plan of what `view` sees as
/// `fresh`, give or take a hand's tremor: the same outputs at the same sizes, nothing
/// overlapping, as many pairs seen touching kept, and its misfit (see [`fit`]) no more
/// than [`DEADBAND`] of the shorter edge per pair kept above `fresh`'s. Else `fresh`. So
/// a nudge sends nothing to the PC and the PC's desktop does not move; a change the
/// user makes on purpose (a monitor moved to another place, another contact) does.
pub fn settled<K: Copy + Ord, V: View<K>>(
    previous: &[Spot<K>],
    fresh: &[Spot<K>],
    view: &V,
) -> Vec<Spot<K>> {
    let sorted = |p: &[Spot<K>]| {
        let mut v = p.to_vec();
        v.sort_by_key(|s| s.id);
        v
    };
    let (old, new) = (sorted(previous), sorted(fresh));
    let same_outputs = old.len() == new.len()
        && old
            .iter()
            .zip(&new)
            .all(|(a, b)| a.id == b.id && a.w == b.w && a.h == b.h);
    let whole = old
        .iter()
        .enumerate()
        .all(|(i, a)| old[i + 1..].iter().all(|b| !a.overlaps(b)));
    if !same_outputs || !whole || old.is_empty() {
        return fresh.to_vec();
    }
    let ((kept_old, misfit_old), (kept_new, misfit_new)) = (fit(&old, view), fit(&new, view));
    let allowance: i64 = {
        let edge = old.iter().map(|s| s.w.min(s.h)).min().unwrap_or(0);
        (DEADBAND * f64::from(edge)).ceil() as i64 * kept_old.max(1) as i64
    };
    if kept_old >= kept_new && misfit_old <= misfit_new + allowance {
        previous.to_vec()
    } else {
        fresh.to_vec()
    }
}

/// Where the compositor wants the desktop's origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin<K> {
    /// The top-left corner of everything at `0,0` (KWin, Mutter, Hyprland).
    TopLeft,
    /// This output's top-left corner at `0,0` (Windows: the primary display).
    At(K),
}

/// The plan moved so its origin is where the compositor wants it.
pub fn rebase<K: Copy + PartialEq>(plan: &[Spot<K>], origin: Origin<K>) -> Vec<Spot<K>> {
    let (dx, dy) = match origin {
        Origin::TopLeft => (
            plan.iter().map(|p| p.x).min().unwrap_or(0),
            plan.iter().map(|p| p.y).min().unwrap_or(0),
        ),
        Origin::At(id) => plan
            .iter()
            .find(|p| p.id == id)
            .map_or((0, 0), |p| (p.x, p.y)),
    };
    plan.iter()
        .map(|p| Spot {
            x: p.x - dx,
            y: p.y - dy,
            ..*p
        })
        .collect()
}

/// Every pair the mouse crosses between, with the side the second is on.
pub fn crossings<K: Copy>(plan: &[Spot<K>]) -> Vec<(K, K, Side)> {
    let mut out = Vec::new();
    for (i, a) in plan.iter().enumerate() {
        for b in &plan[i + 1..] {
            if let Some(side) = a.crossing(b) {
                out.push((a.id, b.id, side));
            }
        }
    }
    out
}
