//! How the planner places the rows: the pieces of each row, chained, placed
//! against what is placed already, and settled.

use super::*;

/// The rows' monitors where `view` says they are on a desktop already, when it says so
/// for all of them at their sizes and that desktop is one this function would make:
/// within each piece of a row neighbours side by side, nothing overlapping, a row above
/// another above it, and every monitor in reach. So a
/// plan planned again (the PC reading the headset's plan) is that plan, to the pixel,
/// and anything else (an old headset's numbers, a staircase) is planned afresh.
pub(super) fn already_planned<K: Copy + Ord, V: View<K>>(
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
pub(super) fn members_of<K: Copy + Ord>(
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
pub(super) fn chain<K: Copy, V: View<K>>(
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
pub(super) fn pieces<'a, K: Copy + PartialEq, V: View<K>>(
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
pub(super) fn bridges<K>(
    rows: &[Vec<Member<K>>],
    r: usize,
    main_row: usize,
) -> Vec<&Vec<Member<K>>> {
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
pub(super) fn put<K: Copy + Ord, V: View<K>>(
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
pub(super) fn place_piece<K: Copy + Ord, V: View<K>>(
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
    let a1 = adjacent.iter().map(Spot::right).max().unwrap_or(0);
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
                                if let Some(bottom) = under.map(Spot::bottom).max() {
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
                Dir::Down => under.map(Spot::bottom).max(),
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
