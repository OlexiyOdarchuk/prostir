//! What the PC keeps between runs: the monitors with their numbers, even one that could
//! not start.

use std::time::Duration;

use prostir_proto::ClientMsg;
use prostir_proto::request::{Outcome, Request};
use prostir_proto::state::{DisplayId, Mode, Placed, Row};
use prostir_testkit::View;

use crate::pc::{Options, Pc, TestDir};
use crate::{SOON, START, TWO_MONITORS, connect, session};

/// Where the fake desktop put each output the last time (`name -> (x, y)`).
pub fn placed(pc: &Pc) -> Vec<(String, i32, i32)> {
    let lines = pc.lines("fake desktop: placed ");
    let Some(line) = lines.last() else {
        return Vec::new();
    };
    let list = line
        .split("fake desktop: placed ")
        .nth(1)
        .unwrap_or_default();
    let mut out: Vec<(String, i32, i32)> = list
        .split(", ")
        .filter_map(|item| {
            let (name, at) = item.split_once(' ')?;
            let (x, y) = at.split_once(',')?;
            Some((name.to_owned(), x.parse().ok()?, y.parse().ok()?))
        })
        .collect();
    out.sort();
    out
}

/// Two rows from the headset: the second monitor over the first.
fn two_rows(first: DisplayId, second: DisplayId) -> ClientMsg {
    let at = |id, middle| Placed {
        id,
        center: Some(0),
        middle: Some(middle),
    };
    ClientMsg::Arrange {
        rows: vec![
            Row {
                entries: vec![at(second, -400)],
            },
            Row {
                entries: vec![at(first, 0)],
            },
        ],
    }
}

fn ids<const N: usize>(n: [u32; N]) -> [DisplayId; N] {
    n.map(DisplayId)
}

/// Screens saved in `screens.txt`, by number.
fn saved_numbers(dir: &TestDir) -> Vec<u32> {
    dir.saved("screens.txt")
        .lines()
        .filter_map(|l| l.strip_prefix("screen ")?.split(' ').next()?.parse().ok())
        .collect()
}

/// A monitor that cannot start keeps its place: in the order the headset gets, and in
/// the saved set. The next run without the fault shows it again.
#[test]
fn a_monitor_that_cannot_start_stays_in_the_set() {
    let dir = TestDir::new("cannot-start");
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let options = Options {
        broken_monitors: "2".into(),
        ..Options::default()
    };
    let pc = Pc::start(&dir, &options);
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "one monitor showing, the other failed", |v| {
            v.streams.values().any(|s| s.has_picture())
                && v.displays().iter().any(|d| {
                    matches!(&d.state, prostir_proto::state::DisplayState::Failed { detail, .. } if detail.contains("broken"))
                })
        })
        .unwrap();
    let view = headset.view();
    assert_eq!(view.order(), ids([0, 1]), "both stay in the list");
    assert!(!view.streams.contains_key(&DisplayId(1)));
    headset.disconnect();
    pc.quit();
    assert_eq!(saved_numbers(&dir), [0, 1], "both stay in the set");

    let pc = Pc::start(&dir, &Options::default());
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(headset.view().order(), ids([0, 1]));
}

/// Monitors keep their numbers between runs, so the headset finds each where it was.
#[test]
fn monitors_keep_their_numbers_after_a_restart() {
    let (dir, pc, mut keys, mut headset) = session("numbers", &Options::default());
    let added = headset
        .ask(Request::AddDisplay {
            mode: Mode::new(800, 450),
        })
        .unwrap();
    assert_eq!(added, Outcome::Added { id: DisplayId(2) });
    let removed = headset
        .ask(Request::RemoveDisplay { id: DisplayId(0) })
        .unwrap();
    assert_eq!(removed, Outcome::Done);
    assert_eq!(headset.view().order(), ids([1, 2]));
    // Written off the main loop within a pass.
    headset.take_for(Duration::from_millis(500)).unwrap();
    headset.disconnect();
    pc.quit();
    assert_eq!(saved_numbers(&dir), [1, 2]);

    let pc = Pc::start(&dir, &Options::default());
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(headset.view().order(), ids([1, 2]), "the same numbers");
    assert_eq!(headset.view().streams[&DisplayId(2)].width, 800);
}

/// Two rows from the headset reach the desktop (one monitor over the other), and a
/// restart of the PC app lays the desktop out the same way before the headset says
/// anything.
#[test]
fn two_rows_survive_a_restart() {
    let (dir, pc, mut keys, mut headset) = session("two-rows", &Options::default());
    let order = headset.view().order();
    headset.send(&two_rows(order[0], order[1])).unwrap();
    // Fake-1 640x360 under Fake-2 480x272, touching.
    let stacked = |p: &[(String, i32, i32)]| {
        let at = |n: &str| p.iter().find(|o| o.0 == n).map(|o| (o.1, o.2));
        matches!((at("Fake-1"), at("Fake-2")), (Some((_, y1)), Some((_, y2))) if y2 + 272 == y1)
    };
    let until = std::time::Instant::now() + SOON;
    while !stacked(&placed(&pc)) {
        assert!(std::time::Instant::now() < until, "{:?}", placed(&pc));
        headset.take_for(Duration::from_millis(100)).unwrap();
    }
    let before = placed(&pc);
    headset.take_for(Duration::from_millis(500)).unwrap();
    headset.disconnect();
    pc.quit();
    assert!(
        dir.saved("screens.txt").matches("row").count() == 2,
        "{}",
        dir.saved("screens.txt")
    );

    let mut pc = Pc::start(&dir, &Options::default());
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    pc.wait_log(SOON, "fake desktop: placed");
    assert_eq!(placed(&pc), before, "laid out as before");
}

/// A choice made in the tray while no headset is connected is kept: the PC app quits
/// and starts with it.
#[test]
fn a_tray_change_without_monitors_survives_quitting() {
    let dir = TestDir::new("tray-offline");
    dir.save_monitors(TWO_MONITORS);
    let mut pc = Pc::start(&dir, &Options::default());
    pc.tray("sharp on");
    pc.wait_log(SOON, "clearer text: true");
    pc.quit();
    assert!(
        dir.saved("settings.txt").contains("sharp on\n"),
        "{}",
        dir.saved("settings.txt")
    );
    // The options of that run were not saved.
    assert!(dir.saved("settings.txt").contains("software off\n"));
    assert!(dir.saved("settings.txt").contains("bitrate 30\n"));
}

/// Killed while the headset changes the layout over and over, the PC app leaves files
/// it reads on the next start, with every monitor.
#[test]
fn killed_mid_change_leaves_readable_files() {
    let (dir, pc, mut keys, mut headset) = session("killed", &Options::default());
    let order = headset.view().order();
    for i in 0..60 {
        let msg = if i % 2 == 0 {
            two_rows(order[0], order[1])
        } else {
            two_rows(order[1], order[0])
        };
        headset.send(&msg).unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
    pc.kill();
    let screens = dir.saved("screens.txt");
    assert_eq!(saved_numbers(&dir), [0, 1], "{screens}");

    let mut pc = Pc::start(&dir, &Options::default());
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(headset.view().order().len(), 2);
    assert_eq!(pc.count_log("screens.txt.bad"), 0);
    pc.wait_log(SOON, "fake desktop: placed");
}
