//! Where the monitors go on the desktop: positions worked out from where the headset
//! shows them. Pure geometry, the same on every desktop.

use prostir_proto::NEXT;

use crate::platform::ActiveWindow;

/// A monitor to arrange: id, where its centre is seen (in the headset's shared pixel
/// scale, [`NEXT`] when unknown), width and height on the desktop.
#[derive(Debug, Clone, Copy)]
pub struct Seat {
    pub id: u8,
    pub center: i32,
    /// Vertical centre as seen, downwards ([`NEXT`]: unknown).
    pub middle: i32,
    pub w: i32,
    pub h: i32,
}

/// Desktop positions (top-left, logical pixels) when the headset said where each
/// monitor is seen, across and up: the desktop is what the eyes see. Starting from
/// `main`, each monitor joins the nearest one already placed on the side where they
/// meet in the headset, touching it, with the offset along that edge as seen (lined
/// up when within a few pixels), and always sharing enough of the edge for the mouse
/// to cross. Nothing overlaps.
pub fn arrange_free(seats: &[Seat], main: u8) -> Vec<(u8, i32, i32)> {
    const REACH: i32 = 200;
    const LINE_UP: i32 = 24;
    // As seen: top-left and size.
    let seen = |m: &Seat| (m.center - m.w / 2, m.middle - m.h / 2);
    let gaps = |a: &Seat, b: &Seat| {
        let ((ax, ay), (bx, by)) = (seen(a), seen(b));
        (
            (ax - (bx + b.w)).max(bx - (ax + a.w)),
            (ay - (by + b.h)).max(by - (ay + a.h)),
        )
    };
    let mut rest: Vec<Seat> = seats.to_vec();
    let Some(first) = rest
        .iter()
        .position(|m| m.id == main)
        .or_else(|| (!rest.is_empty()).then_some(0))
    else {
        return Vec::new();
    };
    let m0 = rest.remove(first);
    let (x0, y0) = seen(&m0);
    let mut placed: Vec<(Seat, i32, i32)> = vec![(m0, x0, y0)];
    let overlaps = |placed: &[(Seat, i32, i32)], m: &Seat, x: i32, y: i32| {
        placed
            .iter()
            .find(|(n, nx, ny)| x < nx + n.w && *nx < x + m.w && y < ny + n.h && *ny < y + m.h)
            .copied()
    };
    while !rest.is_empty() {
        // The monitor closest to one already placed, as seen.
        let (ri, pi) = (0..rest.len())
            .flat_map(|ri| (0..placed.len()).map(move |pi| (ri, pi)))
            .min_by_key(|&(ri, pi)| {
                let (gx, gy) = gaps(&rest[ri], &placed[pi].0);
                (
                    gx.max(gy).max(0),
                    (rest[ri].center - placed[pi].0.center).abs(),
                )
            })
            .expect("not empty");
        let m = rest.remove(ri);
        let (n, nx, ny) = placed[pi];
        let (gx, gy) = gaps(&m, &n);
        let ((mx, my), (sx, sy)) = (seen(&m), seen(&n));
        let across = gx > gy;
        let line_up = |v: i32, edges: [i32; 2]| {
            edges
                .into_iter()
                .find(|e| (v - e).abs() <= LINE_UP)
                .unwrap_or(v)
        };
        let (mut x, mut y) = if across {
            let x = if m.center >= n.center {
                nx + n.w
            } else {
                nx - m.w
            };
            let y = line_up(ny + (my - sy), [ny, ny + n.h - m.h]);
            let reach = REACH.min(m.h).min(n.h);
            (x, y.clamp(ny - m.h + reach, ny + n.h - reach))
        } else {
            let y = if m.middle >= n.middle {
                ny + n.h
            } else {
                ny - m.h
            };
            let x = line_up(nx + (mx - sx), [nx, nx + n.w - m.w]);
            let reach = REACH.min(m.w).min(n.w);
            (x.clamp(nx - m.w + reach, nx + n.w - reach), y)
        };
        // On top of another one: past it, the same way.
        for _ in 0..placed.len() {
            let Some((o, ox, oy)) = overlaps(&placed, &m, x, y) else {
                break;
            };
            if across {
                x = if m.center >= n.center {
                    ox + o.w
                } else {
                    ox - m.w
                };
            } else {
                y = if m.middle >= n.middle {
                    oy + o.h
                } else {
                    oy - m.h
                };
            }
        }
        placed.push((m, x, y));
    }
    let min_x = placed.iter().map(|p| p.1).min().unwrap_or(0);
    let min_y = placed.iter().map(|p| p.2).min().unwrap_or(0);
    placed
        .into_iter()
        .map(|(m, x, y)| (m.id, x - min_x, y - min_y))
        .collect()
}

/// Desktop positions (top-left, logical pixels) for monitors in rows, top to bottom,
/// as the headset sees them. The row with `main` is laid out touching, left to right;
/// every monitor of another row keeps its offset from the monitor of the row next to
/// it that it overlaps most, and touches the monitors under (or over) it, so the mouse
/// leaves one monitor where the eyes see the next. Nothing overlaps.
pub fn arrange(rows: &[Vec<Seat>], main: u8) -> Vec<(u8, i32, i32)> {
    // Unknown centres: right after the previous monitor of the row.
    let rows: Vec<Vec<Seat>> = rows
        .iter()
        .filter(|r| !r.is_empty())
        .map(|r| {
            let mut out: Vec<Seat> = Vec::with_capacity(r.len());
            for m in r {
                let mut m = *m;
                if m.center == NEXT {
                    m.center = out.last().map_or(0, |p| p.center + p.w / 2 + m.w / 2);
                }
                out.push(m);
            }
            out.sort_by_key(|m| m.center);
            out
        })
        .collect();
    let Some(main_row) = rows
        .iter()
        .position(|r| r.iter().any(|m| m.id == main))
        .or_else(|| (!rows.is_empty()).then_some(0))
    else {
        return Vec::new();
    };
    // Placed so far: (seat, x, y).
    let mut placed: Vec<(Seat, i32, i32)> = Vec::new();
    let tallest = rows[main_row].iter().map(|m| m.h).max().unwrap_or(0);
    // Heights as seen when known (a portrait monitor's top lined up with its
    // neighbour's stays so), else centred in the row; every monitor still overlaps the
    // one before it enough for the mouse to cross.
    let seen = rows[main_row].iter().all(|m| m.middle != NEXT);
    let mut x = 0;
    let mut previous: Option<(i32, i32)> = None; // (y, h)
    for m in &rows[main_row] {
        let mut y = if seen {
            m.middle - m.h / 2
        } else {
            (tallest - m.h) / 2
        };
        if let Some((py, ph)) = previous {
            const REACH: i32 = 200;
            let reach = REACH.min(m.h).min(ph);
            y = y.clamp(py - m.h + reach, py + ph - reach);
        }
        placed.push((*m, x, y));
        previous = Some((y, m.h));
        x += m.w;
    }
    let order: Vec<(usize, usize)> = (0..main_row)
        .rev()
        .map(|r| (r, r + 1))
        .chain((main_row + 1..rows.len()).map(|r| (r, r - 1)))
        .collect();
    for (r, next_to) in order {
        let above = r < next_to;
        let neighbours: Vec<(Seat, i32, i32)> = placed
            .iter()
            .copied()
            .filter(|(m, _, _)| rows[next_to].iter().any(|n| n.id == m.id))
            .collect();
        let mut row: Vec<(Seat, i32)> = Vec::new();
        for m in &rows[r] {
            let overlap = |a: &Seat| {
                (m.center + m.w / 2).min(a.center + a.w / 2)
                    - (m.center - m.w / 2).max(a.center - a.w / 2)
            };
            let anchor = neighbours.iter().max_by_key(|(a, _, _)| {
                let o = overlap(a);
                if o > 0 {
                    (1, o)
                } else {
                    (0, -(m.center - a.center).abs())
                }
            });
            let x = match anchor {
                Some((a, ax, _)) => ax + a.w / 2 + (m.center - a.center) - m.w / 2,
                None => 0,
            };
            row.push((*m, x));
        }
        // Left to right, side by side: no overlaps, and near-touching ones touch.
        row.sort_by_key(|(_, x)| *x);
        for i in 1..row.len() {
            let right = row[i - 1].1 + row[i - 1].0.w;
            let gap = row[i].1 - right;
            if gap < row[i].0.w.min(row[i - 1].0.w) / 10 {
                row[i].1 = right;
            }
        }
        // Reachable: each overlaps the row next to it by at least a bit.
        if let (Some(lo), Some(hi)) = (
            neighbours.iter().map(|(_, x, _)| *x).min(),
            neighbours.iter().map(|(n, x, _)| x + n.w).max(),
        ) {
            const REACH: i32 = 200;
            for (m, x) in &mut row {
                // Not clamp: next to a narrow monitor the bounds can cross, and clamp panics.
                *x = (*x).max(lo - m.w + REACH.min(m.w)).min(hi - REACH.min(m.w));
            }
            for i in 1..row.len() {
                let right = row[i - 1].1 + row[i - 1].0.w;
                if row[i].1 < right {
                    row[i].1 = right;
                }
            }
        }
        for (m, x) in row {
            // Touch the nearest monitors under (or over) it, overlapping none.
            let below: Vec<&(Seat, i32, i32)> = placed
                .iter()
                .filter(|(n, nx, _)| x < nx + n.w && *nx < x + m.w)
                .collect();
            let y = if above {
                below.iter().map(|(_, _, ny)| *ny).min().unwrap_or(0) - m.h
            } else {
                below.iter().map(|(n, _, ny)| ny + n.h).max().unwrap_or(0)
            };
            placed.push((m, x, y));
        }
    }
    let min_x = placed.iter().map(|p| p.1).min().unwrap_or(0);
    let min_y = placed.iter().map(|p| p.2).min().unwrap_or(0);
    placed
        .into_iter()
        .map(|(m, x, y)| (m.id, x - min_x, y - min_y))
        .collect()
}

/// Smallest window panel, in logical pixels.
const MIN_SIZE: (i32, i32) = (320, 200);

/// Output pixel size for a panel showing `w`: even numbers, at least [`MIN_SIZE`].
pub fn panel_size(w: &ActiveWindow) -> (u32, u32) {
    // At most what an output may be (an odd report from the compositor or a spoofed
    // one must not overflow).
    let px = |logical: i32, min: i32| {
        let p = (f64::from(logical.clamp(min, 8192)) * w.scale.clamp(0.25, 4.0)).round() as u32;
        (p.min(8192) + 1) & !1
    };
    (px(w.width, MIN_SIZE.0), px(w.height, MIN_SIZE.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prostir_proto::MIRROR_ID;

    #[test]
    fn panel_size_is_even_and_scaled() {
        let w = ActiveWindow {
            id: String::new(),
            width: 801,
            height: 100,
            scale: 1.5,
            output: String::new(),
        };
        assert_eq!(panel_size(&w), (1202, 300));
    }

    #[test]
    fn a_row_keeps_how_monitors_line_up_vertically() {
        // A landscape left of a portrait, tops lined up as seen.
        let mut a = seat(0, -1280, 2560, 1440);
        let mut b = seat(1, 720, 1440, 2560);
        a.middle = -560;
        b.middle = 0;
        let placed = arrange(&[vec![a, b]], 1);
        let y = |id: u8| placed.iter().find(|p| p.0 == id).unwrap().2;
        assert_eq!(y(0), y(1));
        // Unknown heights: centred, as before.
        let placed = arrange(
            &[vec![seat(0, -1280, 2560, 1440), seat(1, 720, 1440, 2560)]],
            1,
        );
        let y = |id: u8| placed.iter().find(|p| p.0 == id).unwrap().2;
        assert_eq!(y(0) - y(1), (2560 - 1440) / 2);
    }

    #[test]
    fn the_desktop_is_what_the_eyes_see() {
        // A portrait primary in the middle; a landscape on its left, tops lined up; a
        // landscape on its right, lower down; the laptop screen under the primary.
        let at = |id, center, middle, w, h| Seat {
            id,
            center,
            middle,
            w,
            h,
        };
        let seats = [
            at(0, -2000, -560, 2560, 1440),
            at(1, 0, 0, 1440, 2560),
            at(2, 1950, 300, 2560, 1440),
            at(MIRROR_ID, 0, 1800, 1920, 1080),
        ];
        let placed = arrange_free(&seats, 1);
        let get = |id| *placed.iter().find(|p| p.0 == id).unwrap();
        let (_, x0, y0) = get(0);
        let (_, x1, y1) = get(1);
        let (_, x2, y2) = get(2);
        let (_, xm, ym) = get(MIRROR_ID);
        // Side by side, touching; tops lined up on the left, lower on the right.
        assert_eq!(x0 + 2560, x1);
        assert_eq!(y0, y1);
        assert_eq!(x1 + 1440, x2);
        assert!(y2 > y1 + 200 && y2 < y1 + 2560 - 200, "y2 {y2}");
        // The laptop screen under the primary, touching it.
        assert_eq!(ym, y1 + 2560);
        assert!(xm < x1 + 1440 && xm + 1920 > x1);
    }

    fn seat(id: u8, center: i32, w: i32, h: i32) -> Seat {
        Seat {
            id,
            center,
            middle: NEXT,
            w,
            h,
        }
    }

    fn at(placed: &[(u8, i32, i32)], id: u8) -> (i32, i32) {
        let p = placed.iter().find(|p| p.0 == id).expect("placed");
        (p.1, p.2)
    }

    fn no_overlaps(placed: &[(u8, i32, i32)], sizes: &[(u8, i32, i32)]) {
        let size = |id: u8| {
            let s = sizes.iter().find(|s| s.0 == id).unwrap();
            (s.1, s.2)
        };
        for a in placed {
            for b in placed {
                if a.0 < b.0 {
                    let (aw, ah) = size(a.0);
                    let (bw, bh) = size(b.0);
                    let overlap =
                        a.1 < b.1 + bw && b.1 < a.1 + aw && a.2 < b.2 + bh && b.2 < a.2 + ah;
                    assert!(!overlap, "{a:?} overlaps {b:?}");
                }
            }
        }
    }

    /// A gap between two monitors in the main row closes, and the monitor seen above
    /// the right one stays above the right one (it drifted right of it before).
    #[test]
    fn above_the_right_monitor_stays_above_it() {
        let rows = vec![
            vec![seat(2, 1500, 2560, 1440)],
            vec![seat(0, -1600, 2560, 1440), seat(1, 1500, 2560, 1440)],
        ];
        let placed = arrange(&rows, 0);
        let (x0, y0) = at(&placed, 0);
        let (x1, y1) = at(&placed, 1);
        let (x2, y2) = at(&placed, 2);
        assert_eq!((x1, y1), (x0 + 2560, y0), "the main row touches");
        assert_eq!(x2, x1, "straight above the right one");
        assert_eq!(y2 + 1440, y1, "touching it");
    }

    /// Side monitors around a tall portrait one, and a monitor above the portrait one:
    /// it touches the portrait top, and overlaps nothing.
    #[test]
    fn above_a_portrait_monitor() {
        let sizes = [
            (0, 2560, 1440),
            (1, 1440, 2560),
            (2, 2560, 1440),
            (3, 1920, 1080),
        ];
        let rows = vec![
            vec![seat(3, 0, 1920, 1080)],
            vec![
                seat(0, -2000, 2560, 1440),
                seat(1, 0, 1440, 2560),
                seat(2, 2000, 2560, 1440),
            ],
        ];
        let placed = arrange(&rows, 1);
        no_overlaps(&placed, &sizes);
        let (x1, y1) = at(&placed, 1);
        let (x3, y3) = at(&placed, 3);
        assert_eq!(y3 + 1080, y1, "touches the portrait one");
        assert_eq!(x3 + 960, x1 + 720, "centred over it");
    }

    /// A monitor far off to the side of the row below still overlaps it a little, so
    /// the mouse can reach it.
    #[test]
    fn far_off_monitor_stays_reachable() {
        let rows = vec![
            vec![seat(1, 9000, 1920, 1080)],
            vec![seat(0, 0, 2560, 1440)],
        ];
        let placed = arrange(&rows, 0);
        let (x0, _) = at(&placed, 0);
        let (x1, _) = at(&placed, 1);
        assert!(x1 < x0 + 2560, "overlaps the monitor below");
    }

    /// Without seen positions (a fresh start): one row, side by side.
    #[test]
    fn unknown_positions_line_up() {
        let rows = vec![vec![
            seat(0, NEXT, 2560, 1440),
            seat(1, NEXT, 1440, 2560),
            seat(2, NEXT, 2560, 1440),
        ]];
        let placed = arrange(&rows, 1);
        assert_eq!(at(&placed, 0).0, 0);
        assert_eq!(at(&placed, 1).0, 2560);
        assert_eq!(at(&placed, 2).0, 4000);
        assert_eq!(at(&placed, 0).1, 560, "vertically centred on the tall one");
    }
}
