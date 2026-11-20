//! The encoders: probed once and kept in the cache, one graphics device shared by all
//! of them, a probe that hangs holding nothing up, and a session limit learned when an
//! encoder does not open, the output going on with the next encoder.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use prostir_proto::request::{Outcome, Request};
use prostir_testkit::View;

use crate::pc::{Options, Pc, TestDir};
use crate::{SOON, START, connect};

/// Three small monitors.
const THREE_MONITORS: &str = "row\n640x360\n480x272\n320x240\nmirror off\n";

/// Five monitors, as many as a desk of them.
const FIVE_MONITORS: &str = "row\n640x360\n640x360\n640x360\n640x360\n640x360\nmirror off\n";

/// The cache of what the probes found, in the test's cache directory.
fn cache(dir: &TestDir) -> String {
    std::fs::read_to_string(dir.path().join("cache/prostir/encoders.txt")).unwrap_or_default()
}

/// Five monitors on the graphics card open its device once: one VA display (a render
/// node of the card) for all their encoders, not one each. Once no encoder uses the
/// card (all moved to the CPU), the device is closed while the session goes on, and
/// opened again when they come back. Skipped where the PC has no hardware encoder that
/// takes the fake desktop's frames.
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

    let encode = |headset: &mut prostir_testkit::FakeHeadset, software| {
        let asked = Request::SetEncoding {
            bitrate: None,
            codec: None,
            software: Some(software),
            sharp: None,
        };
        assert_eq!(headset.ask(asked).unwrap(), Outcome::Done);
    };
    encode(&mut headset, true);
    pc.wait_log(SOON, "closing gst.va.display.handle");
    let until = Instant::now() + SOON;
    while !pc.gpu_files().is_empty() {
        assert!(Instant::now() < until, "still open: {:?}", pc.gpu_files());
        std::thread::sleep(Duration::from_millis(50));
    }
    headset
        .wait(SOON, "every monitor showing", View::all_monitors_show)
        .unwrap();
    encode(&mut headset, false);
    pc.wait_log_after(SOON, "sharing gst.va.display.handle", 1);
    headset
        .wait(SOON, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(pc.gpu_files().len(), 1, "{:?}", pc.gpu_files());
    assert!(!headset.view().closed, "the session went on");
}

/// The encoders are probed when the PC app starts, before any headset, and kept: the
/// next start reads what was found instead. A probe that never ends holds nothing up
/// past its time limit, the encoder it was for is not used, and it is not kept as
/// missing: the next start uses what works at once and probes it again meanwhile.
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
    // Before any headset.
    pc.wait_log(START, "video encoders probed");
    let started = Instant::now();
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "monitors after {took:?}, the probe held them up"
    );
    assert_eq!(pc.count_log("fake hardware H264: did not answer in 3 s"), 1);
    assert_eq!(pc.count_log("encoding with fake hardware"), 0);
    let kept = cache(&dir);
    assert!(kept.contains("retry fake - h264 did not answer"), "{kept}");
    assert!(!kept.contains("missing fake"), "{kept}");
    headset.disconnect();
    pc.quit();

    // Started again: from the cache, the hung one probed again in the background,
    // not waited for.
    let mut pc = Pc::start(&dir, &options);
    let started = Instant::now();
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "monitors after {took:?}");
    pc.wait_log(START, "video encoders as found before");
    pc.wait_log(START, "probing again in the background: fake hardware H264");
    assert_eq!(pc.count_log("video encoders probed in"), 0, "not all again");
    assert_eq!(pc.count_log("encoding with fake hardware"), 0);
}

/// A hardware encoder that opens two sessions: the third monitor's encoder does not
/// open, once and again when tried once more, so the limit is learned and kept, and
/// that monitor alone goes on with the next encoder (x264, or the PC's own card),
/// saying why; the session goes on. The next start tries a third once (a limit kept is
/// a hint, it may have risen) and holds the limit after that one failure.
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
    pc.wait_log(
        START,
        "fake hardware did not open with 2 open: tried once more",
    );
    pc.wait_log(START, "fake hardware opens at most 2 sessions at once");
    let why = pc.wait_log(START, "fake hardware: ");
    assert!(why.contains('2'), "{why}");
    assert_eq!(pc.count_log("did not open: no more encoder sessions"), 2);
    // Tried for all three, twice for the third.
    assert_eq!(pc.count_log("encoding with fake hardware"), 4);
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
    assert_eq!(pc.count_log("did not open: no more encoder sessions"), 1);
    assert_eq!(
        pc.count_log("tried once more"),
        0,
        "one failure is enough now"
    );
    assert_eq!(pc.count_log("encoding with fake hardware"), 3);
    assert!(cache(&dir).contains("sessions fake - 2"), "{}", cache(&dir));
}

/// A hardware encoder that does not start at all past two sessions: the same as one
/// that starts and then does not open. The third monitor tries it twice, the limit is
/// learned, and that monitor goes on with the next encoder.
#[test]
fn an_encoder_that_does_not_start_teaches_its_limit_too() {
    let dir = TestDir::new("refuses");
    dir.save_monitors(THREE_MONITORS);
    let mut keys = dir.pair();
    let options = Options {
        software: false,
        encoder: Some("refuses=2"),
        ..Options::default()
    };
    let mut pc = Pc::start(&dir, &options);
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    pc.wait_log(START, "fake hardware opens at most 2 sessions at once");
    assert_eq!(
        pc.count_log("H264 did not start: no more encoder sessions"),
        2
    );
    assert_eq!(pc.count_log("encoding with fake hardware H264"), 2);
    assert!(cache(&dir).contains("sessions fake - 2"), "{}", cache(&dir));
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
    assert!(cache(&dir).contains("retry fake - h264 broken on purpose"));
}
