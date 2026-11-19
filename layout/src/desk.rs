//! The PC desktop from the rows the headset shows the monitors in: where each output
//! goes, top-left corner in logical pixels (pixel size divided by the output's scale).
//!
//! One plan for every input (with or without the positions seen in the headset):
//!
//! 1. The row with the main monitor goes left to right by where each monitor is seen
//!    across (`center`), side by side. Vertically each one goes where it is seen
//!    (`middle`), lined up with its neighbour's top, bottom or middle when within
//!    [`LINE_UP`]; it stays in the row (it overlaps the main monitor's height by at
//!    least half the shorter one) and shares at least [`REACH`] of its edge with the
//!    neighbour, so the mouse crosses.
//! 2. Every other row, outwards from that one, is one block of monitors side by side.
//!    The monitor that overlaps the next row inwards most, as seen, keeps its offset
//!    from the one it overlaps; the block overlaps that row by at least [`REACH`].
//!    Each monitor of a row above touches the nearest monitor under it (one of a row
//!    below, the nearest over it), so the mouse leaves one monitor where the eyes see
//!    the next; one over nothing lines up with its row. Where a monitor would rest on
//!    only a sliver of a taller one, the block moves by less than [`REACH`] so it
//!    rests on enough of it or clears it.
//! 3. The laptop screen goes under the main monitor unless the headset put it in a
//!    row; other real displays next to it; window panels in a column right of
//!    everything.
//! 4. The whole plan starts at `0,0`, as KDE wants.
//!
//! Nothing overlaps, whatever the input.

use std::collections::BTreeSet;

/// Shared edge the mouse needs to cross from one monitor to the next (or all of a
/// shorter edge).
pub const REACH: i32 = 200;
/// Within this, a monitor lines up with its neighbour's top, bottom or middle.
pub const LINE_UP: i32 = 24;
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

    /// How long an edge the two share where they touch (0 when they do not).
    pub fn shared_edge<L>(&self, o: &Spot<L>) -> i32 {
        let span = |a0: i32, a1: i32, b0: i32, b1: i32| (a1.min(b1) - a0.max(b0)).max(0);
        if self.right() == o.x || o.right() == self.x {
            span(self.y, self.bottom(), o.y, o.bottom())
        } else if self.bottom() == o.y || o.bottom() == self.y {
            span(self.x, self.right(), o.x, o.right())
        } else {
            0
        }
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
        place_main_row(&rows[main_row], desk.main, &mut placed);
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
    // side by side, each under whatever is over it (level with its neighbour when
    // nothing is).
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
    let spots = settle_block(&placed, block, false, floor, (-FAR, FAR));
    placed.extend(spots);
    // Window panels in a column right of everything.
    let right = placed.iter().map(Spot::right).max().unwrap_or(0);
    let mut y = placed.iter().map(|p| p.y).min().unwrap_or(0);
    for &id in &desk.column {
        if let Some((id, s)) = extra(id) {
            placed.push(Spot {
                id,
                x: right,
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

/// The value nearest `v` among `options` within [`LINE_UP`] and inside `lo..=hi`, or
/// `v`.
fn line_up(v: i32, options: [i32; 3], lo: i32, hi: i32) -> i32 {
    options
        .into_iter()
        .filter(|o| (o - v).abs() <= LINE_UP && (lo..=hi).contains(o))
        .min_by_key(|o| (o - v).abs())
        .unwrap_or(v)
}

/// The main row: side by side from `x = 0`, heights as seen (see the module's
/// description).
fn place_main_row<K: Copy + Ord>(row: &[Seat<K>], main: Option<K>, placed: &mut Vec<Spot<K>>) {
    let tallest = row.iter().map(|m| m.size.h).max().unwrap_or(0);
    // Heights as seen when all are known, else centred in the row.
    let known = row.iter().all(|m| m.middle.is_some());
    let wanted = |m: &Seat<K>| match m.middle {
        Some(middle) if known => middle - m.size.h / 2,
        _ => (tallest - m.size.h) / 2,
    };
    let mut spots: Vec<Spot<K>> = Vec::with_capacity(row.len());
    let mut x = 0;
    for m in row {
        spots.push(Spot {
            id: m.id,
            x,
            y: 0,
            w: m.size.w,
            h: m.size.h,
        });
        x += m.size.w;
    }
    // The main monitor stays where it is seen (the tallest one without a main); the
    // others follow outwards from it.
    let first = row
        .iter()
        .position(|m| Some(m.id) == main)
        .or_else(|| row.iter().position(|m| m.size.h == tallest))
        .unwrap_or(0);
    spots[first].y = wanted(&row[first]);
    let reference = spots[first];
    let settle = |m: &Seat<K>, s: &Spot<K>, n: &Spot<K>| {
        // In the row: overlapping the main monitor's height by half the shorter one.
        let half = s.h.min(reference.h) / 2;
        let (band_lo, band_hi) = (reference.y - s.h + half, reference.bottom() - half);
        // Sharing enough of the edge with the neighbour.
        let k = reach(s.h, n.h);
        let (lo, hi) = (n.y - s.h + k, n.bottom() - k);
        let v = wanted(m).clamp(band_lo, band_hi).clamp(lo, hi);
        line_up(
            v,
            [n.y, n.bottom() - s.h, n.y + (n.h - s.h) / 2],
            band_lo.max(lo).min(hi),
            band_hi.min(hi).max(lo),
        )
    };
    for i in first + 1..row.len() {
        spots[i].y = settle(&row[i], &spots[i], &spots[i - 1]);
    }
    for i in (0..first).rev() {
        spots[i].y = settle(&row[i], &spots[i], &spots[i + 1]);
    }
    placed.extend(spots);
}

/// A row above (`above`) or below the row `next_to`, already placed: one block side by
/// side, offset as seen, every monitor touching the nearest one under (or over) it.
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
    let mut xs = vec![0; row.len()];
    xs[anchor] = n_spot.x + (row[anchor].left() - n.left());
    for i in anchor + 1..row.len() {
        xs[i] = xs[i - 1] + row[i - 1].size.w;
    }
    for i in (0..anchor).rev() {
        xs[i] = xs[i + 1] - row[i].size.w;
    }
    // Overlapping the next row by enough for the mouse.
    let (left, right) = (xs[0], xs[row.len() - 1] + row[row.len() - 1].size.w);
    let lo = neighbours.iter().map(|(_, s)| s.x).min().unwrap_or(0);
    let hi = neighbours.iter().map(|(_, s)| s.right()).max().unwrap_or(0);
    let k = REACH.min(right - left).min(hi - lo);
    let shift = 0.clamp(lo + k - right, hi - k - left);
    let block: Vec<Spot<K>> = row
        .iter()
        .zip(&xs)
        .map(|(m, x)| Spot {
            id: m.id,
            x: x + shift,
            y: 0,
            w: m.size.w,
            h: m.size.h,
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
    let spots = settle_block(placed, block, above, edge, nudge);
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

/// `block`, outputs side by side, over (`above`) or under what is `placed`: each
/// touches the nearest placed output under (over) it, one over nothing lines up with
/// the nearest of the block that is over something (or with `edge`, the far side of
/// what it hangs from, when none is). Moved across within `nudge`, as little as can
/// be, when that keeps every output off a sliver of a taller one.
fn settle_block<K: Copy>(
    placed: &[Spot<K>],
    block: Vec<Spot<K>>,
    above: bool,
    edge: i32,
    nudge: (i32, i32),
) -> Vec<Spot<K>> {
    let mut shifts: Vec<i32> = vec![0];
    for b in &block {
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
    // Where it was put when no move keeps it off slivers.
    shifts
        .into_iter()
        .map(|s| heights(placed, &block, s, above, edge))
        .find(|(_, slivers)| !slivers)
        .unwrap_or_else(|| heights(placed, &block, 0, above, edge))
        .0
}

/// The block moved by `shift`, at its heights; and whether an output there rests on
/// too little of what is under (over) it for the mouse to cross.
fn heights<K: Copy>(
    placed: &[Spot<K>],
    block: &[Spot<K>],
    shift: i32,
    above: bool,
    edge: i32,
) -> (Vec<Spot<K>>, bool) {
    let mut spots: Vec<Spot<K>> = block
        .iter()
        .map(|b| Spot {
            x: b.x + shift,
            ..*b
        })
        .collect();
    // How each one would rest on what it is over.
    let rests: Vec<Rest> = spots
        .iter()
        .map(|s| {
            let across: Vec<&Spot<K>> = placed.iter().filter(|p| p.across(s)).collect();
            let line = if above {
                across.iter().map(|p| p.y).min()
            } else {
                across.iter().map(|p| p.bottom()).max()
            };
            let Some(line) = line else {
                return Rest::Free;
            };
            let y = if above { line - s.h } else { line };
            // Enough of an edge with one it touches for the mouse.
            let enough = across.iter().any(|p| {
                let touches = if above {
                    p.y == line
                } else {
                    p.bottom() == line
                };
                let shared = s.right().min(p.right()) - s.x.max(p.x);
                touches && shared >= reach(s.w, p.w)
            });
            if enough {
                Rest::Firm(y)
            } else {
                Rest::Sliver(y)
            }
        })
        .collect();
    let slivers = rests.iter().any(|r| matches!(r, Rest::Sliver(_)));
    // Those resting firmly stay; the others line up with the neighbour next to them
    // nearer one that does (bottoms above, tops below), as near as they can without
    // going into what they are over. With none resting firmly, slivers stay too.
    let mut ys: Vec<Option<i32>> = rests
        .iter()
        .map(|r| match r {
            Rest::Firm(y) => Some(*y),
            _ => None,
        })
        .collect();
    if ys.iter().all(Option::is_none) {
        ys = rests
            .iter()
            .map(|r| match r {
                Rest::Sliver(y) => Some(*y),
                _ => None,
            })
            .collect();
    }
    loop {
        let before = ys.clone();
        for i in 0..spots.len() {
            if before[i].is_some() {
                continue;
            }
            let next = [i.checked_sub(1), Some(i + 1)]
                .into_iter()
                .flatten()
                .find_map(|j| Some((j, (*before.get(j)?)?)));
            let Some((j, y)) = next else { continue };
            let lined = if above {
                y + spots[j].h - spots[i].h
            } else {
                y
            };
            ys[i] = Some(match rests[i] {
                Rest::Sliver(limit) if above => lined.min(limit),
                Rest::Sliver(limit) => lined.max(limit),
                _ => lined,
            });
        }
        if ys == before {
            break;
        }
    }
    for (s, y) in spots.iter_mut().zip(ys) {
        s.y = y.unwrap_or(if above { edge - s.h } else { edge });
    }
    (spots, slivers)
}

/// How an output would rest on what it is over (see [`heights`]).
#[derive(Debug, Clone, Copy)]
enum Rest {
    /// Over nothing.
    Free,
    /// Touching enough of an output to cross over, at this height.
    Firm(i32),
    /// Touching too little of an output, at this height; never further in.
    Sliver(i32),
}

#[cfg(test)]
mod tests;
