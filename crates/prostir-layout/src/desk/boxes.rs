//! What the headset sees, as boxes: a [`View`] from where the monitors are seen.

use super::*;

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

    fn height(&self, k: K) -> Option<f64> {
        self.get(k).map(|(b, _)| b.h)
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
