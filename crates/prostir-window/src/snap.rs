//! Where a monitor lands on the flat map when it is let go: against a side of another
//! monitor, slid along that side as far as the pointer says, pulled into line with
//! the neighbour's edge or centre when close, and never on top of another monitor.
//! The same rules move a monitor with the arrow keys. Desktop logical pixels.

/// A display on the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn at(self, (x, y): (i32, i32)) -> Self {
        Self { x, y, ..self }
    }

    fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

fn dist2(a: (i32, i32), b: (i32, i32)) -> i64 {
    let dx = (a.0 - b.0) as i64;
    let dy = (a.1 - b.1) as i64;
    dx * dx + dy * dy
}

/// `v` pulled onto the nearest of `lines` within `magnet`.
fn pull(v: i32, lines: [i32; 3], magnet: i32) -> i32 {
    lines
        .into_iter()
        .filter(|l| (l - v).abs() <= magnet)
        .min_by_key(|l| (l - v).abs())
        .unwrap_or(v)
}

/// The top-left where a monitor of `size` lands when let go with its top-left at
/// `want`, touching one of `others`. None: there is no free place next to any of
/// them (the caller keeps it where it was).
pub fn snap(
    others: &[Rect],
    size: (i32, i32),
    want: (i32, i32),
    magnet: i32,
) -> Option<(i32, i32)> {
    let (w, h) = size;
    if others.is_empty() {
        return Some(want);
    }
    let mut best: Option<((i32, i32), i64)> = None;
    let mut consider = |p: (i32, i32)| {
        let r = Rect {
            x: p.0,
            y: p.1,
            w,
            h,
        };
        if others.iter().any(|o| r.overlaps(o)) {
            return;
        }
        let d = dist2(p, want);
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((p, d));
        }
    };
    for o in others {
        // Left or right of `o`: any height where the sides still share an eighth of
        // the shorter one, so the mouse has a real edge to cross, not a corner.
        let ty = (h.min(o.h) / 8).max(1);
        let ys = [o.y, o.y + (o.h - h) / 2, o.y + o.h - h];
        let y = pull(want.1.clamp(o.y - h + ty, o.y + o.h - ty), ys, magnet);
        let tx = (w.min(o.w) / 8).max(1);
        let xs = [o.x, o.x + (o.w - w) / 2, o.x + o.w - w];
        let x = pull(want.0.clamp(o.x - w + tx, o.x + o.w - tx), xs, magnet);
        for side_x in [o.x - w, o.x + o.w] {
            consider((side_x, y));
            for a in ys {
                consider((side_x, a));
            }
        }
        for side_y in [o.y - h, o.y + o.h] {
            consider((x, side_y));
            for a in xs {
                consider((a, side_y));
            }
        }
    }
    best.map(|(p, _)| p)
}

/// One step of a monitor with the arrow keys: the nearest place in that direction
/// (`dx`, `dy` of -1, 0 or 1) that differs from where it is.
pub fn step(others: &[Rect], me: Rect, (dx, dy): (i32, i32), magnet: i32) -> Option<(i32, i32)> {
    let quarter = ((me.w * dx.abs() + me.h * dy.abs()) / 4).max(1);
    (1..=12).find_map(|k| {
        let want = (me.x + dx * quarter * k, me.y + dy * quarter * k);
        let p = snap(others, (me.w, me.h), want, magnet)?;
        let moved = (p.0 - me.x) * dx + (p.1 - me.y) * dy;
        (p != (me.x, me.y) && moved > 0).then_some(p)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    const WIDE: Rect = Rect {
        x: 0,
        y: 0,
        w: 2560,
        h: 1440,
    };

    #[test]
    fn lands_against_the_nearest_side() {
        assert_eq!(snap(&[WIDE], (1920, 1080), (2700, 20), 0), Some((2560, 20)));
        assert_eq!(
            snap(&[WIDE], (1920, 1080), (-2000, 300), 0),
            Some((-1920, 300))
        );
        assert_eq!(
            snap(&[WIDE], (1920, 1080), (300, 1500), 0),
            Some((300, 1440))
        );
    }

    #[test]
    fn slides_along_a_side_but_keeps_touching() {
        // Below the right side's end: it stays at the right, still sharing an eighth
        // of its height (1080 / 8 = 135).
        assert_eq!(
            snap(&[WIDE], (1920, 1080), (3000, 1300), 0),
            Some((2560, 1300))
        );
        assert_eq!(
            snap(&[WIDE], (1920, 1080), (3000, 2000), 0),
            Some((2560, 1305))
        );
    }

    #[test]
    fn is_pulled_into_line_when_close() {
        assert_eq!(snap(&[WIDE], (1920, 1080), (2600, 30), 60), Some((2560, 0)));
        // Centres: (1440 - 1080) / 2 = 180.
        assert_eq!(
            snap(&[WIDE], (1920, 1080), (2600, 200), 60),
            Some((2560, 180))
        );
        assert_eq!(
            snap(&[WIDE], (1920, 1080), (2600, 270), 60),
            Some((2560, 270))
        );
    }

    #[test]
    fn never_lands_on_another_monitor() {
        let others = [WIDE, r(2560, 0, 1920, 1080)];
        for want in [
            (2600, 0),
            (1000, 800),
            (2500, -100),
            (4000, 1200),
            (-50, -50),
        ] {
            let p = snap(&others, (1920, 1080), want, 40).unwrap();
            let me = r(p.0, p.1, 1920, 1080);
            assert!(others.iter().all(|o| !me.overlaps(o)), "{want:?} -> {p:?}");
            let touches = others.iter().any(|o| {
                let side = r(o.x - 1, o.y, o.w + 2, o.h);
                let ends = r(o.x, o.y - 1, o.w, o.h + 2);
                me.overlaps(&side) || me.overlaps(&ends)
            });
            assert!(touches, "{want:?} -> {p:?} touches nothing");
        }
    }

    #[test]
    fn alone_it_stays_where_it_was_let_go() {
        assert_eq!(snap(&[], (1920, 1080), (17, 4), 40), Some((17, 4)));
    }

    #[test]
    fn arrow_keys_move_to_the_next_place() {
        let others = [WIDE];
        let me = r(2560, 0, 1920, 1080);
        // Down: slides down the right side.
        let p = step(&others, me, (0, 1), 0).unwrap();
        assert_eq!(p, (2560, 270));
        // Left: the other monitor is in the way, so it goes over it.
        let p = step(&others, me, (-1, 0), 0).unwrap();
        assert!(p.0 < me.x && !me.at(p).overlaps(&WIDE), "{p:?}");
        // Up from the top edge: above the neighbour's right part.
        let p = step(&others, me, (0, -1), 0).unwrap();
        assert!(p.1 < 0, "{p:?}");
    }
}
