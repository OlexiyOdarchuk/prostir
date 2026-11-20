//! The PC desktop from rows: where each output goes, top-left corner in logical pixels
//! (the compositor's units: pixel size divided by the output's scale).
//!
//! The desktop reproduces the arrangement for movement in every direction: where two
//! monitors meet as seen, the mouse (and a window dragged across) leaves one and enters
//! the other at the same point of the room. How two monitors stand along the edge they
//! share comes from a [`View`] as a [`Relation`]: the offset that puts the middle of the
//! shared edge at the same desktop point on both (with different pixel densities a
//! perfect match along the whole edge is impossible; matching the middle halves the
//! largest jump), how much they share, and how far apart they are.
//!
//! - Within a row, monitors go side by side, left to right as seen. Each is offset up
//!   or down from its left neighbour exactly as seen: a monitor hung a third of its
//!   height higher is a third higher on the desktop. Only a step within [`STEP`] of
//!   lining up (tops, middles or bottoms) is taken as what a hand leaves and lined up,
//!   so a row placed by hand is straight, while an offset made on purpose stays.
//!   Neighbours always share at least [`need`] of their edge: an offset so large that
//!   less would be left is cut back to that.
//! - The row with the main monitor starts at `0,0`. Every other row, outwards from it,
//!   is cut where another row's monitor stands between two of its monitors as seen (a
//!   portrait one reaching up from the row below), and each piece is put across where
//!   it is seen against what is placed (a weighted median of what every pair that
//!   faces each other as seen says, snapped to lining up when within [`NOISE`]). Its
//!   monitor seen nearest the rows below rests on them; the others follow their
//!   neighbours as seen, and rest on what is under them too where they are seen
//!   touching it.
//! - The laptop screen goes under the main monitor unless the headset placed it, other
//!   real displays next to it, window panels in a column right of everything.
//!
//! Nothing ever overlaps and every output can be reached by the mouse, whatever the
//! input (see `plans/layout-math.md` for the proofs).

use std::collections::{BTreeMap, BTreeSet};

use crate::space::Side;

/// Shared edge the mouse needs to cross from one output to the next (or all of a
/// shorter edge).
pub const REACH: i32 = 200;
/// A difference this small, as a fraction of the shorter (narrower) of two monitors, is
/// a hand's noise: it is lined up. 3 % is 2 cm on a 1440-pixel-high monitor at the
/// default size (0.70 m high), above a hand's tremor and the magnet's own precision,
/// and well under any offset made on purpose (see `plans/layout-math.md`).
pub const NOISE: f64 = 0.03;
/// A step between two neighbours in a row this small, as a fraction of the shorter of
/// the two, is what a hand leaves: the row is lined up there. A tenth is 7 cm on a
/// 1440-pixel-high monitor at the default size (0.70 m high, 144 px). Placed by hand in
/// the headset a monitor ends up a few centimetres off, and further as seen when it is
/// pulled nearer or pushed further than its neighbour above or below the eyes (10 cm of
/// depth at 0.8 m over the eyes is a step of about 150 px): on 2026-11-20 the owner,
/// lifting a monitor level with its neighbour, got within 100 px (7 %) and no closer.
/// An offset made on purpose is a sixth of the height or more (240 px), well beyond.
/// Between rows (where a monitor stands across the row next to it) [`NOISE`] still
/// holds: there the plan follows what is seen as closely as it can.
pub const STEP: f64 = 0.10;
/// Two monitors are seen touching when the gap between their facing edges is at most
/// this fraction of the first one's size across it (5 %: 6 cm between two monitors 1.25
/// m wide), or they overlap.
pub const TOUCH_GAP: f64 = 0.05;
/// The headset keeps its previous plan while no output of the new one is further than
/// this fraction of its size (across, and up and down) from where it was (see
/// [`settled`]): noise and a centimetre or so more.
pub const DEADBAND: f64 = 0.05;

/// Positions further out than this on the wire are nonsense (and would overflow).
pub const FAR: i32 = 100_000;

/// A monitor in a row of `SetLayout`, and where the centre of its box is in the
/// headset's plan, in the pixels of the streams (see [`Spot::wire`] and `PROTOCOL.md`,
/// "Layout"): what the PC keeps of what the headset sent. `None`: not known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<K> {
    pub id: K,
    /// Across, left to right.
    pub center: Option<i32>,
    /// Down.
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

/// An output's size on the desktop, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

/// How monitor `b` stands next to monitor `a` along the edge they share as seen, in
/// desktop pixels. For `b` right or left of `a` the edge runs down and `offset` is
/// `b.y - a.y`; for `b` above or below it runs across and `offset` is `b.x - a.x`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Relation {
    /// The offset that puts the middle of the shared edge at the same desktop point on
    /// both, each monitor's pixels where they are seen on it.
    pub offset: f64,
    /// Where that middle is along `a`'s edge, in `a`'s pixels from its top (left).
    pub at: f64,
    /// How much of an edge they share as seen, in `a`'s pixels (0 when they do not
    /// face each other).
    pub shared: f64,
    /// The same as a fraction of the shorter of the two edges, as seen (0..=1).
    pub seam: f64,
    /// How far apart they are across the edge as seen, in `a`'s pixels (negative when
    /// they overlap).
    pub gap: f64,
    /// The same as a fraction of `a`'s size across the edge (its width for a side
    /// neighbour, its height for one above or below).
    pub apart: f64,
}

/// What the plan knows of how the monitors are seen.
pub trait View<K> {
    /// How `b` stands on `side` of `a` as seen, if they face each other there.
    fn relate(&self, a: K, b: K, side: Side) -> Option<Relation>;
    /// Whether another monitor is seen in the gap between `a` and `b` (`a` left of
    /// `b`), so they cannot be put side by side.
    fn between(&self, a: K, b: K) -> bool;
    /// Where the middle of `k` is seen: across (right positive) and up, in any unit.
    fn middle(&self, k: K) -> Option<(f64, f64)>;
    /// Where `k` is on a desktop already, if this view is a plan (the headset's, read
    /// back by the PC): its top-left corner, when its box is whole pixels at `size`.
    fn exact(&self, _k: K, _size: Size) -> Option<(i32, i32)> {
        None
    }
}

/// Nothing is known of how the monitors are seen (a fresh start, `SetOrder`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Unseen;

impl<K> View<K> for Unseen {
    fn relate(&self, _: K, _: K, _: Side) -> Option<Relation> {
        None
    }

    fn between(&self, _: K, _: K) -> bool {
        false
    }

    fn middle(&self, _: K) -> Option<(f64, f64)> {
        None
    }
}

/// What goes on the desktop.
#[derive(Debug, Clone)]
pub struct Desk<K> {
    /// Rows from top to bottom, each left to right. A monitor listed twice counts where
    /// it is listed first.
    pub rows: Vec<Vec<K>>,
    /// The primary monitor; its row is laid out first, from `0,0`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// How many columns the two share (0 when none).
    pub fn across<L>(&self, o: &Spot<L>) -> i32 {
        (self.right().min(o.right()) - self.x.max(o.x)).max(0)
    }

    /// How many lines the two share (0 when none).
    pub fn along<L>(&self, o: &Spot<L>) -> i32 {
        (self.bottom().min(o.bottom()) - self.y.max(o.y)).max(0)
    }

    /// Its centre in whole pixels as `SetLayout` carries it: the top-left corner plus
    /// half the size, rounded down. [`SeenBox::from_wire`] adds the half pixel of an odd
    /// size back, so the PC gets this very spot.
    pub fn wire(&self) -> (i32, i32) {
        (self.x + self.w.div_euclid(2), self.y + self.h.div_euclid(2))
    }

    /// Whether the two cover a pixel in common.
    pub fn overlaps<L>(&self, o: &Spot<L>) -> bool {
        self.across(o) > 0 && self.along(o) > 0
    }

    /// Where `o` touches this one and how long an edge they share (`None` when they do
    /// not touch along an edge).
    pub fn shared_edge<L>(&self, o: &Spot<L>) -> Option<(Side, i32)> {
        let (side, len) = if o.x == self.right() {
            (Side::Right, self.along(o))
        } else if o.right() == self.x {
            (Side::Left, self.along(o))
        } else if o.bottom() == self.y {
            (Side::Above, self.across(o))
        } else if o.y == self.bottom() {
            (Side::Below, self.across(o))
        } else {
            return None;
        };
        (len > 0).then_some((side, len))
    }

    /// The side `o` is on if the mouse crosses from this one to it: they share at least
    /// [`need`] of an edge.
    pub fn crossing<L>(&self, o: &Spot<L>) -> Option<Side> {
        let (side, len) = self.shared_edge(o)?;
        let enough = match side {
            Side::Left | Side::Right => need(self.h, o.h),
            Side::Above | Side::Below => need(self.w, o.w),
        };
        (len >= enough).then_some(side)
    }
}

/// Shared edge the mouse needs between edges this long: [`REACH`] or all of the
/// shorter.
pub fn need(a: i32, b: i32) -> i32 {
    REACH.min(a).min(b).max(1)
}

/// The largest difference taken as noise next to a monitor `len` pixels long, in whole
/// pixels (whole, so planning a plan again decides the same: see [`snap`]).
pub fn noise(len: i32) -> i32 {
    (NOISE * f64::from(len.max(0))).floor() as i32
}

/// [`STEP`] of `len` pixels, rounded down.
pub fn step(len: i32) -> i32 {
    (STEP * f64::from(len.max(0))).floor() as i32
}

/// `value` lined up with the nearest of `options` within `tolerance` pixels, else
/// rounded. Half a pixel more is allowed, so a value already rounded snaps exactly as
/// the unrounded one did.
fn snap(value: f64, options: &[i32], tolerance: i32) -> i32 {
    options
        .iter()
        .copied()
        .map(|o| (o, (value - f64::from(o)).abs()))
        .filter(|(_, d)| *d <= f64::from(tolerance) + 0.5)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or_else(|| value.clamp(-1.0e9, 1.0e9).round() as i32, |(o, _)| o)
}

/// How far down from `a`'s top `b`'s top goes, `b` next to `a` in a row: as seen,
/// lined up with `a`'s middle, top or bottom when within [`STEP`], sharing at least
/// [`need`]. Unknown: `fallback` (middles lined up in a row, tops for the laptop screen
/// and other displays).
fn side_offset(rel: Option<Relation>, ha: i32, hb: i32, fallback: Lining) -> i32 {
    let middle = (ha - hb).div_euclid(2);
    let options = [middle, 0, ha - hb];
    let enough = need(ha, hb);
    let (lo, hi) = (enough - hb, ha - enough);
    let offset = match rel {
        Some(r) if r.offset.is_finite() => snap(r.offset, &options, step(ha.min(hb))),
        _ => match fallback {
            Lining::Middle => middle,
            Lining::Top => 0,
        },
    };
    offset.clamp(lo, hi)
}

/// How monitors whose place is not known line up in a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lining {
    Middle,
    Top,
}

/// A monitor of a row and its size.
#[derive(Debug, Clone, Copy)]
struct Member<K> {
    id: K,
    size: Size,
}

/// Which way a piece goes from what is placed: up onto it or down under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
}

impl Dir {
    /// The side the piece is on, seen from what it rests on.
    fn side(self) -> Side {
        match self {
            Dir::Up => Side::Above,
            Dir::Down => Side::Below,
        }
    }
}

/// Where each output goes (see the module's description). Outputs without a size are
/// left out (an output the compositor does not list yet).
///
/// The headset plans from the room ([`crate::view::Room`]) and sends the plan as boxes
/// ([`as_boxes`]); the PC plans from those boxes and gets the same plan back (planning
/// a plan again gives the same plan).
pub fn plan<K: Copy + Ord, V: View<K>>(
    desk: &Desk<K>,
    size: impl Fn(K) -> Option<Size>,
    view: &V,
) -> Vec<Spot<K>> {
    let usable = |id: K| size(id).filter(|s| s.w > 0 && s.h > 0);
    let (rows, mut listed) = members_of(desk, &usable);

    // The main monitor: the primary, or the first of the widest row (the lower of two).
    let width = |r: &[Member<K>]| r.iter().map(|m| m.size.w).sum::<i32>();
    let main_key = desk
        .main
        .filter(|m| rows.iter().flatten().any(|x| x.id == *m))
        .or_else(|| {
            let r = (0..rows.len()).max_by_key(|&r| (width(&rows[r]), r))?;
            Some(rows[r][0].id)
        });
    let main_index = rows
        .iter()
        .position(|r| r.iter().any(|m| Some(m.id) == main_key));
    let mut placed: Vec<Spot<K>> = Vec::new();
    if let Some(kept) = already_planned(&rows, view, main_index) {
        // A plan made already (the headset's, as the PC reads it back): kept as it is.
        placed = kept;
    } else if !rows.is_empty() {
        let main_row = rows
            .iter()
            .position(|r| r.iter().any(|m| Some(m.id) == main_key))
            .expect("the main monitor is in a row");
        let cut: Vec<Vec<&[Member<K>]>> = (0..rows.len())
            .map(|r| pieces(&rows[r], &bridges(&rows, r, main_row), view, r == main_row))
            .collect();
        // What each row's pieces became, by piece.
        let mut done: Vec<BTreeMap<usize, Vec<Spot<K>>>> = vec![BTreeMap::new(); rows.len()];
        let main_cut = &cut[main_row];
        let first_piece = main_cut
            .iter()
            .position(|p| p.iter().any(|m| Some(m.id) == main_key))
            .unwrap_or(0);
        let first = main_cut[first_piece]
            .iter()
            .position(|m| Some(m.id) == main_key)
            .unwrap_or(0);
        let spots = chain(main_cut[first_piece], first, (0, 0), view, Lining::Middle);
        placed.extend(spots.iter().copied());
        done[main_row].insert(first_piece, spots);
        // The main row's other pieces wait, outwards from the main one, for what holds
        // them where they are seen: the row above that spans them, else the row below,
        // else the piece next to them.
        let mut waiting: Vec<usize> = (first_piece + 1..main_cut.len())
            .chain((0..first_piece).rev())
            .collect();
        let held = |piece: &[Member<K>], placed: &[Spot<K>], side: Side| -> Vec<Spot<K>> {
            placed
                .iter()
                .filter(|p| {
                    piece.iter().any(|m| {
                        view.relate(p.id, m.id, side)
                            .is_some_and(|r| r.shared > 0.0)
                    })
                })
                .copied()
                .collect()
        };
        let row_spots = |done: &BTreeMap<usize, Vec<Spot<K>>>| -> Vec<Spot<K>> {
            done.values().flatten().copied().collect()
        };
        for r in (0..main_row).rev() {
            let adjacent = row_spots(&done[r + 1]);
            for k in 0..cut[r].len() {
                put(
                    &cut[r],
                    k,
                    Dir::Up,
                    &adjacent,
                    &mut placed,
                    &mut done[r],
                    view,
                );
            }
        }
        waiting.retain(|&k| {
            let over = held(cut[main_row][k], &placed, Side::Below);
            if over.is_empty() {
                return true;
            }
            put(
                &cut[main_row],
                k,
                Dir::Down,
                &over,
                &mut placed,
                &mut done[main_row],
                view,
            );
            false
        });
        for r in main_row + 1..rows.len() {
            let adjacent = row_spots(&done[r - 1]);
            for k in 0..cut[r].len() {
                put(
                    &cut[r],
                    k,
                    Dir::Down,
                    &adjacent,
                    &mut placed,
                    &mut done[r],
                    view,
                );
            }
        }
        // Every piece left was held from above (it was cut for that): none is left.
        debug_assert!(waiting.is_empty());
        for k in waiting {
            put(
                &cut[main_row],
                k,
                Dir::Down,
                &[],
                &mut placed,
                &mut done[main_row],
                view,
            );
        }
    }

    // The laptop screen under the main monitor, other real displays right of it, side
    // by side, tops lined up, hanging under whatever is over them.
    let mut extra = |id: K| {
        listed
            .insert(id)
            .then(|| usable(id))
            .flatten()
            .map(|size| Member { id, size })
    };
    let lid = desk.under_main.and_then(&mut extra);
    let members: Vec<Member<K>> = lid
        .into_iter()
        .chain(desk.bottom.iter().filter_map(|&id| extra(id)))
        .collect();
    if !members.is_empty() {
        let main_spot = main_key
            .and_then(|m| placed.iter().find(|p| p.id == m))
            .copied();
        match main_spot {
            Some(main) => {
                let target = match lid {
                    Some(l) => main.x + (main.w - l.size.w).div_euclid(2),
                    None => main.x,
                };
                let spots = place_piece(
                    &members,
                    Dir::Down,
                    &placed,
                    &[main],
                    (None, None),
                    &Unseen,
                    Some(target),
                );
                placed.extend(spots);
            }
            None => placed.extend(chain(&members, 0, (0, 0), &Unseen, Lining::Top)),
        }
    }

    // Window panels in a column right of everything, from the top of the output that
    // reaches furthest right.
    let (x, mut y) = placed
        .iter()
        .max_by_key(|p| (p.right(), std::cmp::Reverse(p.y)))
        .map_or((0, 0), |p| (p.right(), p.y));
    for &id in &desk.column {
        if let Some(m) = extra(id) {
            placed.push(Spot {
                id,
                x,
                y,
                w: m.size.w,
                h: m.size.h,
            });
            y += m.size.h;
        }
    }

    // The desktop starts at 0,0 (KWin, Mutter and Hyprland take that; see `rebase`).
    rebase(&placed, Origin::TopLeft)
}

/// The rows' monitors where `view` says they are on a desktop already, when it says so
/// for all of them at their sizes and that desktop is one this function would make:
/// within each piece of a row neighbours side by side, nothing overlapping, a row above
/// another above it, and every monitor in reach. So a
/// plan planned again (the PC reading the headset's plan) is that plan, to the pixel,
/// and anything else (an old headset's numbers, a staircase) is planned afresh.
fn already_planned<K: Copy + Ord, V: View<K>>(
    rows: &[Vec<Member<K>>],
    view: &V,
    main_row: Option<usize>,
) -> Option<Vec<Spot<K>>> {
    if rows.is_empty() {
        return None;
    }
    let mut spots = Vec::new();
    for m in rows.iter().flatten() {
        let (x, y) = view.exact(m.id, m.size)?;
        spots.push(Spot {
            id: m.id,
            x,
            y,
            w: m.size.w,
            h: m.size.h,
        });
    }
    let at = |id: K| spots.iter().find(|s| s.id == id).copied();
    // Neighbours side by side, or apart where the plan cuts a row (another monitor seen
    // between them, or a row over or under holding both sides).
    for r in 0..rows.len() {
        let main = Some(r) == main_row;
        let holders = bridges(rows, r, main_row.unwrap_or(r));
        for w in rows[r].windows(2) {
            let (a, b) = (at(w[0].id)?, at(w[1].id)?);
            let side_by_side = b.x == a.right();
            let cut = b.x > a.right()
                && ((!main && view.between(a.id, b.id))
                    || bridged(a.id, b.id, &holders, main, view));
            if !side_by_side && !cut {
                return None;
            }
        }
    }
    let row_of = |id: K| rows.iter().position(|r| r.iter().any(|m| m.id == id));
    for (i, a) in spots.iter().enumerate() {
        for b in &spots[i + 1..] {
            if a.overlaps(b) {
                return None;
            }
            let (ra, rb) = (row_of(a.id), row_of(b.id));
            let (upper, lower) = if ra < rb { (a, b) } else { (b, a) };
            if ra != rb && upper.across(lower) > 0 && upper.bottom() > lower.y {
                return None;
            }
        }
    }
    let mut reached = vec![false; spots.len()];
    reached[0] = true;
    let mut stack = vec![0];
    while let Some(i) = stack.pop() {
        for j in 0..spots.len() {
            if !reached[j] && spots[i].crossing(&spots[j]).is_some() {
                reached[j] = true;
                stack.push(j);
            }
        }
    }
    reached.iter().all(|&r| r).then_some(spots)
}

/// The rows' monitors with a size, each listed once (where first listed), empty rows
/// dropped; and every key listed.
fn members_of<K: Copy + Ord>(
    desk: &Desk<K>,
    usable: &impl Fn(K) -> Option<Size>,
) -> (Vec<Vec<Member<K>>>, BTreeSet<K>) {
    let mut listed = BTreeSet::new();
    let rows = desk
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .filter(|&&id| listed.insert(id))
                .filter_map(|&id| {
                    Some(Member {
                        id,
                        size: usable(id)?,
                    })
                })
                .collect::<Vec<_>>()
        })
        .filter(|r| !r.is_empty())
        .collect();
    (rows, listed)
}

/// The pieces each row is cut into on the desktop (see [`plan`]); within a piece the
/// monitors go side by side. Rows top to bottom as [`plan`] takes them.
pub fn pieces_of<K: Copy + Ord, V: View<K>>(
    desk: &Desk<K>,
    size: impl Fn(K) -> Option<Size>,
    view: &V,
) -> Vec<Vec<Vec<K>>> {
    let usable = |id: K| size(id).filter(|s| s.w > 0 && s.h > 0);
    let (rows, _) = members_of(desk, &usable);
    let main_key = desk
        .main
        .filter(|m| rows.iter().flatten().any(|x| x.id == *m))
        .or_else(|| {
            let width = |r: &[Member<K>]| r.iter().map(|m| m.size.w).sum::<i32>();
            let r = (0..rows.len()).max_by_key(|&r| (width(&rows[r]), r))?;
            Some(rows[r][0].id)
        });
    let main_row = rows
        .iter()
        .position(|r| r.iter().any(|m| Some(m.id) == main_key))
        .unwrap_or(0);
    (0..rows.len())
        .map(|r| {
            pieces(&rows[r], &bridges(&rows, r, main_row), view, r == main_row)
                .into_iter()
                .map(|p| p.iter().map(|m| m.id).collect())
                .collect()
        })
        .collect()
}

/// A row side by side from `members[first]` at `at` (its top-left corner), each
/// neighbour offset up or down as seen (see [`side_offset`]).
fn chain<K: Copy, V: View<K>>(
    members: &[Member<K>],
    first: usize,
    at: (i32, i32),
    view: &V,
    fallback: Lining,
) -> Vec<Spot<K>> {
    let n = members.len();
    let mut xs = vec![0; n];
    let mut ys = vec![0; n];
    (xs[first], ys[first]) = at;
    for i in first + 1..n {
        let (a, b) = (members[i - 1], members[i]);
        let rel = view.relate(a.id, b.id, Side::Right);
        xs[i] = xs[i - 1] + a.size.w;
        ys[i] = ys[i - 1] + side_offset(rel, a.size.h, b.size.h, fallback);
    }
    for i in (0..first).rev() {
        let (a, b) = (members[i], members[i + 1]);
        let rel = view.relate(a.id, b.id, Side::Right);
        xs[i] = xs[i + 1] - a.size.w;
        ys[i] = ys[i + 1] - side_offset(rel, a.size.h, b.size.h, fallback);
    }
    members
        .iter()
        .enumerate()
        .map(|(i, m)| Spot {
            id: m.id,
            x: xs[i],
            y: ys[i],
            w: m.size.w,
            h: m.size.h,
        })
        .collect()
}

/// A gap seen between two neighbours of a row, under this fraction of the narrower,
/// is closed on the desktop: they are meant to touch.
pub const GAP_CLOSE: f64 = 0.1;

/// A row cut into pieces that each go side by side: where another monitor is seen
/// between two neighbours, or where they are seen clearly apart ([`GAP_CLOSE`]) under or
/// over another row that spans the gap without a gap of its own there (then each side
/// goes where it is seen against that row). Any other gap is closed: the two are meant
/// to touch, or nothing else could join them.
fn pieces<'a, K: Copy + PartialEq, V: View<K>>(
    row: &'a [Member<K>],
    bridges: &[&Vec<Member<K>>],
    view: &V,
    main: bool,
) -> Vec<&'a [Member<K>]> {
    let apart = |a: Member<K>, b: Member<K>| {
        let narrower = f64::from(a.size.w.min(b.size.w).max(1));
        view.relate(a.id, b.id, Side::Right)
            .map_or(0.0, |r| r.apart * f64::from(a.size.w) / narrower)
    };
    let cut_here = |a: Member<K>, b: Member<K>| {
        let big = apart(a, b) > GAP_CLOSE;
        if main {
            big && bridged(a.id, b.id, bridges, main, view)
        } else {
            view.between(a.id, b.id) || (big && bridged(a.id, b.id, bridges, main, view))
        }
    };
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..row.len() {
        if cut_here(row[i - 1], row[i]) {
            out.push(&row[start..i]);
            start = i;
        }
    }
    out.push(&row[start..]);
    out
}

/// The rows that may hold row `r`'s pieces apart: every other row, or for the main row
/// the rows above it (placed before its other pieces).
fn bridges<K>(rows: &[Vec<Member<K>>], r: usize, main_row: usize) -> Vec<&Vec<Member<K>>> {
    (0..rows.len())
        .filter(|&o| o != r && (r != main_row || o < main_row))
        .map(|o| &rows[o])
        .collect()
}

/// Whether one of `bridges` has a monitor over (under) `a` and one over (under) `b`,
/// joined in that row without a gap to keep between them. The main row is laid out
/// first, so only a row above it can hold its other pieces (placed after the rows
/// above).
fn bridged<K: Copy + PartialEq, V: View<K>>(
    a: K,
    b: K,
    bridges: &[&Vec<Member<K>>],
    main: bool,
    view: &V,
) -> bool {
    let faces = |a: K, c: K, side: Side| view.relate(a, c, side).is_some_and(|r| r.shared > 0.0);
    let apart = |a: Member<K>, b: Member<K>| {
        let narrower = f64::from(a.size.w.min(b.size.w).max(1));
        view.relate(a.id, b.id, Side::Right)
            .map_or(0.0, |r| r.apart * f64::from(a.size.w) / narrower)
    };
    let sides: &[Side] = if main {
        &[Side::Above]
    } else {
        &[Side::Above, Side::Below]
    };
    bridges.iter().any(|r| {
        sides.iter().copied().any(|side| {
            let first = r.iter().position(|m| faces(a, m.id, side));
            let last = r.iter().rposition(|m| faces(b, m.id, side));
            match (first, last) {
                (Some(i), Some(j)) if i <= j => {
                    r[i..=j].windows(2).all(|w| apart(w[0], w[1]) <= GAP_CLOSE)
                }
                _ => false,
            }
        })
    })
}

/// Piece `k` of a row (its pieces `cut`) placed going `dir` from what is `placed`,
/// against `adjacent`, between the pieces of its row already placed (`done`).
fn put<K: Copy + Ord, V: View<K>>(
    cut: &[&[Member<K>]],
    k: usize,
    dir: Dir,
    adjacent: &[Spot<K>],
    placed: &mut Vec<Spot<K>>,
    done: &mut BTreeMap<usize, Vec<Spot<K>>>,
    view: &V,
) {
    let left = (0..k)
        .rev()
        .find_map(|j| done.get(&j))
        .and_then(|s| s.last().copied());
    let right = (k + 1..cut.len())
        .find_map(|j| done.get(&j))
        .and_then(|s| s.first().copied());
    let spots = place_piece(cut[k], dir, placed, adjacent, (left, right), view, None);
    placed.extend(spots.iter().copied());
    done.insert(k, spots);
}

/// A piece of a row placed up onto (`Dir::Up`) or down under (`Dir::Down`) what is
/// `placed`, next to the outputs `adjacent`, between the pieces of its own row placed
/// already (`bounds`: the last monitor of the one on its left, the first of the one on
/// its right), as near across to where it is seen (or to `target`) as it can be while
/// the mouse can cross into it.
///
/// Across: what every pair that faces each other as seen says (the weighted median),
/// snapped to lining up with an edge or a centre when within noise. The candidates are
/// that, and every offset that lines an edge or a centre of the piece's monitors up with
/// one of a placed output's (or leaves exactly what the mouse needs). The winner shares
/// an edge the mouse can cross, with the adjacent row if it can, within the window
/// where it shares at least [`REACH`] with that row if it can, then is nearest to the
/// goal, then shares the most, then is leftmost. One candidate always shares enough:
/// the piece's anchor centred on the highest placed output (see
/// `plans/layout-math.md`).
fn place_piece<K: Copy + Ord, V: View<K>>(
    piece: &[Member<K>],
    dir: Dir,
    placed: &[Spot<K>],
    adjacent: &[Spot<K>],
    bounds: (Option<Spot<K>>, Option<Spot<K>>),
    view: &V,
    target: Option<i32>,
) -> Vec<Spot<K>> {
    let n = piece.len();
    let mut ix = vec![0; n];
    for i in 1..n {
        ix[i] = ix[i - 1] + piece[i - 1].size.w;
    }
    let width = ix[n - 1] + piece[n - 1].size.w;
    let fallback = if target.is_some() {
        Lining::Top
    } else {
        Lining::Middle
    };
    // Offsets between neighbours as seen.
    let down: Vec<i32> = (0..n)
        .map(|i| {
            if i == 0 {
                0
            } else {
                let (a, b) = (piece[i - 1], piece[i]);
                side_offset(
                    view.relate(a.id, b.id, Side::Right),
                    a.size.h,
                    b.size.h,
                    fallback,
                )
            }
        })
        .collect();
    // How each monitor is seen against what is placed: touching (as a fraction of the
    // output it touches), and votes for where the piece goes across.
    // Votes for where the piece goes across, from every pair facing each other as seen,
    // and which monitors are seen touching what is placed through an edge the mouse
    // could cross (overlapping as seen counts: on the desktop they can only touch).
    // The placed outputs each monitor is seen touching: member, output, the side the
    // output is on, and the offset along the edge they share as seen (lined up when
    // within noise): the member's x minus the output's for one over the other, its y
    // minus the output's for one beside the other.
    let mut seen_touching: Vec<(usize, K, Side, i32)> = Vec::new();
    let onto_side = match dir {
        Dir::Up => Side::Below,
        Dir::Down => Side::Above,
    };
    let mut votes: Vec<(f64, f64)> = Vec::new();
    for (i, m) in piece.iter().enumerate() {
        for p in placed {
            let Some(rel) = view
                .relate(p.id, m.id, dir.side())
                .filter(|r| r.shared > 0.0)
            else {
                continue;
            };
            let near = rel.apart <= TOUCH_GAP;
            // The row next to it says where it goes across; another one only where
            // they are seen touching (a portrait reaching up from further down).
            if near || adjacent.iter().any(|s| s.id == p.id) {
                votes.push((f64::from(p.x) + rel.offset - f64::from(ix[i]), rel.shared));
            }
            if near && rel.shared >= f64::from(need(p.w, m.size.w)) {
                let (pw, mw) = (p.w, m.size.w);
                let lined = [(pw - mw).div_euclid(2), 0, pw - mw];
                let offset = snap(rel.offset, &lined, noise(pw.min(mw)));
                seen_touching.push((i, p.id, onto_side, offset));
            }
        }
    }
    // A placed output seen touching the piece's first monitor on its left, or its last
    // on its right, puts it there.
    for p in placed {
        let ends = [(0usize, Side::Right), (n - 1, Side::Left)];
        for (i, side) in ends {
            let Some(rel) = view.relate(p.id, piece[i].id, side) else {
                continue;
            };
            if rel.shared > 0.0 && rel.apart.abs() <= TOUCH_GAP {
                let x = match side {
                    Side::Right => p.right() - ix[i],
                    _ => p.x - ix[i] - piece[i].size.w,
                };
                votes.push((f64::from(x), rel.shared));
                if rel.shared >= f64::from(need(p.h, piece[i].size.h)) {
                    let offset = side_offset(Some(rel), p.h, piece[i].size.h, fallback);
                    seen_touching.push((i, p.id, side.opposite(), offset));
                }
            }
        }
    }

    let a0 = adjacent.iter().map(|s| s.x).min().unwrap_or(0);
    let a1 = adjacent.iter().map(|s| s.right()).max().unwrap_or(0);
    let k = REACH.min(width).min(a1 - a0).max(1);
    let (lo, hi) = (a0 + k - width, (a1 - k).max(a0 + k - width));
    let seen_x = mean(&votes);
    let wanted = match (target, seen_x) {
        (Some(t), _) => f64::from(t),
        (None, Some(x)) => x,
        (None, None) => f64::from(a0 + (a1 - a0 - width).div_euclid(2)),
    };

    // Candidates across, each with how close a goal must be to snap onto it.
    let narrowest = piece.iter().map(|m| m.size.w).min().unwrap_or(1);
    let mut candidates: Vec<(i32, i32)> = vec![(lo, noise(narrowest)), (hi, noise(narrowest))];
    for (i, m) in piece.iter().enumerate() {
        let (bx, w) = (ix[i], m.size.w);
        for p in placed {
            let (t, nd) = (noise(w.min(p.w)), need(w, p.w));
            candidates.extend(
                [
                    p.x - bx,
                    p.right() - (bx + w),
                    p.x + (p.w - w).div_euclid(2) - bx,
                    p.right() - bx,
                    p.x - (bx + w),
                    p.right() - nd - bx,
                    p.x + nd - (bx + w),
                ]
                .map(|c| (c, t)),
            );
        }
    }
    // The one that always works: the deepest monitor centred on the highest placed
    // output (the lowest, going down).
    let highest = placed.iter().min_by_key(|p| match dir {
        Dir::Up => (p.y, p.x),
        Dir::Down => (-p.bottom(), p.x),
    });
    // The piece's deepest monitor (the one reaching furthest down, going up; up, going
    // down) as the chain stands.
    let rel_y: Vec<i32> = down
        .iter()
        .scan(0, |y, d| {
            *y += d;
            Some(*y)
        })
        .collect();
    let deepest = (0..n)
        .max_by_key(|&i| match dir {
            Dir::Up => (rel_y[i] + piece[i].size.h, std::cmp::Reverse(i)),
            Dir::Down => (-rel_y[i], std::cmp::Reverse(i)),
        })
        .expect("a piece has monitors");
    let sure = highest.map(|h| h.x + (h.w - piece[deepest].size.w).div_euclid(2) - ix[deepest]);
    if let Some(s) = sure {
        candidates.push((s, noise(narrowest)));
    }
    // Between the pieces of its own row placed already, touching them at the nearest.
    let (left, right) = bounds;
    if let Some(l) = left {
        candidates.push((l.right(), noise(narrowest)));
    }
    if let Some(r) = right {
        candidates.push((r.x - width, noise(narrowest)));
    }
    let free =
        |x: i32| left.is_none_or(|l| x >= l.right()) && right.is_none_or(|r| x + width <= r.x);
    let goal = {
        let options: Vec<i32> = candidates.iter().map(|c| c.0).collect();
        let tolerance = |o: i32| {
            candidates
                .iter()
                .filter(|c| c.0 == o)
                .map(|c| c.1)
                .max()
                .unwrap_or(0)
        };
        options
            .iter()
            .copied()
            .map(|o| (o, (wanted - f64::from(o)).abs()))
            .filter(|(o, d)| *d <= f64::from(tolerance(*o)) + 0.5)
            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
            .map_or_else(|| wanted.clamp(-1.0e9, 1.0e9).round() as i32, |(o, _)| o)
    };
    let mut xs: Vec<i32> = candidates.iter().map(|c| c.0).collect();
    xs.push(goal);
    xs.push(goal.clamp(lo, hi));
    xs.sort_unstable();
    xs.dedup();

    let adjacent_ids: Vec<K> = adjacent.iter().map(|s| s.id).collect();
    type Key = (
        bool,
        bool,
        usize,
        std::cmp::Reverse<i64>,
        bool,
        bool,
        std::cmp::Reverse<i64>,
        i64,
        std::cmp::Reverse<i32>,
    );
    let mut best: Option<(Key, Vec<Spot<K>>)> = None;
    // At each place across: resting on what is under it, and held by the side of an
    // output seen touching its end, as seen there (when that goes into nothing).
    let mut layouts: Vec<(i32, Vec<Spot<K>>)> = Vec::new();
    for &x in &xs {
        if let Some(spots) = settle(piece, &ix, &down, dir, placed, x) {
            layouts.push((x, spots));
        }
        for &(i, pid, side, offset) in &seen_touching {
            if !matches!(side, Side::Left | Side::Right) {
                continue;
            }
            let Some(p) = placed.iter().find(|p| p.id == pid) else {
                continue;
            };
            let at = x + ix[i];
            let beside = match side {
                Side::Left => at == p.right(),
                _ => at + piece[i].size.w == p.x,
            };
            if !beside {
                continue;
            }
            let rel_y: Vec<i32> = down
                .iter()
                .scan(0, |y, d| {
                    *y += d;
                    Some(*y)
                })
                .collect();
            let top = p.y + offset - rel_y[i];
            let spots = chain_spots(piece, &ix, &down, x, top);
            // Above everything under it (below everything over it, going down).
            let clear = spots.iter().all(|s| {
                placed
                    .iter()
                    .filter(|q| s.across(q) > 0)
                    .all(|q| match dir {
                        Dir::Up => s.bottom() <= q.y,
                        Dir::Down => s.y >= q.bottom(),
                    })
            });
            if clear {
                layouts.push((x, spots));
            }
        }
    }
    for (x, spots) in layouts {
        let mut usable = false;
        let mut with_adjacent = false;
        let mut shared: i64 = 0;
        // Pairs seen touching that the mouse crosses between here, on the right side, and
        // how far each is from standing against the other as seen.
        let mut kept = 0usize;
        let mut misfit: i64 = 0;
        for (i, s) in spots.iter().enumerate() {
            for p in placed {
                let Some(side) = s.crossing(p) else { continue };
                usable = true;
                let len = match side {
                    Side::Left | Side::Right => s.along(p),
                    Side::Above | Side::Below => s.across(p),
                };
                shared += i64::from(len);
                let onto = matches!(
                    (dir, side),
                    (Dir::Up, Side::Below) | (Dir::Down, Side::Above)
                );
                if onto && adjacent_ids.contains(&p.id) {
                    with_adjacent = true;
                }
                let seen = seen_touching
                    .iter()
                    .find(|t| t.0 == i && t.1 == p.id && t.2 == side);
                if let Some(&(_, _, _, offset)) = seen {
                    kept += 1;
                    let actual = match side {
                        Side::Left | Side::Right => s.y - p.y,
                        Side::Above | Side::Below => s.x - p.x,
                    };
                    misfit += i64::from((actual - offset).abs());
                }
            }
        }
        let key = (
            free(x),
            usable,
            kept,
            std::cmp::Reverse(misfit),
            with_adjacent,
            (lo..=hi).contains(&x),
            std::cmp::Reverse((i64::from(x) - i64::from(goal)).abs()),
            shared,
            std::cmp::Reverse(x),
        );
        if best.as_ref().is_none_or(|(k, _)| key > *k) {
            best = Some((key, spots));
        }
    }
    match best {
        Some(((true, true, ..), spots)) => spots,
        best => {
            // No place between the row's other pieces rests on anything with an edge the
            // mouse can cross: right beside the one on its left (or on its right), offset
            // from it as seen, never into anything placed.
            let beside = match (left, right) {
                (Some(l), _) => {
                    let first = piece[0];
                    let rel = view.relate(l.id, first.id, Side::Right);
                    Some((
                        l.right(),
                        l.y + side_offset(rel, l.h, first.size.h, fallback),
                    ))
                }
                (None, Some(r)) => {
                    let last = piece[n - 1];
                    let rel = view.relate(last.id, r.id, Side::Right);
                    let y_last = r.y - side_offset(rel, last.size.h, r.h, fallback);
                    Some((r.x - width, y_last - rel_y[n - 1]))
                }
                (None, None) => None,
            };
            match beside {
                Some((x, y)) => {
                    let mut spots = chain_spots(piece, &ix, &down, x, y);
                    for s in &mut spots {
                        let under = placed.iter().filter(|p| s.x < p.right() && p.x < s.right());
                        match dir {
                            Dir::Up => {
                                if let Some(top) = under.map(|p| p.y).min() {
                                    s.y = s.y.min(top - s.h);
                                }
                            }
                            Dir::Down => {
                                if let Some(bottom) = under.map(|p| p.bottom()).max() {
                                    s.y = s.y.max(bottom);
                                }
                            }
                        }
                    }
                    spots
                }
                None => best.map_or_else(|| chain_spots(piece, &ix, &down, goal, 0), |(_, s)| s),
            }
        }
    }
}

/// Weighted mean of `(value, weight)`; `None` without weight. A mean, not a median: it
/// moves smoothly when one vote does (a hand's tremor moves the piece as little), and
/// it puts the squared errors at the seams' middles at their least.
fn mean(votes: &[(f64, f64)]) -> Option<f64> {
    let total: f64 = votes.iter().map(|v| v.1.max(0.0)).sum();
    (total > 0.0).then(|| votes.iter().map(|v| v.0 * v.1.max(0.0)).sum::<f64>() / total)
}

/// The piece side by side from `x`, its first monitor's top at `y`, offsets as given.
fn chain_spots<K: Copy>(
    piece: &[Member<K>],
    ix: &[i32],
    down: &[i32],
    x: i32,
    y: i32,
) -> Vec<Spot<K>> {
    let mut top = y;
    piece
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if i > 0 {
                top += down[i];
            }
            Spot {
                id: m.id,
                x: x + ix[i],
                y: top,
                w: m.size.w,
                h: m.size.h,
            }
        })
        .collect()
}

/// The piece at `x`: side by side, each monitor offset from its neighbour as seen, the
/// whole resting on what is placed under it (hanging under what is over it, going down)
/// at its tightest point; then each monitor within noise of resting on what is under it
/// goes down onto it, as long as both its neighbours still share what the mouse needs
/// (further than noise, how it stands against its neighbours as seen wins). Never into
/// anything placed. `None` when nothing placed is under (over) any of its monitors.
fn settle<K: Copy>(
    piece: &[Member<K>],
    ix: &[i32],
    down: &[i32],
    dir: Dir,
    placed: &[Spot<K>],
    x: i32,
) -> Option<Vec<Spot<K>>> {
    let n = piece.len();
    // Where each monitor would rest (hang), if anything is under (over) it.
    let rest: Vec<Option<i32>> = (0..n)
        .map(|i| {
            let (l, r) = (x + ix[i], x + ix[i] + piece[i].size.w);
            let under = placed.iter().filter(|p| l < p.right() && p.x < r);
            match dir {
                Dir::Up => under.map(|p| p.y).min().map(|top| top - piece[i].size.h),
                Dir::Down => under.map(|p| p.bottom()).max(),
            }
        })
        .collect();
    // The chain as seen, then the whole of it onto what is under it.
    let rel_y: Vec<i32> = down
        .iter()
        .scan(0, |y, d| {
            *y += d;
            Some(*y)
        })
        .collect();
    let offsets = (0..n).filter_map(|i| Some(rest[i]? - rel_y[i]));
    let lift = match dir {
        Dir::Up => offsets.min()?,
        Dir::Down => offsets.max()?,
    };
    let mut ys: Vec<i32> = rel_y.iter().map(|y| lift + y).collect();
    // Each all but touching what is under it down onto it.
    let fits = |ys: &[i32], i: usize, j: usize| {
        let (a, b) = (i.min(j), i.max(j));
        let (ha, hb) = (piece[a].size.h, piece[b].size.h);
        let enough = need(ha, hb);
        (ys[a] + enough - hb..=ys[a] + ha - enough).contains(&ys[b])
    };
    for i in 0..n {
        let Some(r) = rest[i] else { continue };
        if r == ys[i] || (r - ys[i]).abs() > noise(piece[i].size.h) {
            continue;
        }
        let was = ys[i];
        ys[i] = r;
        let ok = (i == 0 || fits(&ys, i - 1, i)) && (i + 1 == n || fits(&ys, i, i + 1));
        if !ok {
            ys[i] = was;
        }
    }
    Some(
        piece
            .iter()
            .enumerate()
            .map(|(i, m)| Spot {
                id: m.id,
                x: x + ix[i],
                y: ys[i],
                w: m.size.w,
                h: m.size.h,
            })
            .collect(),
    )
}

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

/// Where a monitor is seen, as a box: centre across (right positive) and down, and
/// size, in a unit all monitors share. On the wire: the centre and middle of
/// `SetLayout` and the stream's size in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeenBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl SeenBox {
    pub fn left(&self) -> f64 {
        self.x - self.w / 2.0
    }

    pub fn right(&self) -> f64 {
        self.x + self.w / 2.0
    }

    pub fn top(&self) -> f64 {
        self.y - self.h / 2.0
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h / 2.0
    }

    fn usable(&self) -> bool {
        [self.x, self.y, self.w, self.h]
            .iter()
            .all(|v| v.is_finite() && v.abs() < 1.0e7)
            && self.w > 0.0
            && self.h > 0.0
    }

    /// The box of a planned spot (its own pixels).
    pub fn of<K>(s: &Spot<K>) -> Self {
        Self {
            x: f64::from(s.x) + f64::from(s.w) / 2.0,
            y: f64::from(s.y) + f64::from(s.h) / 2.0,
            w: f64::from(s.w),
            h: f64::from(s.h),
        }
    }

    /// From `SetLayout`: the centre (`center`, `middle`) in the pixels of the headset's
    /// plan as [`Spot::wire`] rounds it, and the stream's size in pixels.
    pub fn from_wire(center: i32, middle: i32, stream: Size) -> Self {
        let half = |len: i32| if len % 2 == 0 { 0.0 } else { 0.5 };
        Self {
            x: f64::from(center) + half(stream.w),
            y: f64::from(middle) + half(stream.h),
            w: f64::from(stream.w),
            h: f64::from(stream.h),
        }
    }
}

/// A view of boxes: each monitor seen as a box, its pixels spread evenly over it. What
/// the PC has from `SetLayout`; also the headset's plan read back.
#[derive(Debug, Clone)]
pub struct Boxes<K> {
    /// Each monitor's box and its size on the desktop.
    pub boxes: Vec<(K, SeenBox, Size)>,
}

impl<K: Copy + Ord> Boxes<K> {
    pub fn new(boxes: Vec<(K, SeenBox, Size)>) -> Self {
        Self {
            boxes: boxes
                .into_iter()
                .filter(|b| b.1.usable() && b.2.w > 0 && b.2.h > 0)
                .collect(),
        }
    }

    fn get(&self, id: K) -> Option<(SeenBox, Size)> {
        self.boxes.iter().find(|b| b.0 == id).map(|b| (b.1, b.2))
    }
}

impl<K: Copy + Ord> View<K> for Boxes<K> {
    fn relate(&self, a: K, b: K, side: Side) -> Option<Relation> {
        let ((ba, sa), (bb, sb)) = (self.get(a)?, self.get(b)?);
        // `b` must be on that side of `a` at all.
        let on_side = match side {
            Side::Right => bb.x > ba.x,
            Side::Left => bb.x < ba.x,
            Side::Above => bb.y < ba.y,
            Side::Below => bb.y > ba.y,
        };
        if !on_side {
            return None;
        }
        // Along the shared edge: (start, end, length in pixels) of each.
        let (a0, a1, an, b0, b1, bn, gap, scale) = match side {
            Side::Right => (
                ba.top(),
                ba.bottom(),
                sa.h,
                bb.top(),
                bb.bottom(),
                sb.h,
                bb.left() - ba.right(),
                f64::from(sa.w) / ba.w,
            ),
            Side::Left => (
                ba.top(),
                ba.bottom(),
                sa.h,
                bb.top(),
                bb.bottom(),
                sb.h,
                ba.left() - bb.right(),
                f64::from(sa.w) / ba.w,
            ),
            Side::Above => (
                ba.left(),
                ba.right(),
                sa.w,
                bb.left(),
                bb.right(),
                sb.w,
                ba.top() - bb.bottom(),
                f64::from(sa.h) / ba.h,
            ),
            Side::Below => (
                ba.left(),
                ba.right(),
                sa.w,
                bb.left(),
                bb.right(),
                sb.w,
                bb.top() - ba.bottom(),
                f64::from(sa.h) / ba.h,
            ),
        };
        let (lo, hi) = (a0.max(b0), a1.min(b1));
        let at = |v: f64, s0: f64, s1: f64, n: i32| (v - s0) / (s1 - s0) * f64::from(n);
        let m = if hi > lo {
            (lo + hi) / 2.0
        } else {
            (a0 + a1 + b0 + b1) / 4.0
        };
        Some(Relation {
            offset: at(m, a0, a1, an) - at(m, b0, b1, bn),
            at: at(m, a0, a1, an),
            shared: ((hi - lo) / (a1 - a0) * f64::from(an)).max(0.0),
            seam: ((hi - lo) / (a1 - a0).min(b1 - b0)).clamp(0.0, 1.0),
            gap: gap * scale,
            apart: match side {
                Side::Left | Side::Right => gap / ba.w,
                Side::Above | Side::Below => gap / ba.h,
            },
        })
    }

    fn between(&self, a: K, b: K) -> bool {
        let (Some((ba, _)), Some((bb, _))) = (self.get(a), self.get(b)) else {
            return false;
        };
        let (l, r) = (ba.right(), bb.left());
        let (t, btm) = (ba.top().max(bb.top()), ba.bottom().min(bb.bottom()));
        if r - l <= NOISE * ba.w.min(bb.w) || btm <= t {
            return false;
        }
        self.boxes.iter().any(|(id, c, _)| {
            *id != a
                && *id != b
                && c.right().min(r) - c.left().max(l) > NOISE * (r - l)
                && c.bottom().min(btm) - c.top().max(t) > NOISE * (btm - t)
        })
    }

    fn middle(&self, k: K) -> Option<(f64, f64)> {
        self.get(k).map(|(b, _)| (b.x, -b.y))
    }

    fn exact(&self, k: K, size: Size) -> Option<(i32, i32)> {
        let (b, _) = self.get(k)?;
        let whole = |v: f64| (v.fract() == 0.0).then_some(v as i32);
        (b.w == f64::from(size.w) && b.h == f64::from(size.h)).then_some(())?;
        Some((whole(b.left())?, whole(b.top())?))
    }
}

/// The plan as boxes, each monitor at its spot: what the headset sends as `SetLayout`
/// (the centre and middle of each spot) and what planning again reproduces.
pub fn as_boxes<K: Copy + Ord>(plan: &[Spot<K>]) -> Boxes<K> {
    Boxes::new(
        plan.iter()
            .map(|s| (s.id, SeenBox::of(s), Size { w: s.w, h: s.h }))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spot sent as `SetLayout` (centre and middle in whole pixels) comes back as its
    /// own box, odd sizes too, so the PC plans the headset's plan to the pixel.
    #[test]
    fn a_spot_survives_the_wire() {
        for (w, h) in [(2560, 1440), (1366, 768), (1365, 767), (1, 1)] {
            for (x, y) in [(0, 0), (-1281, 7), (4096, -2049)] {
                let s = Spot {
                    id: 0u8,
                    x,
                    y,
                    w,
                    h,
                };
                let (c, m) = s.wire();
                assert_eq!(SeenBox::from_wire(c, m, Size { w, h }), SeenBox::of(&s));
                let boxes = Boxes::new(vec![(
                    0u8,
                    SeenBox::from_wire(c, m, Size { w, h }),
                    Size { w, h },
                )]);
                assert_eq!(boxes.exact(0, Size { w, h }), Some((x, y)));
            }
        }
    }
}
