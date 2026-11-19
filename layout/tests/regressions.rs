//! The failures seen in the headset on 2026-11-19, and the default that must not change.

mod common;

use common::*;
use glam::Vec3;
use prostir_layout::circle::{self, ARC, Limits, MAX_RADIUS, Preset};
use prostir_layout::desk::{self, Boxes, Desk, SeenBox, Size};
use prostir_layout::rows::rows_of;
use prostir_layout::space::{Frame, Monitor, PixelSize, Side, covers};

/// 2026-11-20 in the headset (the rows of step 4, a393ef9): 2560x1440 | 1440x2560
/// primary | 2560x1440 in one row. While the owner moved them a little, the rows flipped
/// between [0, 1, 2], [1] / [0, 2] and [1, 2] / [0], and the desktop ended with the
/// portrait alone under the other two. Here every frame of such drags (up to 8 cm up or
/// down, up to 1.5° into a neighbour or away, a 2 mm tremor) is one row, worked out
/// afresh or from the frame before, and on the desktop the portrait stays between the
/// two, side by side.
#[test]
fn a_portrait_between_two_stays_in_their_row_while_they_move() {
    let start = portrait_between_two();
    let ids = [0u8, 1, 2];
    for seed in 0..40u64 {
        let mut rng = Rng::new(0x0f11_0000 + seed);
        let frames = small_drags(&start, &mut rng, 12, 1.5f32.to_radians(), 0.08);
        let mut previous: Option<Vec<Vec<u8>>> = None;
        let (mut fresh_rows, mut kept_rows) = (Vec::new(), Vec::new());
        for (i, f) in frames.iter().enumerate() {
            let room = f.room();
            let fresh = rows_of(&ids, &room, None);
            let kept = rows_of(&ids, &room, previous.as_deref());
            assert_eq!(kept, vec![vec![0, 1, 2]], "seed {seed} frame {i}");
            let desk_in = Desk {
                rows: kept.clone(),
                main: Some(1),
                ..Desk::default()
            };
            let plan = desk::plan(&desk_in, |k| f.size(k), &room);
            let (s0, s1, s2) = (
                spot(&plan, 0).unwrap(),
                spot(&plan, 1).unwrap(),
                spot(&plan, 2).unwrap(),
            );
            assert_eq!(s0.crossing(&s1), Some(Side::Right), "seed {seed} frame {i}");
            assert_eq!(s1.crossing(&s2), Some(Side::Right), "seed {seed} frame {i}");
            previous = Some(kept.clone());
            fresh_rows.push(fresh);
            kept_rows.push(kept);
        }
        assert_eq!(flips(&fresh_rows), 0, "seed {seed}");
        assert_eq!(flips(&kept_rows), 0, "seed {seed}");
    }
}

/// Three 2560 monitors, the default, arranged exactly as the headset of 0.2.0 did before
/// this crate took over: the same floats, bit for bit, for every preset but a wall with
/// the laptop screen (whose lid now hinges along the wall instead of turning out of it),
/// any primary, any curve. `data/three_2560.txt` holds what the headset's own
/// `arrange_curved` made (preset, primary, curve factor, laptop screen, monitor, then
/// the position, rotation and width as `f32` bits).
#[test]
fn three_2560_stay_exactly_as_today() {
    let three = [(0u8, 2560u32, 1440u32), (1, 2560, 1440), (2, 2560, 1440)];
    let mine: Vec<(u8, PixelSize)> = three
        .iter()
        .map(|m| (m.0, PixelSize::new(m.1, m.2)))
        .collect();
    let mut checked = 0;
    for line in include_str!("data/three_2560.txt").lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(f.len(), 13, "{line}");
        let preset = match f[0] {
            "arc" => Preset::Arc,
            "overhead" => Preset::Overhead,
            "wall" => Preset::Wall,
            other => panic!("{other}"),
        };
        let primary: Option<u8> = (f[1] != "-").then(|| f[1].parse().unwrap());
        let curve: Option<f32> = (f[2] != "-").then(|| f[2].parse().unwrap());
        let mirror = (f[3] != "-").then(|| {
            let (w, h) = f[3].split_once('x').unwrap();
            (
                MIRROR,
                PixelSize::new(w.parse().unwrap(), h.parse().unwrap()),
            )
        });
        let id: u8 = f[4].parse().unwrap();
        let v: Vec<f32> = f[5..]
            .iter()
            .map(|b| f32::from_bits(u32::from_str_radix(b, 16).unwrap()))
            .collect();
        let new = circle::arrange(preset, &mine, primary, mirror, curve, &Limits::default());
        let q = new[&id];
        assert_eq!(
            (q.position.to_array(), q.rotation.to_array(), q.width),
            ([v[0], v[1], v[2]], [v[3], v[4], v[5], v[6]], v[7]),
            "{preset:?} primary {primary:?} curve {curve:?} mirror {mirror:?}: monitor {id}"
        );
        checked += 1;
    }
    assert_eq!(checked, 300);
}

/// The six monitors of 2026-11-19 (three 3440, two 2560, a 1440x2560 primary, the
/// laptop screen): 474° at 1.1 m, so the headset of 0.2.0 overlapped them. Now two rows
/// at about 1.35 m, nothing covering anything as seen, and a desktop with every
/// invariant, every touch seen kept, no wider than its widest row and a monitor.
#[test]
fn the_six_monitors_make_two_rows_and_a_whole_desktop() {
    let six: Vec<(u8, PixelSize)> = [
        (3440, 1440),
        (2560, 1440),
        (1440, 2560),
        (3440, 1440),
        (2560, 1440),
        (3440, 1440),
    ]
    .iter()
    .enumerate()
    .map(|(i, &(w, h))| (i as u8, PixelSize::new(w, h)))
    .collect();
    let widths: Vec<f32> = six
        .iter()
        .map(|m| m.1.w as f32 * prostir_layout::space::METRES_PER_PIXEL)
        .collect();
    let limits = Limits::default();
    let (rows, radius) = circle::split(&widths, &limits);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        (1.3..=1.4).contains(&radius) && radius <= MAX_RADIUS,
        "{radius}"
    );
    for r in &rows {
        assert!(circle::row_arc(&widths[r.clone()], radius, 0.0) <= ARC);
    }
    let lid = (MIRROR, PixelSize::new(1920, 1080));
    let placed = circle::arrange(Preset::Arc, &six, Some(2), Some(lid), None, &limits);
    let scene = Scene {
        monitors: placed
            .iter()
            .map(|(&id, p)| Monitor {
                id,
                px: six.iter().find(|m| m.0 == id).map_or(lid.1, |m| m.1),
                placement: *p,
            })
            .collect(),
        frame: Frame::upright(Vec3::ZERO, 0.0),
        main: Some(2),
    };
    let corners: Vec<[Vec3; 4]> = scene
        .monitors
        .iter()
        .map(|m| m.placement.corners(m.height()))
        .collect();
    for (i, a) in corners.iter().enumerate() {
        for b in &corners[i + 1..] {
            assert!(!covers(a, b, 0.01));
        }
    }
    let (seen_rows, plan) = pipeline(&scene);
    assert_eq!(seen_rows, vec![vec![3, 4, 5], vec![0, 1, 2], vec![MIRROR]]);
    no_overlap(&plan).unwrap();
    at_origin(&plan).unwrap();
    connected(&plan).unwrap();
    rows_in_order(&plan, &seen_rows).unwrap();
    let c = continuity(&scene, &plan, desk::TOUCH_GAP, 0.25);
    assert!(c.lost.is_empty(), "{:?}", c.lost);
    assert!(c.side_ratio <= 1.0);
    let width = plan.iter().map(|s| s.right()).max().unwrap();
    let height = plan.iter().map(|s| s.bottom()).max().unwrap();
    assert!(width <= 3440 * 2 + 2560 + 3440, "{width}");
    assert!(height <= 2560 + 1440 + 1080, "{height}");
}

/// What the PC of 0.2.0 saved from that session (`monitors-six.txt`: rows and where the
/// old chain put each centre, from -1720 to 7600 across): planned from those numbers,
/// a whole desktop, no wider than the widest row and a monitor.
#[test]
fn the_saved_six_rows_make_a_whole_desktop() {
    // (id, width, height, centre, middle)
    type Saved = (u8, i32, i32, i32, i32);
    let rows: Vec<Vec<Saved>> = vec![
        vec![(0, 3440, 1440, -2872, -72)],
        vec![
            (1, 2560, 1440, -1280, 1368),
            (2, 1440, 2560, 720, 1280),
            (3, 3440, 1440, -2264, 6768),
            (4, 2560, 1440, -2000, 5328),
        ],
        vec![(5, 3440, 1440, -1800, 2808)],
    ];
    let boxes = Boxes::new(
        rows.iter()
            .flatten()
            .map(|&(id, w, h, c, m)| (id, SeenBox::from_wire(c, m, Size { w, h }), Size { w, h }))
            .collect(),
    );
    let desk_in = Desk {
        rows: rows
            .iter()
            .map(|r| r.iter().map(|e| e.0).collect())
            .collect(),
        main: Some(2),
        under_main: Some(MIRROR),
        ..Desk::default()
    };
    let size = |id: u8| {
        if id == MIRROR {
            return Some(Size { w: 1920, h: 1080 });
        }
        rows.iter()
            .flatten()
            .find(|e| e.0 == id)
            .map(|e| Size { w: e.1, h: e.2 })
    };
    let plan = desk::plan(&desk_in, size, &boxes);
    no_overlap(&plan).unwrap();
    at_origin(&plan).unwrap();
    connected(&plan).unwrap();
    rows_in_order(&plan, &desk_in.rows).unwrap();
    let pieces = desk::pieces_of(&desk_in, size, &boxes);
    rows_side_by_side(&plan, &pieces).unwrap();
    let width = plan.iter().map(|s| s.right()).max().unwrap();
    let height = plan.iter().map(|s| s.bottom()).max().unwrap();
    assert!(width <= 10_000 + 3440, "{width}: {}", describe_plan(&plan));
    assert!(
        height <= 1440 + 2560 + 1440 + 1080,
        "{height}: {}",
        describe_plan(&plan)
    );
}

/// The staircase KDE showed at 22:04 (each monitor touching the next at a corner only):
/// the rows the headset sent, the middles where KDE put each centre. Planned from them,
/// every piece of a row is side by side sharing what the mouse needs, nothing overlaps,
/// every monitor is in reach, the rows stay in order.
#[test]
fn the_staircase_of_22_04_becomes_rows() {
    let size = |id: u8| -> Option<Size> {
        Some(match id {
            0 | 2 => Size { w: 2560, h: 1440 },
            1 => Size { w: 1440, h: 2560 },
            3..=5 => Size { w: 3440, h: 1440 },
            MIRROR => Size { w: 1920, h: 1080 },
            _ => return None,
        })
    };
    // (id, centre as sent, middle as KDE then had it)
    let rows: Vec<Vec<(u8, i32, i32)>> = vec![
        vec![(4, -2024, 720), (5, 1000, 2160)],
        vec![(1, 720, 4160), (3, 4520, 2720)],
        vec![(0, -1280, 4536), (2, 2720, 4160)],
        vec![(MIRROR, 720, 5980)],
    ];
    let boxes = Boxes::new(
        rows.iter()
            .flatten()
            .map(|&(id, c, m)| {
                (
                    id,
                    SeenBox::from_wire(c, m, size(id).unwrap()),
                    size(id).unwrap(),
                )
            })
            .collect(),
    );
    let desk_in = Desk {
        rows: rows
            .iter()
            .map(|r| r.iter().map(|e| e.0).collect())
            .collect(),
        main: Some(1),
        ..Desk::default()
    };
    let plan = desk::plan(&desk_in, size, &boxes);
    no_overlap(&plan).unwrap();
    at_origin(&plan).unwrap();
    connected(&plan).unwrap();
    rows_in_order(&plan, &desk_in.rows).unwrap();
    let pieces = desk::pieces_of(&desk_in, size, &boxes);
    rows_side_by_side(&plan, &pieces).unwrap();
    // The two that touched only at a corner now share an edge.
    let (s4, s5) = (spot(&plan, 4).unwrap(), spot(&plan, 5).unwrap());
    assert_eq!(
        s4.crossing(&s5),
        Some(prostir_layout::space::Side::Right),
        "{}",
        describe_plan(&plan)
    );
}
