//! The encoders: probed once and kept in the cache, one graphics device shared by all
//! of them, a probe that hangs holding nothing up, and a session limit learned when an
//! encoder does not open, the output going on with the next encoder.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use prostir_testkit::View;

use crate::pc::{Options, Pc, TestDir};
use crate::{START, connect};

/// Three small monitors.
const THREE_MONITORS: &str = "row\n640x360\n480x272\n320x240\nmirror off\n";

/// Five monitors, as many as a desk of them.
const FIVE_MONITORS: &str = "row\n640x360\n640x360\n640x360\n640x360\n640x360\nmirror off\n";

/// The cache of what the probes found, in the test's cache directory.
fn cache(dir: &TestDir) -> String {
    std::fs::read_to_string(dir.path().join("cache/prostir/encoders.txt")).unwrap_or_default()
}

/// Five monitors on the graphics card open its device once: one VA display (a render
/// node of the card) for all their encoders, not one each. Skipped where the PC has no
/// hardware encoder that takes the fake desktop's frames.
#[test]
fn monitors_share_one_graphics_device() {
    let dir = TestDir::new("one-device");
    dir.save_monitors(FIVE_MONITORS);
    let mut keys = dir.pair();
    let options = Options {
        software: false,
        ..Options::default()
    };
    let mut pc = Pc::start(&dir, &options);
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    pc.wait_log(START, "video encoders");
    let on_card = pc.count_log("encoding with VA-API");
    if on_card == 0 {
        eprintln!("no VA-API encoder here: nothing shares a device");
        return;
    }
    let files = pc.gpu_files();
    let cards: BTreeSet<&String> = files.iter().collect();
    eprintln!(
        "{on_card} monitors on VA-API with {} graphics device files open: {files:?}",
        files.len()
    );
    assert_eq!(on_card, 5, "every monitor on the card");
    assert_eq!(
        files.len(),
        cards.len(),
        "one device per card for every encoder: {files:?}"
    );
    assert_eq!(pc.count_log("sharing gst.va.display.handle"), 1);
}

/// The encoders are probed at the first session and kept: the next start reads what
/// was found instead, and a probe that never ends holds the first session up for no
/// more than its time limit, the encoder it was for not used.
#[test]
fn probes_are_kept_and_one_that_hangs_holds_nothing_up() {
    let dir = TestDir::new("probe-hangs");
    dir.save_monitors(THREE_MONITORS);
    let mut keys = dir.pair();
    let options = Options {
        software: false,
        encoder: Some("hangs"),
        ..Options::default()
    };
    let mut pc = Pc::start(&dir, &options);
    let started = Instant::now();
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    let took = started.elapsed();
    pc.wait_log(START, "video encoders probed");
    assert!(
        took < Duration::from_secs(8),
        "monitors after {took:?}, the probe held them up"
    );
    assert_eq!(pc.count_log("fake hardware H264: did not answer in 3 s"), 1);
    assert_eq!(pc.count_log("encoding with fake hardware"), 0);
    let kept = cache(&dir);
    assert!(kept.contains("missing fake h264 did not answer"), "{kept}");
    headset.disconnect();
    pc.quit();

    // Started again: from the cache, nothing probed, the hung one not waited for.
    let mut pc = Pc::start(&dir, &options);
    let started = Instant::now();
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "monitors after {took:?}");
    pc.wait_log(START, "video encoders as found before");
    assert_eq!(pc.count_log("video encoders probed"), 0);
}

/// A hardware encoder that opens two sessions: the third monitor's encoder does not
/// open, the limit is learned and kept, and that monitor alone goes on with the next
/// encoder (x264, or the PC's own card), saying why; the session goes on. The next
/// start knows the limit and does not try a third.
#[test]
fn a_session_limit_is_learned_and_one_output_goes_on_with_the_next() {
    let dir = TestDir::new("session-limit");
    dir.save_monitors(THREE_MONITORS);
    let mut keys = dir.pair();
    let options = Options {
        software: false,
        encoder: Some("sessions=2"),
        ..Options::default()
    };
    let mut pc = Pc::start(&dir, &options);
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    pc.wait_log(START, "did not open: no more encoder sessions");
    pc.wait_log(START, "fake hardware opens at most 2 sessions at once");
    let why = pc.wait_log(START, "fake hardware: ");
    assert!(why.contains('2'), "{why}");
    // Tried for all three, the third did not open.
    assert_eq!(pc.count_log("encoding with fake hardware"), 3);
    assert_eq!(pc.count_log("session ended"), 0, "the session goes on");
    assert!(cache(&dir).contains("sessions fake - 2"), "{}", cache(&dir));
    // Each monitor still streams.
    headset.take_for(Duration::from_secs(1)).unwrap();
    assert!(headset.view().all_monitors_show());
    headset.disconnect();
    pc.quit();

    let mut pc = Pc::start(&dir, &options);
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    pc.wait_log(START, "fake hardware: ");
    assert_eq!(pc.count_log("did not open"), 0, "the limit is known");
    assert_eq!(pc.count_log("encoding with fake hardware"), 2);
}

/// A hardware encoder whose probe fails is not used; the monitors stream all the same.
#[test]
fn an_encoder_that_fails_its_probe_is_not_used() {
    let dir = TestDir::new("probe-fails");
    dir.save_monitors(THREE_MONITORS);
    let mut keys = dir.pair();
    let options = Options {
        software: false,
        encoder: Some("broken"),
        ..Options::default()
    };
    let mut pc = Pc::start(&dir, &options);
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    pc.wait_log(START, "fake hardware H264: broken on purpose");
    assert_eq!(pc.count_log("encoding with fake hardware"), 0);
}
