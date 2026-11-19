//! What a session survives and what ends it: Wi-Fi freezing for a moment or for good,
//! a PC going to sleep, a connection cut half way through a frame, a broken packet.

use std::time::{Duration, Instant};

use prostir_testkit::View;

use crate::pc::Options;
use crate::pc::{Pc, TestDir};
use crate::{SOON, START, TWO_MONITORS, connect, session};

/// A busy screen, so frames keep coming and a stall loses some.
fn busy() -> Options {
    Options {
        fps: 15,
        noise: true,
        ..Options::default()
    }
}

/// Wi-Fi frozen for 2 s: the session lives on; the frames that got too old are
/// dropped on the PC, and every stream starts again from a keyframe.
#[test]
fn a_short_stall_keeps_the_session() {
    let (_dir, mut pc, _keys, mut headset) = session("stall-short", &busy());
    headset.reset_counts();
    headset.stall(Duration::from_secs(2));
    // Without a loss no keyframe would come for 40 s (600 frames at 15 a second).
    headset
        .wait(SOON, "a keyframe on every stream", |v| {
            v.streams.len() == 2 && v.streams.values().all(|s| s.keyframes > 0)
        })
        .unwrap();
    headset.take_for(Duration::from_secs(1)).unwrap();
    assert!(!headset.view().closed);
    assert_eq!(pc.count_log("session ended"), 0);
    let line = pc.wait_log(Duration::from_secs(15), "Prostir-1: sent");
    assert!(
        !line.contains("dropped 0 late"),
        "frames were dropped late: {line}"
    );
}

/// Wi-Fi frozen for longer than 5 s: both sides end the session, and the headset
/// connects again to the same monitors.
#[test]
fn a_long_stall_ends_the_session_on_both_sides() {
    let (_dir, mut pc, mut keys, mut headset) = session("stall-long", &busy());
    let stalled = Instant::now();
    headset.stall(Duration::from_secs(6));
    pc.wait_log(Duration::from_secs(10), "session ended");
    assert!(stalled.elapsed() < Duration::from_secs(8));
    pc.wait_log(SOON, "keeping virtual monitors for 30 s");
    headset.wait_closed(Duration::from_secs(10)).unwrap();
    let mut back = connect(&pc, &mut keys);
    back.wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(
        pc.count_log("fake desktop: created"),
        2,
        "the same monitors"
    );
}

/// A PC that stops dead (asleep): the headset notices the silence within 5 s and
/// closes; once the PC runs again its session ends too, and the headset connects.
#[test]
fn a_pc_that_stops_is_noticed() {
    let (_dir, pc, mut keys, mut headset) = session("frozen-pc", &Options::default());
    pc.freeze();
    let frozen = Instant::now();
    headset.wait_closed(Duration::from_secs(8)).unwrap();
    assert!(frozen.elapsed() >= Duration::from_secs(4));
    assert_eq!(headset.view().end, Some(std::io::ErrorKind::TimedOut));
    std::thread::sleep(Duration::from_secs(1));
    pc.thaw();
    let mut back = connect(&pc, &mut keys);
    back.wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
}

/// The headset hangs up half way through a frame: the half frame goes with the
/// session, and the next session starts clean, a keyframe first on every stream.
#[test]
fn a_cut_in_the_middle_of_a_frame_leaves_nothing_behind() {
    let (_dir, mut pc, mut keys, mut headset) = session("cut", &busy());
    headset.cut_mid_message();
    headset.wait_closed(SOON).unwrap();
    assert_eq!(
        headset.view().end,
        Some(std::io::ErrorKind::ConnectionAborted)
    );
    pc.wait_log(SOON, "keeping virtual monitors for 30 s");
    let mut back = connect(&pc, &mut keys);
    back.wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    for s in back.view().streams.values() {
        assert_eq!(
            s.first_is_key,
            Some(true),
            "{} starts with a keyframe",
            s.name
        );
        assert_eq!(s.bad_keyframes, 0);
    }
    assert_eq!(pc.count_log("panicked"), 0);
}

/// Monitors that take long to make (7 s here, longer than 5 s of silence ends a
/// session) do not cost the session: the PC opens it once it can serve it, so the
/// headset does not count that time as silence.
#[test]
fn slow_monitors_do_not_end_the_session() {
    let options = Options {
        slow_monitors: Duration::from_millis(3500),
        ..Options::default()
    };
    let (_dir, pc, _keys, mut headset) = session("slow-monitors", &options);
    headset.take_for(Duration::from_secs(1)).unwrap();
    assert!(!headset.view().closed);
    assert_eq!(pc.count_log("headset connected"), 1, "one connection");
}

/// A broken packet ends the session with a line in the log, not a crash.
#[test]
fn a_broken_packet_ends_the_session() {
    let (_dir, mut pc, mut keys, mut headset) = session("broken", &Options::default());
    // Flags no packet has.
    headset.send_record(&[0x80, 0, 1, 2, 3]).unwrap();
    headset.wait_closed(SOON).unwrap();
    let line = pc.wait_log(SOON, "session ended");
    assert!(line.contains("unknown packet flags"), "{line}");
    assert_eq!(pc.count_log("panicked"), 0);
    let mut back = connect(&pc, &mut keys);
    back.wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
}

/// A headset that reads slowly (a slow link on plain loopback): the PC's queues fill,
/// and sound still goes between the packets of the frames instead of after them.
#[test]
fn sound_goes_between_the_packets_of_frames() {
    let dir = TestDir::new("slow-reader");
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let pc = Pc::start(&dir, &busy());
    // 16 Mbit/s: less than the two busy monitors send.
    let mut headset = prostir_testkit::FakeHeadset::connect_reading_at(pc.addr, &mut keys, 2 << 20)
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    headset.reset_counts();
    headset.take_for(Duration::from_secs(5)).unwrap();
    let view = headset.view();
    assert!(
        view.sound_between_fragments > 10,
        "sound between the packets of frames: {} of {} slices",
        view.sound_between_fragments,
        view.sound_delays.len()
    );
    let over = view.sound_gaps.over();
    assert_eq!(over[3], 0, "no pause in the sound over 250 ms: {over:?}");
}
