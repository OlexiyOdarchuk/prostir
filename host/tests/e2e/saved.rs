//! What the PC keeps between runs: the monitors with their numbers, even one that could
//! not start.

use std::time::Duration;

use prostir_proto::ClientMsg;
use prostir_testkit::View;

use crate::pc::{Options, Pc, TestDir};
use crate::{SOON, START, TWO_MONITORS, connect, session};

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
        .wait(START, "one monitor showing, an error for the other", |v| {
            v.streams.values().any(|s| s.has_picture())
                && v.errors.iter().any(|e| e.contains("could not start"))
        })
        .unwrap();
    let view = headset.view();
    assert_eq!(view.order, [0, 1], "both stay in the order");
    assert!(!view.streams.contains_key(&1));
    headset.disconnect();
    pc.quit();
    assert_eq!(saved_numbers(&dir), [0, 1], "both stay in the set");

    let pc = Pc::start(&dir, &Options::default());
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(headset.view().order, [0, 1]);
}

/// Monitors keep their numbers between runs, so the headset finds each where it was.
#[test]
fn monitors_keep_their_numbers_after_a_restart() {
    let (dir, pc, mut keys, mut headset) = session("numbers", &Options::default());
    headset
        .send(&ClientMsg::AddMonitor {
            width: 800,
            height: 450,
        })
        .unwrap();
    headset
        .wait(SOON, "a third monitor", |v| v.order.len() == 3)
        .unwrap();
    headset.send(&ClientMsg::RemoveMonitor { id: 0 }).unwrap();
    headset
        .wait(SOON, "the first one gone", |v| v.order == [1, 2])
        .unwrap();
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
    assert_eq!(headset.view().order, [1, 2], "the same numbers");
    assert_eq!(headset.view().streams[&2].width, 800);
}
