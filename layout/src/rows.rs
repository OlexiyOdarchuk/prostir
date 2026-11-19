//! Rows from what the eye sees: which monitors make up one row, and the rows from top
//! to bottom, each left to right.
//!
//! A row is a chain of side neighbours. Two monitors are neighbours when, as seen, the
//! second is the nearest to the right of the first whose left edge shares at least half
//! of the shorter edge with the first one's right edge ([`JOIN`]), and the first is
//! the nearest such to the left of the second. Measured where the edges face each
//! other, not by where the monitors are seen overall: on a wall above eye level a far
//! monitor is seen lower than a near one, yet the two meet along a whole edge.
//!
//! Each monitor has at most one neighbour on each side, so rows are chains: a monitor
//! between two rows joins one of them, and two rows never become one because of it.
//! A pair that were neighbours stay so while they share a quarter ([`KEEP`]): between
//! the two thresholds nothing changes, so a hand's tremor never moves a monitor from
//! one row to another.

use crate::desk::{TOUCH_GAP, View};
use crate::space::Side;

/// Two monitors become neighbours in a row when their facing edges share at least this
/// much of the shorter one as seen.
pub const JOIN: f64 = 0.5;
/// Neighbours stay neighbours while they share at least this much.
pub const KEEP: f64 = 0.25;

/// Rows top to bottom, each left to right, of the monitors `ids` as `view` sees them.
/// `previous`: the rows last worked out (with monitors that may have gone or not be
/// there yet); a pair of neighbours there stays a pair while they share [`KEEP`].
///
/// Deterministic, and the same whatever order `ids` come in. O(n²) relations.
pub fn rows_of<K: Copy + Ord, V: View<K>>(
    ids: &[K],
    view: &V,
    previous: Option<&[Vec<K>]>,
) -> Vec<Vec<K>> {
    let mut all: Vec<K> = ids.to_vec();
    all.sort();
    all.dedup();
    let n = all.len();
    let was_pair = |a: K, b: K| {
        previous.is_some_and(|rows| {
            rows.iter()
                .any(|r| r.windows(2).any(|w| w[0] == a && w[1] == b))
        })
    };
    // Who may be whose neighbour on that side: sharing enough as seen. With how far
    // apart they are (a fraction of the first one's width; negative when they overlap).
    // Only a pair apart across: one seen more over the other than beside it is not a
    // pair of neighbours in a row (a laptop screen under its monitor).
    let fits = |a: K, b: K, side: Side| -> Option<(f64, f64)> {
        let rel = view.relate(a, b, side)?;
        let (l, r) = if side == Side::Right { (a, b) } else { (b, a) };
        let least = if was_pair(l, r) { KEEP } else { JOIN };
        let facing = |side: Side| view.relate(a, b, side).filter(|r| r.shared > 0.0);
        let up = facing(Side::Above).or_else(|| facing(Side::Below));
        let across = up.is_none_or(|u| rel.apart >= u.apart);
        (rel.seam >= least && across).then_some((rel.apart.max(0.0), rel.shared))
    };
    // The nearest such on each side, overlapping counting as touching; of those within
    // the touching distance of the nearest, the one sharing the longest edge (then the
    // smaller key), which a hand's tremor does not change. A neighbour it had stays its
    // neighbour while it fits, unless another is nearer by more than the touching
    // distance.
    let nearest = |a: K, side: Side| -> Option<usize> {
        let pair = |j: usize| {
            let (l, r) = if side == Side::Right {
                (a, all[j])
            } else {
                (all[j], a)
            };
            was_pair(l, r)
        };
        let fitting: Vec<(usize, (f64, f64))> = (0..n)
            .filter(|&j| all[j] != a)
            .filter_map(|j| fits(a, all[j], side).map(|f| (j, f)))
            .collect();
        let least = fitting.iter().map(|f| f.1.0).fold(f64::INFINITY, f64::min);
        let best = fitting
            .iter()
            .filter(|f| f.1.0 <= least + TOUCH_GAP)
            .min_by(|(j1, f1), (j2, f2)| f2.1.total_cmp(&f1.1).then(all[*j1].cmp(&all[*j2])));
        let kept = fitting.iter().find(|(j, _)| pair(*j));
        match (best, kept) {
            (Some(_), Some(k)) if k.1.0 <= least + TOUCH_GAP => Some(k.0),
            (b, _) => b.map(|b| b.0),
        }
    };
    let right: Vec<Option<usize>> = (0..n).map(|i| nearest(all[i], Side::Right)).collect();
    let left: Vec<Option<usize>> = (0..n).map(|i| nearest(all[i], Side::Left)).collect();
    // Linked when each is the other's nearest.
    let next: Vec<Option<usize>> = (0..n)
        .map(|i| right[i].filter(|&j| left[j] == Some(i)))
        .collect();
    let has_left: Vec<bool> = (0..n).map(|j| next.contains(&Some(j))).collect();
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut taken = vec![false; n];
    let follow = |start: usize, taken: &mut Vec<bool>| {
        let mut row = Vec::new();
        let mut at = Some(start);
        while let Some(i) = at {
            if taken[i] {
                break;
            }
            taken[i] = true;
            row.push(i);
            at = next[i];
        }
        row
    };
    for start in 0..n {
        if !has_left[start] && !taken[start] {
            let row = follow(start, &mut taken);
            rows.push(row);
        }
    }
    // A ring of links (monitors all the way around): cut at its smallest key.
    for start in 0..n {
        if !taken[start] {
            let row = follow(start, &mut taken);
            rows.push(row);
        }
    }

    // Top to bottom: by how high the row is seen on average, then a row seen over
    // another (pairs of their monitors seen one over the other) goes above it.
    let middle_y = |row: &[usize]| {
        let ys: Vec<f64> = row
            .iter()
            .filter_map(|&i| view.middle(all[i]).map(|m| m.1))
            .collect();
        if ys.is_empty() {
            0.0
        } else {
            ys.iter().sum::<f64>() / ys.len() as f64
        }
    };
    let first = |row: &[usize]| row.iter().map(|&i| all[i]).min();
    rows.sort_by(|a, b| {
        middle_y(b)
            .total_cmp(&middle_y(a))
            .then(first(a).cmp(&first(b)))
    });
    let over = |upper: &[usize], lower: &[usize]| -> f64 {
        let mut votes = 0.0;
        for &u in upper {
            for &l in lower {
                if let Some(r) = view.relate(all[l], all[u], Side::Above) {
                    votes += r.seam;
                }
            }
        }
        votes
    };
    for _ in 0..rows.len() {
        let mut swapped = false;
        for k in 0..rows.len().saturating_sub(1) {
            if over(&rows[k + 1], &rows[k]) > over(&rows[k], &rows[k + 1]) + 1e-9 {
                rows.swap(k, k + 1);
                swapped = true;
            }
        }
        if !swapped {
            break;
        }
    }
    rows.into_iter()
        .map(|r| r.into_iter().map(|i| all[i]).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::{Boxes, SeenBox, Size};

    /// Boxes seen at `(x, y)` (y down), `w` by `h`.
    fn boxes(list: &[(u8, f64, f64, f64, f64)]) -> Boxes<u8> {
        Boxes::new(
            list.iter()
                .map(|&(id, x, y, w, h)| {
                    (
                        id,
                        SeenBox { x, y, w, h },
                        Size {
                            w: w as i32,
                            h: h as i32,
                        },
                    )
                })
                .collect(),
        )
    }

    fn ids(v: &Boxes<u8>) -> Vec<u8> {
        v.boxes.iter().map(|b| b.0).collect()
    }

    #[test]
    fn a_monitor_between_two_rows_joins_one() {
        // Two rows of two, touching; a fifth monitor halfway between them on the right.
        let v = boxes(&[
            (0, 0.0, 0.0, 100.0, 100.0),
            (1, 100.0, 0.0, 100.0, 100.0),
            (2, 0.0, -100.0, 100.0, 100.0),
            (3, 100.0, -100.0, 100.0, 100.0),
            (4, 200.0, -50.0, 100.0, 100.0),
        ]);
        let rows = rows_of(&ids(&v), &v, None);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(
            rows.contains(&vec![2, 3, 4]) || rows.contains(&vec![0, 1, 4]),
            "{rows:?}"
        );
        assert!(
            rows.contains(&vec![2, 3]) || rows.contains(&vec![0, 1]),
            "{rows:?}"
        );
    }

    #[test]
    fn a_tall_monitor_with_tops_lined_up_is_in_the_row() {
        let v = boxes(&[(0, 0.0, 27.0, 100.0, 54.0), (1, 100.0, 100.0, 100.0, 200.0)]);
        assert_eq!(rows_of(&ids(&v), &v, None), vec![vec![0, 1]]);
    }

    #[test]
    fn hysteresis_keeps_neighbours_between_keep_and_join() {
        // Sharing 40 %: two rows without history, one row with it.
        let v = boxes(&[(0, 0.0, 0.0, 100.0, 100.0), (1, 100.0, -60.0, 100.0, 100.0)]);
        assert_eq!(rows_of(&ids(&v), &v, None).len(), 2);
        assert_eq!(rows_of(&ids(&v), &v, Some(&[vec![0, 1]])), vec![vec![0, 1]]);
        // At 20 % they part, the higher one on top.
        let v = boxes(&[(0, 0.0, 0.0, 100.0, 100.0), (1, 100.0, -80.0, 100.0, 100.0)]);
        assert_eq!(
            rows_of(&ids(&v), &v, Some(&[vec![0, 1]])),
            vec![vec![1], vec![0]]
        );
    }

    #[test]
    fn a_row_closes_over_a_gap_but_takes_the_nearest() {
        let v = boxes(&[
            (0, 0.0, 0.0, 100.0, 100.0),
            (1, 400.0, 0.0, 100.0, 100.0),
            (2, 200.0, 10.0, 100.0, 100.0),
        ]);
        assert_eq!(rows_of(&ids(&v), &v, None), vec![vec![0, 2, 1]]);
    }
}
