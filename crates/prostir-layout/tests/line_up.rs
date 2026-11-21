//! "Line up" takes the crookedness out of each row and nothing more: the invariants L1
//! to L9 of `plans/layout-math.md` over thousands of seeded arrangements, and the rows
//! pitched back by 10°, 20° and 35° the owner lined up on 2026-11-20.

mod common;

use std::collections::BTreeMap;

use common::*;
use glam::{Quat, Vec3};
use prostir_layout::circle::{self, Limits, Preset};
use prostir_layout::space::{Frame, Monitor, PixelSize, Placement};
use prostir_layout::tidy::{glued_pairs, line_up};

/// This crate's "Line up", rows worked out afresh.
fn mine(scene: &Scene) -> Option<BTreeMap<u8, Placement>> {
    let rows = rows_now(scene);
    let lid = scene.get(MIRROR).map(|m| m.id);
    Some(line_up(
        &scene.monitors,
        &scene.frame,
        scene.main,
        Some(&rows),
        lid,
    ))
}

#[test]
fn every_invariant_of_lining_up_holds() {
    let checks = measure_line_up(&mine, cases(600), 0x11e0_0001);
    for c in &checks {
        eprintln!("{}", c.report());
    }
    for c in &checks {
        // Hard for every scene: turning commutes, symmetry, a tilt kept, presets kept.
        // The rest hold but for the measured exceptions of `plans/layout-math.md` (a
        // nudge across a decision, hand scenes that wrap round or cover one another):
        // in percent of the scenes.
        let percent = match &c.name[..2] {
            "L1" | "L3" | "L7" | "L8" => 0,
            "L6" | "L2" if c.name.contains("direction") => 1,
            "L4" => 3,
            "L5" => 4,
            _ => 5,
        };
        assert!(c.broken * 100 <= c.total * percent, "{}", c.report());
        assert!(c.skipped * 4 <= c.total + c.skipped, "{}", c.report());
    }
}

/// The owner's rows of 2026-11-20 (three 2560x1440; 2560x1440 | 1440x2560 primary |
/// 2560x1440) pitched back as a whole by 10°, 20° and 35° about the eye, as for a
/// reclined pose, then disturbed by a hand (a gap, a step, a monitor turned and hanging
/// crooked): "Line up" gives the pitched row back, seams closed, the tilt kept.
#[test]
fn a_row_pitched_back_by_10_20_35_degrees_stays_pitched() {
    for sizes in [
        [(2560u32, 1440u32), (2560, 1440), (2560, 1440)],
        [(2560, 1440), (1440, 2560), (2560, 1440)],
    ] {
        let px: Vec<(u8, PixelSize)> = sizes
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| (i as u8, PixelSize::new(w, h)))
            .collect();
        let placed = circle::arrange(Preset::Arc, &px, Some(1), None, None, &Limits::default());
        let frame = Frame::upright(Vec3::new(0.3, 1.2, -0.4), 0.4);
        let upright = Scene {
            monitors: px
                .iter()
                .map(|&(id, px)| Monitor {
                    id,
                    px,
                    placement: frame.place(placed[&id]),
                })
                .collect(),
            frame,
            main: Some(1),
        };
        for back in [10.0f32, 20.0, 35.0] {
            let turn =
                frame.turn * Quat::from_rotation_x(-back.to_radians()) * frame.turn.inverse();
            let pitched = turned(&upright, turn);
            let tidy = placements(&pitched);
            // As it is: nothing moves.
            let out = mine(&pitched).unwrap();
            assert!(farthest(&pitched, &tidy, &out) < 1e-3, "{back}°: moved");
            // Disturbed: the left one 3 cm away, the right one 4 cm up and turned 2°,
            // the middle one hanging 5° crooked.
            let mut hand = lined(&pitched, &tidy);
            let axis = turn * (frame.turn * Vec3::Y);
            let left = turn * (frame.turn * Vec3::NEG_X);
            hand.monitors[0].placement.position += left * 0.03;
            let p = &mut hand.monitors[2].placement;
            p.position += axis * 0.04;
            p.rotation = (Quat::from_axis_angle(axis, 2f32.to_radians()) * p.rotation).normalize();
            let p = &mut hand.monitors[1].placement;
            p.rotation =
                (Quat::from_axis_angle(p.normal(), 5f32.to_radians()) * p.rotation).normalize();
            let out = mine(&hand).unwrap();
            // The tilt is kept: every monitor's up is the pitched row's.
            for m in &hand.monitors {
                let up = out[&m.id].rotation * Vec3::Y;
                assert!(
                    up.angle_between(axis).to_degrees() < 0.1,
                    "{back}°: {} leans",
                    m.id
                );
            }
            // Side by side: each right edge on its neighbour's left edge.
            for (a, b) in [(0u8, 1u8), (1, 2)] {
                let ha = hand.get(a).unwrap().height();
                let hb = hand.get(b).unwrap().height();
                let (ca, cb) = (out[&a].corners(ha), out[&b].corners(hb));
                // Mid-points of the facing edges.
                let ra = (ca[1] + ca[2]) / 2.0;
                let lb = (cb[0] + cb[3]) / 2.0;
                assert!(
                    ra.distance(lb) < 1e-3,
                    "{back}°: seam {a}-{b} open by {}",
                    ra.distance(lb)
                );
            }
        }
    }
}

/// L5 again, telling apart the nudges that change which monitors "Line up" keeps glued
/// (see `tidy::glued_pairs`): those cross a decision, where a jump is what the decision
/// is. Of the others, no more break L5 than L5 allows over all of them.
#[test]
fn a_nudge_across_no_glue_moves_little() {
    let mut rng = Rng::new(0x11e0_0005);
    let (mut same, mut broken, mut crossing, mut crossing_broken) = (0, 0, 0, 0);
    for _ in 0..cases(600) {
        let scene = hand_scene(&mut rng);
        let i = rng.below(scene.monitors.len());
        let d = loop {
            let v = Vec3::new(
                rng.float(-1.0, 1.0),
                rng.float(-1.0, 1.0),
                rng.float(-1.0, 1.0),
            );
            if (0.1..=1.0).contains(&v.length()) {
                break v.normalize() * 0.01;
            }
        };
        let mut nudged = lined(&scene, &placements(&scene));
        nudged.monitors[i].placement.position += d;
        let (rows, rows_nudged) = (rows_now(&scene), rows_now(&nudged));
        if rows != rows_nudged {
            continue;
        }
        let lid = scene.get(MIRROR).map(|m| m.id);
        let run = |s: &Scene, rows: &[Vec<u8>]| {
            (
                line_up(&s.monitors, &s.frame, s.main, Some(rows), lid),
                glued_pairs(&s.monitors, &s.frame, s.main, Some(rows), lid),
            )
        };
        let ((a, glued_a), (b, glued_b)) = (run(&scene, &rows), run(&nudged, &rows_nudged));
        let moved = scene
            .monitors
            .iter()
            .map(|m| a[&m.id].position.distance(b[&m.id].position))
            .fold(0.0f32, f32::max);
        if glued_a == glued_b {
            same += 1;
            broken += usize::from(moved > 0.012);
        } else {
            crossing += 1;
            crossing_broken += usize::from(moved > 0.012);
        }
    }
    eprintln!(
        "L5 across no glue: broken in {broken} of {same}; across a glue: {crossing_broken} of {crossing}"
    );
    assert!(broken * 100 <= same * 4, "{broken} of {same}");
}

/// Reported (`--ignored`): how long "Line up" takes over the hand scenes of T1 to T5
/// (800 of them): the median, the 95th percentile and the longest, in microseconds.
#[test]
#[ignore = "a report, not a check"]
fn report_line_up_timings() {
    let mut rng = Rng::new(0x3e7a_0007);
    let mut took: Vec<u128> = Vec::with_capacity(800);
    for _ in 0..800 {
        let scene = hand_scene(&mut rng);
        let ids: Vec<u8> = scene.monitors.iter().map(|m| m.id).collect();
        let rows = prostir_layout::rows::rows_of(&ids, &scene.room(), None);
        let lid = scene.get(MIRROR).map(|m| m.id);
        let start = std::time::Instant::now();
        let done = line_up(&scene.monitors, &scene.frame, scene.main, Some(&rows), lid);
        took.push(start.elapsed().as_micros());
        std::hint::black_box(done);
    }
    took.sort_unstable();
    let at = |q: f64| took[((took.len() - 1) as f64 * q) as usize];
    eprintln!(
        "line up over {} hand scenes: median {} us, p95 {} us, longest {} us",
        took.len(),
        at(0.5),
        at(0.95),
        took[took.len() - 1]
    );
}
