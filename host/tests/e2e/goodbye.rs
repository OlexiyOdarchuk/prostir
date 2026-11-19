//! How sessions end: the PC and the headset say goodbye, and why.

use prostir_proto::{HeadsetGoodbye, PcGoodbye};
use prostir_testkit::{ConnectError, FakeHeadset};

use crate::pc::Options;
use crate::{SOON, session};

/// The PC app quitting (a signal, the PC shutting down) says so before it closes.
#[test]
fn the_pc_says_goodbye_when_it_quits() {
    let (_dir, pc, _keys, mut headset) = session("bye-quit", &Options::default());
    pc.quit();
    headset.wait_closed(SOON).unwrap();
    assert_eq!(headset.view().goodbye, Some(PcGoodbye::Quit.code()));
}

/// "Forget paired headsets" in the tray ends the session of the headset it forgot,
/// which then has to pair again.
#[test]
fn forgetting_the_headsets_ends_the_session() {
    let (_dir, mut pc, mut keys, mut headset) = session("bye-forget", &Options::default());
    pc.tray("forget");
    headset.wait_closed(SOON).unwrap();
    assert_eq!(headset.view().goodbye, Some(PcGoodbye::Forgotten.code()));
    pc.wait_log(SOON, "keeping virtual monitors for 30 s");
    let again = FakeHeadset::connect(pc.addr, &mut keys, None);
    assert!(matches!(again, Err(ConnectError::NotPaired)));
}

/// A headset that leaves on purpose says why, and the PC writes it down; the
/// monitors stay for the next session.
#[test]
fn a_headset_that_leaves_says_why() {
    let (_dir, mut pc, _keys, headset) = session("bye-headset", &Options::default());
    headset.leave(HeadsetGoodbye::OtherPc).unwrap();
    pc.wait_log(SOON, "the headset chose another PC");
    pc.wait_log(SOON, "keeping virtual monitors for 30 s");
    assert_eq!(pc.count_log("saying goodbye"), 0, "no goodbye back");
}
