//! Another program resizes a monitor, as sharing it in Discord does on KDE: the PC puts
//! its size back, makes it anew when that does not help, and gives up on a program
//! that keeps doing it.

use std::time::Duration;

use prostir_testkit::FakeHeadset;

use crate::pc::{Options, Pc};
use crate::saved::placed;
use crate::{SOON, session};

/// Fake-2 of the session's two monitors: 480x272.
const SECOND: &str = "Fake-2";
const PLACED: &str = "fake desktop: placed";

/// Waits up to `timeout` for the `skip + 1`th log line containing `needle`, taking what
/// the PC sends meanwhile so the session stays open.
fn wait_log(pc: &Pc, headset: &mut FakeHeadset, needle: &str, skip: usize) {
    let until = std::time::Instant::now() + SOON;
    while pc.count_log(needle) <= skip {
        assert!(
            std::time::Instant::now() < until,
            "the PC app did not log {needle:?} within {SOON:?}"
        );
        headset.take_for(Duration::from_millis(100)).unwrap();
    }
}

/// The desktop is laid out again (after the first `skip` layouts) at the same positions
/// as `before`, once the size is back.
fn wait_placed_as(pc: &Pc, headset: &mut FakeHeadset, skip: usize, before: &[(String, i32, i32)]) {
    wait_log(pc, headset, PLACED, skip);
    let until = std::time::Instant::now() + SOON;
    while placed(pc) != before {
        assert!(
            std::time::Instant::now() < until,
            "laid out at {:?}, not {before:?}",
            placed(pc)
        );
        headset.take_for(Duration::from_millis(100)).unwrap();
    }
}

#[test]
fn a_monitor_resized_by_another_program_gets_its_size_back() {
    let (_dir, pc, _keys, mut headset) = session("resized", &Options::default());
    headset.take_for(Duration::from_secs(1)).unwrap();
    let id = headset.view().order()[1];
    let laid_out = placed(&pc);
    let layouts = pc.count_log(PLACED);
    let starts = pc.source_starts();
    let stream_starts = headset.view().streams[&id].starts;

    pc.desktop(&format!("resize {SECOND} 320x240"));
    wait_log(&pc, &mut headset, "Fake-2 was resized to 320x240", 0);
    wait_log(&pc, &mut headset, "Fake-2 is 480x272 again", 0);
    wait_placed_as(&pc, &mut headset, layouts, &laid_out);
    headset
        .wait(SOON, "the monitor streaming again", |v| {
            v.streams[&id].starts > stream_starts && v.streams[&id].has_picture()
        })
        .unwrap();

    let mut once_more = starts.clone();
    *once_more.get_mut(SECOND).unwrap() += 1;
    assert_eq!(
        pc.source_starts(),
        once_more,
        "only Fake-2 started over, once"
    );
    assert_eq!(pc.count_log("created Fake-2"), 1, "not made anew");
    let view = headset.view();
    assert_eq!(view.streams[&id].width, 480);
    assert_eq!(view.order().len(), 2);
    assert!(!view.closed, "the session goes on");
    assert_eq!(pc.count_log("notification: Screen sharing"), 1, "the tip");
}

#[test]
fn a_size_that_does_not_come_back_makes_the_monitor_anew() {
    let (_dir, pc, _keys, mut headset) = session("resized-sticky", &Options::default());
    headset.take_for(Duration::from_secs(1)).unwrap();
    let order = headset.view().order();
    let id = order[1];
    let laid_out = placed(&pc);
    let layouts = pc.count_log(PLACED);
    let stream_starts = headset.view().streams[&id].starts;

    pc.desktop(&format!("resize-sticky {SECOND} 320x240"));
    wait_log(&pc, &mut headset, "Fake-2 was resized to 320x240", 0);
    wait_log(
        &pc,
        &mut headset,
        "Fake-2 is still 320x240: making it again",
        0,
    );
    wait_log(&pc, &mut headset, "Fake-2 is 480x272 again", 0);
    wait_placed_as(&pc, &mut headset, layouts, &laid_out);
    // Started over by the restart, then by the new output.
    headset
        .wait(SOON, "the new monitor streaming", |v| {
            v.streams[&id].starts >= stream_starts + 2 && v.streams[&id].has_picture()
        })
        .unwrap();

    assert_eq!(pc.count_log("created Fake-2"), 2, "made anew once");
    let view = headset.view();
    assert_eq!(view.order(), order, "the same monitor in the same place");
    assert_eq!(view.streams[&id].width, 480);
    assert!(!view.closed, "the session goes on");
}

#[test]
fn a_program_that_keeps_resizing_is_not_fought_forever() {
    let (_dir, pc, _keys, mut headset) = session("resized-again", &Options::default());
    headset.take_for(Duration::from_secs(1)).unwrap();
    let id = headset.view().order()[1];
    let starts = pc.source_starts()[SECOND];

    for i in 0..3 {
        pc.desktop(&format!("resize {SECOND} 320x240"));
        wait_log(&pc, &mut headset, "Fake-2 was resized to 320x240", i);
        wait_log(&pc, &mut headset, "Fake-2 is 480x272 again", i);
    }
    pc.desktop(&format!("resize {SECOND} 320x240"));
    wait_log(&pc, &mut headset, "Fake-2 keeps being resized", 0);
    // Left as it is from now on.
    headset.take_for(Duration::from_secs(3)).unwrap();

    assert_eq!(pc.count_log("Fake-2 was resized to"), 3, "restored 3 times");
    assert_eq!(pc.count_log("making it again"), 0);
    assert_eq!(pc.source_starts()[SECOND], starts + 3);
    assert_eq!(pc.count_log("notification: Monitor size"), 1, "said once");
    assert_eq!(
        pc.count_log("notification: Screen sharing"),
        1,
        "the tip once"
    );
    let view = headset.view();
    assert!(view.streams[&id].has_picture(), "monitor 2 still streams");
    assert!(!view.closed, "the session goes on");
}
