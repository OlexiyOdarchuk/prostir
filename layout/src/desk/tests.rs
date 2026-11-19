use std::collections::BTreeMap;

use super::*;

/// A small seeded generator (splitmix64), so a failing case comes back the same.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// `0..n`.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// `lo..=hi`.
    pub(crate) fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo + 1) as u64) as i32
    }

    pub(crate) fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

/// Stream sizes in pixels and output scales of the tests.
const PIXELS: [(i32, i32); 6] = [
    (1920, 1080),
    (2560, 1440),
    (3440, 1440),
    (5120, 1440),
    (3840, 2160),
    (1440, 2560),
];
const SCALES: [f64; 4] = [1.0, 1.25, 1.5, 2.0];

fn random_size(rng: &mut Rng) -> Size {
    let (w, h) = PIXELS[rng.below(PIXELS.len())];
    let scale = SCALES[rng.below(SCALES.len())];
    let logical = |px: i32| (f64::from(px) / scale).round() as i32;
    Size {
        w: logical(w),
        h: logical(h),
    }
}

/// A seen position: none, an ordinary one, or nonsense.
fn random_seen(rng: &mut Rng, ordinary: i32) -> Option<i32> {
    match rng.below(10) {
        0..2 => None,
        2..8 => Some(rng.range(-ordinary, ordinary)),
        _ => Some(rng.range(-FAR, FAR)),
    }
}

/// Monitors 0.., the laptop screen 100, other real displays 101.., window panels 10...
type Case = (Desk<u8>, BTreeMap<u8, Size>);

/// Up to 10 monitors in up to 4 rows, anything seen anywhere, a laptop screen and up
/// to 3 more real displays and 3 window panels.
fn random_case(rng: &mut Rng) -> Case {
    let monitors = 1 + rng.below(10);
    let mut sizes = BTreeMap::new();
    let mut rows: Vec<Vec<Entry<u8>>> = vec![Vec::new(); 1 + rng.below(monitors.min(4))];
    for id in 0..monitors as u8 {
        sizes.insert(id, random_size(rng));
        let r = rng.below(rows.len());
        rows[r].push(Entry {
            id,
            center: random_seen(rng, 8000),
            middle: random_seen(rng, 3000),
        });
    }
    let mut desk = Desk {
        rows,
        main: rng
            .chance(85)
            .then(|| rng.below(monitors) as u8)
            .or(rng.chance(50).then_some(200)),
        ..Desk::default()
    };
    if rng.chance(50) {
        sizes.insert(100, random_size(rng));
        // Placed by the headset now and then.
        if rng.chance(30) {
            let r = rng.below(desk.rows.len());
            desk.rows[r].push(Entry {
                id: 100,
                center: random_seen(rng, 8000),
                middle: random_seen(rng, 3000),
            });
        }
        desk.under_main = Some(100);
    }
    for id in 101..101 + rng.below(4) as u8 {
        sizes.insert(id, random_size(rng));
        desk.bottom.push(id);
    }
    for id in 10..10 + rng.below(4) as u8 {
        sizes.insert(id, random_size(rng));
        desk.column.push(id);
    }
    (desk, sizes)
}

fn run(case: &Case) -> Vec<Spot<u8>> {
    plan(&case.0, |id| case.1.get(&id).copied())
}

fn spot_of(plan: &[Spot<u8>], id: u8) -> Spot<u8> {
    *plan.iter().find(|p| p.id == id).expect("placed")
}

fn no_overlaps(plan: &[Spot<u8>]) -> Result<(), String> {
    for (i, a) in plan.iter().enumerate() {
        for b in &plan[i + 1..] {
            if a.across(b) && a.along(b) {
                return Err(format!("{a:?} overlaps {b:?}"));
            }
        }
    }
    Ok(())
}

/// Every output reaches every other through edges the mouse can cross.
fn reachable(plan: &[Spot<u8>]) -> Result<(), String> {
    let Some(first) = plan.first() else {
        return Ok(());
    };
    let crosses = |a: &Spot<u8>, b: &Spot<u8>| {
        let side = a.right() == b.x || b.right() == a.x;
        let need = if side {
            reach(a.h, b.h)
        } else {
            reach(a.w, b.w)
        };
        a.shared_edge(b) >= need.max(1)
    };
    let mut reached = vec![first.id];
    let mut i = 0;
    while i < reached.len() {
        let a = spot_of(plan, reached[i]);
        for b in plan {
            if !reached.contains(&b.id) && crosses(&a, b) {
                reached.push(b.id);
            }
        }
        i += 1;
    }
    if reached.len() == plan.len() {
        Ok(())
    } else {
        let lost: Vec<u8> = plan
            .iter()
            .map(|p| p.id)
            .filter(|id| !reached.contains(id))
            .collect();
        Err(format!("{lost:?} cannot be reached from {}", first.id))
    }
}

/// The plan's rows again, with what the plan shows as where they are seen.
fn seen_again(case: &Case, plan: &[Spot<u8>]) -> Case {
    let mut again = case.clone();
    for e in again.0.rows.iter_mut().flatten() {
        if let Some(p) = plan.iter().find(|p| p.id == e.id) {
            e.center = Some(p.x + p.w / 2);
            e.middle = Some(p.y + p.h / 2);
        }
    }
    again
}

/// What every plan holds, whatever it was given.
fn check(case: &Case) -> Result<Vec<Spot<u8>>, String> {
    let (desk, sizes) = case;
    let plan = run(case);
    let mut wanted: Vec<u8> = desk
        .rows
        .iter()
        .flatten()
        .map(|e| e.id)
        .chain(desk.under_main)
        .chain(desk.bottom.iter().copied())
        .chain(desk.column.iter().copied())
        .filter(|id| sizes.contains_key(id))
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut got: Vec<u8> = plan.iter().map(|p| p.id).collect();
    got.sort_unstable();
    if got != wanted {
        return Err(format!("placed {got:?}, not {wanted:?}"));
    }
    no_overlaps(&plan)?;
    if plan.iter().map(|p| p.x).min() != Some(0) || plan.iter().map(|p| p.y).min() != Some(0) {
        return Err("does not start at 0,0".into());
    }
    // Without the window panels, which hang right of everything.
    let laid: Vec<Spot<u8>> = plan
        .iter()
        .copied()
        .filter(|p| !desk.column.contains(&p.id))
        .collect();
    reachable(&laid).map_err(|e| format!("{e}\nplan {plan:?}"))?;
    // A row above another is above it.
    let row_of = |id: u8| desk.rows.iter().position(|r| r.iter().any(|e| e.id == id));
    for a in &plan {
        for b in &plan {
            if let (Some(ra), Some(rb)) = (row_of(a.id), row_of(b.id))
                && ra < rb
                && a.across(b)
                && a.bottom() > b.y
            {
                return Err(format!("{a:?} of row {ra} is not above {b:?} of row {rb}"));
            }
        }
    }
    // Left to right as seen.
    for row in &desk.rows {
        for a in row {
            for b in row {
                if let (Some(ca), Some(cb)) = (a.center, b.center)
                    && ca.clamp(-FAR, FAR) < cb.clamp(-FAR, FAR)
                    && sizes.contains_key(&a.id)
                    && sizes.contains_key(&b.id)
                    && spot_of(&plan, a.id).x >= spot_of(&plan, b.id).x
                {
                    return Err(format!("{} is seen left of {} but is not", a.id, b.id));
                }
            }
        }
    }
    // The plan of what it shows is itself.
    let mut again = run(&seen_again(case, &plan));
    let mut sorted = plan.clone();
    again.sort_by_key(|p| p.id);
    sorted.sort_by_key(|p| p.id);
    if again != sorted {
        let moved: Vec<String> = sorted
            .iter()
            .zip(&again)
            .filter(|(a, b)| a != b)
            .map(|(a, b)| format!("{a:?} -> {b:?}"))
            .collect();
        return Err(format!("planned again, moved: {moved:?}"));
    }
    Ok(plan)
}

#[test]
fn any_desk_is_whole_reachable_and_stable() {
    let mut rng = Rng::new(0x5eed_0001);
    for i in 0..5000 {
        let case = random_case(&mut rng);
        if let Err(e) = check(&case) {
            panic!("case {i}: {e}\n{case:?}");
        }
    }
}

/// Rows as the headset lays them out: each row side by side around straight ahead,
/// rows stacked above the main one, every monitor within its row's height; seen a
/// little off (a hand's tremor).
fn seen_case(rng: &mut Rng) -> Case {
    let mut sizes = BTreeMap::new();
    let mut rows = Vec::new();
    let mut id = 0u8;
    let mut top = 0;
    for _ in 0..1 + rng.below(3) {
        let row: Vec<(u8, Size)> = (0..1 + rng.below(5))
            .map(|_| {
                id += 1;
                (id, random_size(rng))
            })
            .collect();
        let width: i32 = row.iter().map(|m| m.1.w).sum();
        let tallest = row.iter().map(|m| m.1.h).max().unwrap_or(0);
        let mut x = -width / 2;
        let mut entries = Vec::new();
        for (id, s) in row {
            sizes.insert(id, s);
            // Anywhere within the row's height: centred, tops or bottoms lined up.
            let y = top - tallest + [0, (tallest - s.h) / 2, tallest - s.h][rng.below(3)];
            entries.push(Entry {
                id,
                center: Some(x + s.w / 2 + rng.range(-10, 10)),
                middle: Some(y + s.h / 2 + rng.range(-10, 10)),
            });
            x += s.w;
        }
        top -= tallest;
        rows.push(entries);
    }
    // The first row made is the main one, at the bottom.
    rows.reverse();
    let main = rows.last().map(|r| r[rng.below(r.len())].id);
    let desk = Desk {
        rows,
        main,
        ..Desk::default()
    };
    (desk, sizes)
}

/// Laid out as seen, the desktop is no wider than its widest row and a monitor, nor
/// taller than its rows' tallest monitors on top of each other (give or take the
/// tremor, which lines up only with a neighbour).
#[test]
fn a_desk_as_seen_does_not_spread() {
    let mut rng = Rng::new(0x5eed_0002);
    for i in 0..5000 {
        let case = seen_case(&mut rng);
        let plan = check(&case).unwrap_or_else(|e| panic!("case {i}: {e}\n{case:?}"));
        let (desk, sizes) = &case;
        let widest_row = desk
            .rows
            .iter()
            .map(|r| r.iter().map(|e| sizes[&e.id].w).sum::<i32>())
            .max()
            .unwrap();
        let widest = sizes.values().map(|s| s.w).max().unwrap();
        let tallest: i32 = desk
            .rows
            .iter()
            .map(|r| r.iter().map(|e| sizes[&e.id].h).max().unwrap())
            .sum();
        let width = plan.iter().map(Spot::right).max().unwrap();
        let height = plan.iter().map(Spot::bottom).max().unwrap();
        // Up to 10 px off each, and lined up with a neighbour that is off too.
        let tremor = 20 * sizes.len() as i32;
        assert!(
            width <= widest_row + widest && height <= tallest + tremor,
            "case {i}: {width}x{height} for rows {widest_row} wide, {tallest} tall\n{case:?}\n{plan:?}"
        );
    }
}

/// `(id, w, h)` with unknown positions.
fn seats(row: &[(u8, i32, i32)]) -> (Vec<Entry<u8>>, Vec<(u8, Size)>) {
    row.iter()
        .map(|&(id, w, h)| (Entry::new(id), (id, Size { w, h })))
        .unzip()
}

fn at(id: u8, center: Option<i32>, middle: Option<i32>) -> Entry<u8> {
    Entry { id, center, middle }
}

fn sized(list: &[(u8, i32, i32)]) -> BTreeMap<u8, Size> {
    list.iter().map(|&(id, w, h)| (id, Size { w, h })).collect()
}

#[test]
fn a_row_keeps_how_monitors_line_up_vertically() {
    // A landscape left of a portrait, tops lined up as seen.
    let sizes = sized(&[(0, 2560, 1440), (1, 1440, 2560)]);
    let desk = Desk {
        rows: vec![vec![
            at(0, Some(-1280), Some(-560)),
            at(1, Some(720), Some(0)),
        ]],
        main: Some(1),
        ..Desk::default()
    };
    let p = run(&(desk, sizes.clone()));
    assert_eq!(spot_of(&p, 0).y, spot_of(&p, 1).y);
    // Unknown heights: centred.
    let desk = Desk {
        rows: vec![vec![at(0, Some(-1280), None), at(1, Some(720), None)]],
        main: Some(1),
        ..Desk::default()
    };
    let p = run(&(desk, sizes));
    assert_eq!(spot_of(&p, 0).y - spot_of(&p, 1).y, (2560 - 1440) / 2);
}

/// A portrait primary in the middle; a landscape on its left, tops lined up; a
/// landscape on its right, lower down; the laptop screen under the primary.
#[test]
fn the_desktop_is_what_the_eyes_see() {
    let sizes = sized(&[
        (0, 2560, 1440),
        (1, 1440, 2560),
        (2, 2560, 1440),
        (100, 1920, 1080),
    ]);
    let desk = Desk {
        rows: vec![vec![
            at(0, Some(-2000), Some(-560)),
            at(1, Some(0), Some(0)),
            at(2, Some(1950), Some(300)),
        ]],
        main: Some(1),
        under_main: Some(100),
        ..Desk::default()
    };
    let p = run(&(desk, sizes));
    let (s0, s1, s2, m) = (
        spot_of(&p, 0),
        spot_of(&p, 1),
        spot_of(&p, 2),
        spot_of(&p, 100),
    );
    assert_eq!((s0.right(), s0.y), (s1.x, s1.y), "touching, tops lined up");
    assert_eq!(s1.right(), s2.x);
    assert!(s2.y > s1.y + 200 && s2.y < s1.bottom() - 200, "{s2:?}");
    assert_eq!(m.y, s1.bottom(), "the laptop screen under the primary");
    assert!(m.across(&s1));
}

/// A monitor seen above the right one of two stays above it, touching it.
#[test]
fn above_the_right_monitor_stays_above_it() {
    let sizes = sized(&[(0, 2560, 1440), (1, 2560, 1440), (2, 2560, 1440)]);
    let desk = Desk {
        rows: vec![
            vec![at(2, Some(1500), None)],
            vec![at(0, Some(-1600), None), at(1, Some(1500), None)],
        ],
        main: Some(0),
        ..Desk::default()
    };
    let p = run(&(desk, sizes));
    let (s0, s1, s2) = (spot_of(&p, 0), spot_of(&p, 1), spot_of(&p, 2));
    assert_eq!((s1.x, s1.y), (s0.right(), s0.y), "the main row touches");
    assert_eq!(s2.x, s1.x, "straight above the right one");
    assert_eq!(s2.bottom(), s1.y, "touching it");
}

/// Side monitors around a tall portrait one, and one above the portrait one: it
/// touches the portrait top, centred over it.
#[test]
fn above_a_portrait_monitor() {
    let sizes = sized(&[
        (0, 2560, 1440),
        (1, 1440, 2560),
        (2, 2560, 1440),
        (3, 1920, 1080),
    ]);
    let desk = Desk {
        rows: vec![
            vec![at(3, Some(0), None)],
            vec![
                at(0, Some(-2000), None),
                at(1, Some(0), None),
                at(2, Some(2000), None),
            ],
        ],
        main: Some(1),
        ..Desk::default()
    };
    let p = run(&(desk, sizes));
    let (s1, s3) = (spot_of(&p, 1), spot_of(&p, 3));
    assert_eq!(s3.bottom(), s1.y, "touches the portrait one");
    assert_eq!(s3.x + 960, s1.x + 720, "centred over it");
}

/// A monitor far off to the side of the row below still overlaps it enough for the
/// mouse.
#[test]
fn far_off_monitor_stays_reachable() {
    let sizes = sized(&[(0, 2560, 1440), (1, 1920, 1080)]);
    let desk = Desk {
        rows: vec![vec![at(1, Some(9000), None)], vec![at(0, Some(0), None)]],
        main: Some(0),
        ..Desk::default()
    };
    let p = run(&(desk, sizes));
    let (s0, s1) = (spot_of(&p, 0), spot_of(&p, 1));
    assert_eq!(s1.right().min(s0.right()) - s1.x.max(s0.x), REACH);
}

/// Without seen positions (a fresh start): one row, side by side, centred on the
/// tallest.
#[test]
fn unknown_positions_line_up() {
    let (row, list) = seats(&[(0, 2560, 1440), (1, 1440, 2560), (2, 2560, 1440)]);
    let desk = Desk {
        rows: vec![row],
        main: Some(1),
        ..Desk::default()
    };
    let p = run(&(desk, list.into_iter().collect()));
    assert_eq!(spot_of(&p, 0).x, 0);
    assert_eq!(spot_of(&p, 1).x, 2560);
    assert_eq!(spot_of(&p, 2).x, 4000);
    assert_eq!(spot_of(&p, 0).y, 560, "centred on the tall one");
}

/// Window panels hang in a column right of everything; other real displays sit next
/// to the laptop screen, under the monitors.
#[test]
fn panels_and_other_displays() {
    let sizes = sized(&[
        (0, 2560, 1440),
        (1, 2560, 1440),
        (100, 1920, 1200),
        (101, 1920, 1080),
        (10, 800, 600),
        (11, 640, 480),
    ]);
    let desk = Desk {
        rows: vec![vec![at(0, None, None), at(1, None, None)]],
        main: Some(1),
        under_main: Some(100),
        bottom: vec![101],
        column: vec![10, 11],
    };
    let p = check(&(desk, sizes)).unwrap();
    let (s1, m, o) = (spot_of(&p, 1), spot_of(&p, 100), spot_of(&p, 101));
    assert_eq!((m.x, m.y), (s1.x + (2560 - 1920) / 2, s1.bottom()));
    assert_eq!((o.x, o.y), (m.right(), s1.bottom()));
    let (a, b) = (spot_of(&p, 10), spot_of(&p, 11));
    assert_eq!((a.x, a.y), (o.right(), 0));
    assert_eq!((b.x, b.y), (a.x, a.bottom()));
}

/// The six monitors of 2026-11-19, as the PC saved them then (`monitors.txt`, laid out
/// by the old chain through overlapping monitors): no overlaps, every monitor in reach,
/// and no wider than the widest row and a monitor, no taller than the rows' tallest
/// monitors on top of each other (the old chain spread it over -1720..7600).
#[test]
fn six_monitors_from_the_old_chain() {
    let sizes = sized(&[
        (0, 3440, 1440),
        (1, 2560, 1440),
        (2, 1440, 2560),
        (3, 3440, 1440),
        (4, 2560, 1440),
        (5, 3440, 1440),
        (100, 1440, 900),
    ]);
    let desk = Desk {
        rows: vec![
            vec![at(0, Some(-2872), Some(-72))],
            vec![
                at(1, Some(-1280), Some(1368)),
                at(2, Some(720), Some(1280)),
                at(3, Some(-2264), Some(6768)),
                at(4, Some(-2000), Some(5328)),
            ],
            vec![at(5, Some(-1800), Some(2808))],
        ],
        main: Some(2),
        under_main: Some(100),
        ..Desk::default()
    };
    let p = check(&(desk, sizes)).unwrap();
    let width = p.iter().map(Spot::right).max().unwrap();
    let height = p.iter().map(Spot::bottom).max().unwrap();
    assert!(width <= 10_000 + 3440, "{width}: {p:?}");
    assert!(height <= 1440 + 2560 + 1440, "{height}: {p:?}");
}
