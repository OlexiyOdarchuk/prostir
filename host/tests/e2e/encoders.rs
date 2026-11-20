//! The encoders: one graphics device shared by all of them.

use std::collections::BTreeSet;

use prostir_testkit::View;

use crate::pc::{Options, Pc, TestDir};
use crate::{START, connect};

/// Five monitors, as many as a desk of them.
const FIVE_MONITORS: &str = "row\n640x360\n640x360\n640x360\n640x360\n640x360\nmirror off\n";

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
    let pc = Pc::start(&dir, &options);
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    if pc.count_log("encoding with Va ") == 0 {
        eprintln!("no VA-API encoder here: nothing shares a device");
        return;
    }
    let files = pc.gpu_files();
    let cards: BTreeSet<&String> = files.iter().collect();
    eprintln!(
        "5 monitors on VA-API with {} graphics device files open: {files:?}",
        files.len()
    );
    assert_eq!(
        files.len(),
        cards.len(),
        "one device per card for every encoder: {files:?}"
    );
    assert_eq!(pc.count_log("sharing gst.va.display.handle"), 1);
}
