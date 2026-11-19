//! The PC desktop from the rows the headset shows the monitors in: where each output
//! goes, top-left corner in logical pixels (pixel size divided by the output's scale).
//!
//! One plan for every input (with or without the positions seen in the headset):
//!
//! 1. Every row lies on one line, side by side, so neighbours in it share the whole
//!    edge of the shorter one: no staircase from small differences in height. The
//!    row with the main monitor goes left to right by where each monitor is seen
//!    across (`center`), its line (tops, middles or bottoms lined up with the main
//!    monitor) the one closest to where they are seen (`middle`); middles without
//!    one.
//! 2. Every other row, outwards from that one, is one block side by side, bottoms
//!    lined up above the main row and tops below it. The monitor that overlaps the
//!    next row inwards most, as seen, keeps its offset from the one it overlaps; the
//!    block overlaps that row by at least [`REACH`] and touches the nearest monitor
//!    under (over) it. Where it would touch a sliver of an edge, it moves by less
//!    than [`REACH`] to where the mouse crosses every edge it shares, or at least
//!    one (a row of other widths can leave a sliver at a seam).
//! 3. The laptop screen goes under the main monitor unless the headset put it in a
//!    row; other real displays next to it, the same way; window panels in a column
//!    right of everything, from the top of what is rightmost.
//! 4. The whole plan starts at `0,0`, as KDE wants.
//!
//! Nothing overlaps, whatever the input.

use std::collections::BTreeSet;

/// Shared edge the mouse needs to cross from one monitor to the next (or all of a
/// shorter edge).
pub const REACH: i32 = 200;
/// Seen positions further out than this are nonsense (and would overflow).
pub const FAR: i32 = 100_000;

/// An output's size on the desktop, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

/// A monitor in a row and where its centre is seen, in pixels of one scale shared by
/// all monitors (see `PROTOCOL.md`, "Layout").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<K> {
    pub id: K,
    /// Across, left to right (`None`: just right of the previous one).
    pub center: Option<i32>,
    /// Down (`None`: not known, centred in the row).
    pub middle: Option<i32>,
}

impl<K> Entry<K> {
    /// A monitor whose place is not known.
    pub fn new(id: K) -> Self {
        Self {
            id,
            center: None,
            middle: None,
        }
    }
}

/// What goes on the desktop.
#[derive(Debug, Clone)]
pub struct Desk<K> {
    /// Rows from top to bottom, each left to right as the headset shows them. A
    /// monitor listed twice counts where it is listed first.
    pub rows: Vec<Vec<Entry<K>>>,
    /// The primary monitor; its row is laid out first.
    pub main: Option<K>,
    /// Goes under the main monitor unless it is in a row (the laptop screen).
    pub under_main: Option<K>,
    /// Other real displays: next to `under_main`, under everything over them.
    pub bottom: Vec<K>,
    /// Window panels, in a column right of everything.
    pub column: Vec<K>,
}

impl<K> Default for Desk<K> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            main: None,
            under_main: None,
            bottom: Vec::new(),
            column: Vec::new(),
        }
    }
}

/// A placed output: top-left corner and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spot<K> {
    pub id: K,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl<K> Spot<K> {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// Whether the two overlap across (their columns share a pixel).
    pub fn across<L>(&self, o: &Spot<L>) -> bool {
        self.x < o.right() && o.x < self.right()
    }

    /// Whether the two overlap up and down (their lines share a pixel).
    pub fn along<L>(&self, o: &Spot<L>) -> bool {
        self.y < o.bottom() && o.y < self.bottom()
    }

    /// Where the two touch: the length of the edge they share and how much of it the
    /// mouse needs to cross ([`reach`] of the two edges). `None` when they do not touch
    /// (a corner is not touching).
    pub fn touching<L>(&self, o: &Spot<L>) -> Option<(i32, i32)> {
        let span = |a0: i32, a1: i32, b0: i32, b1: i32| a1.min(b1) - a0.max(b0);
        let (shared, need) = if self.right() == o.x || o.right() == self.x {
            (
                span(self.y, self.bottom(), o.y, o.bottom()),
                reach(self.h, o.h),
            )
        } else if self.bottom() == o.y || o.bottom() == self.y {
            (
                span(self.x, self.right(), o.x, o.right()),
                reach(self.w, o.w),
            )
        } else {
            return None;
        };
        (shared > 0).then_some((shared, need))
    }

    /// Whether the mouse can cross where the two touch, if they do: not on a corner or
    /// a sliver of an edge.
    pub fn crossable<L>(&self, o: &Spot<L>) -> bool {
        self.touching(o).is_none_or(|(shared, need)| shared >= need)
    }
}

/// How much of an edge two outputs of these lengths share at least, so the mouse
/// crosses.
pub fn reach(a: i32, b: i32) -> i32 {
    REACH.min(a).min(b)
}

/// A monitor of a row with its seen centre worked out.
#[derive(Debug, Clone, Copy)]
struct Seat<K> {
    id: K,
    center: i32,
    middle: Option<i32>,
    size: Size,
}

impl<K> Seat<K> {
    /// Left edge as seen (`center` is `left + w / 2`).
    fn left(&self) -> i32 {
        self.center - self.size.w / 2
    }
}

/// Where each output goes (see the module's description). Outputs without a size are
/// left out (an output the desktop does not list yet).
pub fn plan<K: Copy + Ord>(desk: &Desk<K>, size: impl Fn(K) -> Option<Size>) -> Vec<Spot<K>> {
    let mut listed = BTreeSet::new();
    let rows: Vec<Vec<Seat<K>>> = desk
        .rows
        .iter()
        .map(|row| {
            let mut out: Vec<Seat<K>> = Vec::new();
            for e in row {
                if !listed.insert(e.id) {
                    continue;
                }
                let Some(size) = size(e.id).filter(|s| s.w > 0 && s.h > 0) else {
                    continue;
                };
                let center = match e.center {
                    Some(c) => c.clamp(-FAR, FAR),
                    // Right after the previous one.
                    None => out.last().map_or(0, |p| p.left() + p.size.w + size.w / 2),
                };
                out.push(Seat {
                    id: e.id,
                    center,
                    middle: e.middle.map(|m| m.clamp(-FAR, FAR)),
                    size,
                });
            }
            out.sort_by_key(|m| m.center);
            out
        })
        .filter(|r| !r.is_empty())
        .collect();
    let mut placed: Vec<Spot<K>> = Vec::new();
    if let Some(main_row) = rows
        .iter()
        .position(|r| r.iter().any(|m| Some(m.id) == desk.main))
        .or_else(|| (!rows.is_empty()).then_some(0))
    {
        placed.extend(main_row_spots(&rows[main_row], desk.main));
        for r in (0..main_row).rev() {
            place_row(&rows[r], &rows[r + 1], true, &mut placed);
        }
        for r in main_row + 1..rows.len() {
            place_row(&rows[r], &rows[r - 1], false, &mut placed);
        }
    }
    // Each output once: not in a row already, nor listed before.
    let mut extra = |id: K| {
        let size = size(id).filter(|s| s.w > 0 && s.h > 0);
        listed.insert(id).then_some(size).flatten().map(|s| (id, s))
    };
    // The laptop screen under the main monitor and other real displays right of it,
    // a block under whatever is over it.
    let main_spot = desk
        .main
        .and_then(|m| placed.iter().find(|p| p.id == m))
        .or_else(|| placed.first())
        .copied();
    let under_main = desk.under_main.and_then(&mut extra);
    let x = match (main_spot, under_main) {
        (Some(m), Some((_, s))) => m.x + (m.w - s.w) / 2,
        (Some(m), None) => m.x,
        (None, _) => 0,
    };
    let block = side_by_side(
        under_main
            .into_iter()
            .chain(desk.bottom.iter().filter_map(|&id| extra(id))),
        x,
    );
    let floor = placed.iter().map(Spot::bottom).max().unwrap_or(0);
    // Anywhere along the bottom: nothing says where they are seen.
    let spots = settle_block(&placed, &block, false, floor, (-FAR, FAR));
    placed.extend(spots);
    // Window panels in a column right of everything, from the top of what is
    // rightmost.
    let rightmost = placed.iter().max_by_key(|p| (p.right(), -p.y)).copied();
    let x = rightmost.map_or(0, |p| p.right());
    let mut y = rightmost.map_or(0, |p| p.y);
    for &id in &desk.column {
        if let Some((id, s)) = extra(id) {
            placed.push(Spot {
                id,
                x,
                y,
                w: s.w,
                h: s.h,
            });
            y += s.h;
        }
    }
    // KDE wants the desktop to start at 0,0.
    let min_x = placed.iter().map(|p| p.x).min().unwrap_or(0);
    let min_y = placed.iter().map(|p| p.y).min().unwrap_or(0);
    for p in &mut placed {
        p.x -= min_x;
        p.y -= min_y;
    }
    placed
}

/// The line a row lies on, against its main monitor.
#[derive(Debug, Clone, Copy)]
enum Line {
    Middles,
    Tops,
    Bottoms,
}

/// The main row: side by side from `x = 0`, on the line closest to where the
/// monitors are seen (see the module's description).
fn main_row_spots<K: Copy + Ord>(row: &[Seat<K>], main: Option<K>) -> Vec<Spot<K>> {
    let tallest = row.iter().map(|m| m.size.h).max().unwrap_or(0);
    let reference = row
        .iter()
        .find(|m| Some(m.id) == main)
        .or_else(|| row.iter().find(|m| m.size.h == tallest))
        .unwrap_or(&row[0]);
    let top = reference.middle.unwrap_or(0) - reference.size.h / 2;
    let y = |line: Line, h: i32| match line {
        Line::Middles => top + (reference.size.h - h) / 2,
        Line::Tops => top,
        Line::Bottoms => top + reference.size.h - h,
    };
    // Middles first: when lines fit as well, or heights are not known.
    let line = if row.iter().all(|m| m.middle.is_some()) {
        [Line::Middles, Line::Tops, Line::Bottoms]
            .into_iter()
            .min_by_key(|&line| {
                row.iter()
                    .filter_map(|m| {
                        let seen = m.middle? - m.size.h / 2;
                        Some(i64::from((seen - y(line, m.size.h)).abs()))
                    })
                    .sum::<i64>()
            })
            .unwrap_or(Line::Middles)
    } else {
        Line::Middles
    };
    let mut spots = side_by_side(row.iter().map(|m| (m.id, m.size)), 0);
    for s in &mut spots {
        s.y = y(line, s.h);
    }
    spots
}

/// A row above (`above`) or below the row `next_to`, already placed: one block side by
/// side, offset as seen, touching the nearest monitors under (or over) it.
fn place_row<K: Copy + Ord>(
    row: &[Seat<K>],
    next_to: &[Seat<K>],
    above: bool,
    placed: &mut Vec<Spot<K>>,
) {
    let neighbours: Vec<(Seat<K>, Spot<K>)> = next_to
        .iter()
        .filter_map(|n| Some((*n, *placed.iter().find(|p| p.id == n.id)?)))
        .collect();
    // The monitor that overlaps the next row most as seen keeps its offset from the
    // one it overlaps; without any overlap, the one nearest across.
    let overlap = |m: &Seat<K>, n: &Seat<K>| {
        (m.left() + m.size.w).min(n.left() + n.size.w) - m.left().max(n.left())
    };
    let Some((anchor, n, n_spot)) = row
        .iter()
        .enumerate()
        .flat_map(|(i, m)| neighbours.iter().map(move |(n, s)| (i, m, n, s)))
        .max_by_key(|(_, m, n, _)| {
            let o = overlap(m, n);
            if o > 0 {
                (1, o)
            } else {
                (0, -(m.center - n.center).abs())
            }
        })
        .map(|(i, _, n, s)| (i, *n, *s))
    else {
        return;
    };
    // Side by side from the anchor.
    let from = n_spot.x + (row[anchor].left() - n.left())
        - row[..anchor].iter().map(|m| m.size.w).sum::<i32>();
    let block = side_by_side(row.iter().map(|m| (m.id, m.size)), from);
    // Overlapping the next row by enough for the mouse.
    let (left, right) = (block[0].x, block[block.len() - 1].right());
    let lo = neighbours.iter().map(|(_, s)| s.x).min().unwrap_or(0);
    let hi = neighbours.iter().map(|(_, s)| s.right()).max().unwrap_or(0);
    let k = REACH.min(right - left).min(hi - lo);
    let shift = 0.clamp(lo + k - right, hi - k - left);
    let block: Vec<Spot<K>> = block
        .into_iter()
        .map(|b| Spot {
            x: b.x + shift,
            ..b
        })
        .collect();
    let edge = if above {
        neighbours.iter().map(|(_, s)| s.y).min()
    } else {
        neighbours.iter().map(|(_, s)| s.bottom()).max()
    }
    .unwrap_or(0);
    // Nudged by less than REACH, only as far as it still overlaps the next row enough.
    let nudge = (
        (lo + k - right - shift).max(1 - REACH),
        (hi - k - left - shift).min(REACH - 1),
    );
    let spots = settle_block(placed, &block, above, edge, nudge);
    placed.extend(spots);
}

/// Outputs side by side from `x`, heights still to be settled.
fn side_by_side<K>(outputs: impl IntoIterator<Item = (K, Size)>, mut x: i32) -> Vec<Spot<K>> {
    outputs
        .into_iter()
        .map(|(id, s)| {
            let spot = Spot {
                id,
                x,
                y: 0,
                w: s.w,
                h: s.h,
            };
            x += s.w;
            spot
        })
        .collect()
}

/// `block`, outputs side by side, over (`above`, bottoms lined up) or under (tops
/// lined up) what is `placed`, touching the nearest placed output under (over) it; at
/// `edge` when it is over nothing. Moved across within `nudge`, as little as can be,
/// to where the mouse crosses every edge it shares with what is placed (no corners,
/// no slivers), or else at least one.
fn settle_block<K: Copy>(
    placed: &[Spot<K>],
    block: &[Spot<K>],
    above: bool,
    edge: i32,
    nudge: (i32, i32),
) -> Vec<Spot<K>> {
    let at = |shift: i32| {
        let mut spots: Vec<Spot<K>> = block
            .iter()
            .map(|b| Spot {
                x: b.x + shift,
                ..*b
            })
            .collect();
        let across = placed.iter().filter(|p| spots.iter().any(|s| s.across(*p)));
        let line = if above {
            across.map(|p| p.y).min()
        } else {
            across.map(Spot::bottom).max()
        }
        .unwrap_or(edge);
        for s in &mut spots {
            s.y = if above { line - s.h } else { line };
        }
        spots
    };
    // Every edge it shares crossable; else at least one it touches, so it is reached.
    let crossable = |spots: &[Spot<K>]| spots.iter().all(|s| placed.iter().all(|p| s.crossable(p)));
    let reached = |spots: &[Spot<K>]| {
        spots.iter().any(|s| {
            placed
                .iter()
                .any(|p| s.touching(p).is_some_and(|(shared, need)| shared >= need))
        })
    };
    let mut shifts: Vec<i32> = vec![0];
    for b in block {
        for p in placed {
            let r = reach(b.w, p.w);
            shifts.extend([
                p.x - b.right(),
                p.right() - b.x,
                p.x + r - b.right(),
                p.right() - r - b.x,
            ]);
        }
    }
    shifts.retain(|s| (nudge.0..=nudge.1).contains(s));
    shifts.sort_by_key(|s| (s.abs(), *s));
    shifts.dedup();
    let tried: Vec<Vec<Spot<K>>> = shifts.into_iter().map(at).collect();
    tried
        .iter()
        .find(|spots| crossable(spots))
        .or_else(|| tried.iter().find(|spots| reached(spots)))
        .cloned()
        .unwrap_or_else(|| at(0))
}

#[cfg(test)]
mod tests;
