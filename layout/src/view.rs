//! The desk plan's [`View`] of the room: how two monitors stand along the edge they
//! share, from where they float, as the eye sees them (what the headset knows).
//!
//! Along an edge a monitor's pixels are not spread evenly over the angle it takes: its
//! middle is nearer the eye than its ends. So the point on an edge seen at a given
//! angle is found on the edge itself (by bisection), and so is the pixel there.

use glam::{DVec3, Vec3};

use crate::desk::{NOISE, Relation, View};
use crate::space::{Frame, Monitor, Seen, Side, Touch, azimuth, elevation, see, unwrap};

/// The room as the desk plan sees it.
#[derive(Debug, Clone)]
pub struct Room<K> {
    monitors: Vec<Shown<K>>,
}

#[derive(Debug, Clone)]
struct Shown<K> {
    id: K,
    /// Corners in the frame: top left, top right, bottom right, bottom left.
    corners: [DVec3; 4],
    /// Pixels across and down.
    px: (f64, f64),
    seen: Seen<K>,
}

/// An edge as seen: a segment in the frame and the pixels along it.
#[derive(Debug, Clone, Copy)]
struct Edge {
    from: DVec3,
    to: DVec3,
    pixels: f64,
    /// Runs down (the pixel 0 is at `to`, the top) rather than across.
    down: bool,
}

impl<K: Copy + PartialEq> Room<K> {
    /// The monitors as seen from the frame's eye; their stream sizes are their sizes on
    /// the desktop (the headset's plan).
    pub fn new(frame: &Frame, monitors: &[Monitor<K>]) -> Self {
        let seen = see(frame, monitors);
        let monitors = monitors
            .iter()
            .zip(seen)
            .map(|(m, s)| Shown {
                id: m.id,
                corners: m
                    .placement
                    .corners(m.height())
                    .map(|c: Vec3| frame.to_local(c).as_dvec3()),
                px: (f64::from(m.px.w), f64::from(m.px.h)),
                seen: s,
            })
            .collect();
        Self { monitors }
    }

    /// Every monitor as seen.
    pub fn seen(&self) -> Vec<Seen<K>> {
        self.monitors.iter().map(|m| m.seen).collect()
    }

    fn get(&self, id: K) -> Option<&Shown<K>> {
        self.monitors.iter().find(|m| m.id == id)
    }

    /// Points along the edge `a` and `b` share as seen (`b` on `side` of `a`), `n` of
    /// them spread over the middle 90 % of it: for each, the pixel along `a`'s edge and
    /// the pixel along `b`'s (from the top, or from the left) seen at the same angle
    /// along the edge. Where the mouse leaves `a` at the first, it should enter `b` at
    /// the second.
    pub fn seam(&self, a: K, b: K, side: Side, n: usize) -> Vec<(f64, f64)> {
        let (Some(ma), Some(mb)) = (self.get(a), self.get(b)) else {
            return Vec::new();
        };
        let ea = Self::edge(ma, side);
        let eb = Self::edge(mb, side.opposite());
        let (ca, cb) = (ma.seen.center_x(), mb.seen.center_x());
        let range = |e: &Edge, around: f64| {
            let (x, y) = (e.angle(0.0, around), e.angle(1.0, around));
            (x.min(y), x.max(y))
        };
        let ((a0, a1), (b0, b1)) = (range(&ea, ca), range(&eb, cb));
        let (lo, hi) = (a0.max(b0), a1.min(b1));
        if hi <= lo {
            return Vec::new();
        }
        (0..n)
            .map(|j| {
                let f = 0.05 + 0.9 * (j as f64 + 0.5) / n as f64;
                let angle = lo + f * (hi - lo);
                (ea.pixel(ea.find(angle, ca)), eb.pixel(eb.find(angle, cb)))
            })
            .collect()
    }

    /// The angle seen along `k`'s edge on `side` (elevation for a side edge, azimuth for
    /// a top or bottom one, radians, cut where [`see`] cuts) at `pixel` along it (from
    /// the top, or the left).
    pub fn angle_along(&self, k: K, side: Side, pixel: f64) -> Option<f64> {
        let m = self.get(k)?;
        let e = Self::edge(m, side);
        let t = if e.down {
            1.0 - pixel / e.pixels
        } else {
            pixel / e.pixels
        };
        Some(e.angle(t, m.seen.center_x()))
    }

    /// Metres per pixel along `k`'s edge on `side`.
    pub fn metres_per_pixel(&self, k: K, side: Side) -> Option<f64> {
        let m = self.get(k)?;
        let e = Self::edge(m, side);
        Some((e.to - e.from).length() / e.pixels)
    }

    /// The pairs that meet as seen, from their edges: `b` on `side` of `a`, the facing
    /// edges at most `gap` apart (a fraction of `a`'s size across them; overlapping
    /// counts), sharing at least `seam` of the shorter edge. Of a pair seen both beside
    /// and over the other, the way they are apart (the larger gap) counts.
    pub fn touches(&self, gap: f64, seam: f64) -> Vec<Touch<K>> {
        let mut out = Vec::new();
        for (i, a) in self.monitors.iter().enumerate() {
            for b in &self.monitors[i + 1..] {
                let apart = |side: Side| {
                    let rel = self.relate(a.id, b.id, side).filter(|r| r.shared > 0.0)?;
                    Some((side, rel.apart, rel.seam))
                };
                let across = apart(Side::Right).or_else(|| apart(Side::Left));
                let up = apart(Side::Above).or_else(|| apart(Side::Below));
                let way = match (across, up) {
                    (Some(x), Some(y)) => Some(if x.1 >= y.1 { x } else { y }),
                    (x, y) => x.or(y),
                };
                if let Some((side, g, s)) = way
                    && g <= gap
                    && s >= seam
                {
                    out.push(Touch {
                        a: a.id,
                        b: b.id,
                        side,
                        seam: s,
                    });
                }
            }
        }
        out
    }

    /// The azimuth of `k`'s middle as seen (cut where [`see`] cuts).
    pub fn center_x(&self, k: K) -> Option<f64> {
        Some(self.get(k)?.seen.center_x())
    }

    /// The edge of `m` on `side` (as a segment, pixel 0 at its start).
    fn edge(m: &Shown<K>, side: Side) -> Edge {
        let [tl, tr, br, bl] = m.corners;
        match side {
            Side::Right => Edge {
                from: br,
                to: tr,
                pixels: m.px.1,
                down: true,
            },
            Side::Left => Edge {
                from: bl,
                to: tl,
                pixels: m.px.1,
                down: true,
            },
            Side::Above => Edge {
                from: tl,
                to: tr,
                pixels: m.px.0,
                down: false,
            },
            Side::Below => Edge {
                from: bl,
                to: br,
                pixels: m.px.0,
                down: false,
            },
        }
    }
}

impl Edge {
    fn at(&self, t: f64) -> DVec3 {
        self.from + (self.to - self.from) * t
    }

    /// The angle along the edge at `t`: elevation for an edge that runs down, else
    /// azimuth (unwrapped around `around`).
    fn angle(&self, t: f64, around: f64) -> f64 {
        let p = self.at(t);
        if self.down {
            elevation(p)
        } else {
            unwrap(azimuth(p), around)
        }
    }

    /// The angle across the edge at `t` (azimuth for an edge that runs down, else
    /// elevation).
    fn across(&self, t: f64, around: f64) -> f64 {
        let p = self.at(t);
        if self.down {
            unwrap(azimuth(p), around)
        } else {
            elevation(p)
        }
    }

    /// Where along the edge (0..=1) the angle along it is `angle` (clamped to the ends).
    fn find(&self, angle: f64, around: f64) -> f64 {
        let (a0, a1) = (self.angle(0.0, around), self.angle(1.0, around));
        let rising = a1 >= a0;
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        for _ in 0..60 {
            let mid = (lo + hi) / 2.0;
            let below = self.angle(mid, around) < angle;
            if below == rising {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    }

    /// The pixel along the edge at `t`, counted from the top (an edge that runs down) or
    /// from the left.
    fn pixel(&self, t: f64) -> f64 {
        if self.down {
            (1.0 - t) * self.pixels
        } else {
            t * self.pixels
        }
    }
}

impl<K: Copy + PartialEq> View<K> for Room<K> {
    fn relate(&self, a: K, b: K, side: Side) -> Option<Relation> {
        let (ma, mb) = (self.get(a)?, self.get(b)?);
        // `b` must be on that side of `a` at all.
        let on_side = match side {
            Side::Right => mb.seen.center_x() > ma.seen.center_x(),
            Side::Left => mb.seen.center_x() < ma.seen.center_x(),
            Side::Above => mb.seen.center_y() > ma.seen.center_y(),
            Side::Below => mb.seen.center_y() < ma.seen.center_y(),
        };
        if !on_side {
            return None;
        }
        let ea = Self::edge(ma, side);
        let eb = Self::edge(mb, side.opposite());
        // Each edge's azimuths around its own monitor's middle: every middle is already
        // cut where `see` cuts, so the two edges are comparable.
        let (ca, cb) = (ma.seen.center_x(), mb.seen.center_x());
        let range = |e: &Edge, around: f64| {
            let (x, y) = (e.angle(0.0, around), e.angle(1.0, around));
            (x.min(y), x.max(y))
        };
        let ((a0, a1), (b0, b1)) = (range(&ea, ca), range(&eb, cb));
        let (lo, hi) = (a0.max(b0), a1.min(b1));
        let m = if hi > lo {
            (lo + hi) / 2.0
        } else {
            (a0 + a1 + b0 + b1) / 4.0
        };
        let (ta, tb) = (ea.find(m, ca), eb.find(m, cb));
        let shared = if hi > lo {
            (ea.pixel(ea.find(hi, ca)) - ea.pixel(ea.find(lo, ca))).abs()
        } else {
            0.0
        };
        // Across the edge, from `a` outwards, in `a`'s pixels.
        let (across_a, across_b) = (ea.across(ta, ca), eb.across(tb, cb));
        let outwards = match side {
            Side::Right | Side::Above => across_b - across_a,
            Side::Left | Side::Below => across_a - across_b,
        };
        let scale = match side {
            Side::Right | Side::Left => ma.px.0 / ma.seen.width(),
            Side::Above | Side::Below => ma.px.1 / ma.seen.height(),
        };
        let size_across = match side {
            Side::Right | Side::Left => ma.seen.width(),
            Side::Above | Side::Below => ma.seen.height(),
        };
        Some(Relation {
            offset: ea.pixel(ta) - eb.pixel(tb),
            at: ea.pixel(ta),
            shared,
            seam: ((hi - lo) / (a1 - a0).min(b1 - b0).max(1e-12)).clamp(0.0, 1.0),
            gap: outwards * scale,
            apart: outwards / size_across,
        })
    }

    fn between(&self, a: K, b: K) -> bool {
        let (Some(ma), Some(mb)) = (self.get(a), self.get(b)) else {
            return false;
        };
        let (sa, sb) = (ma.seen, mb.seen);
        let (l, r) = (sa.right, sb.left);
        let (lo, hi) = (sa.bottom.max(sb.bottom), sa.top.min(sb.top));
        if r - l <= NOISE * sa.width().min(sb.width()) || hi <= lo {
            return false;
        }
        self.monitors.iter().any(|c| {
            let s = c.seen;
            c.id != a
                && c.id != b
                && s.right.min(r) - s.left.max(l) > NOISE * (r - l)
                && s.top.min(hi) - s.bottom.max(lo) > NOISE * (hi - lo)
        })
    }

    fn middle(&self, k: K) -> Option<(f64, f64)> {
        self.get(k).map(|m| (m.seen.center_x(), m.seen.center_y()))
    }
}
