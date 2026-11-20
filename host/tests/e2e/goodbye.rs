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

/// A development build of the headset with other messages than the PC's (another
/// `+pN` in its version) is turned away before the session, told which build the PC is.
#[test]
fn another_development_build_gets_no_session() {
    let Some(ours) = prostir_proto::PROTO_REVISION else {
        return;
    };
    let dir = crate::pc::TestDir::new("other-build");
    dir.save_monitors(crate::TWO_MONITORS);
    let mut keys = dir.pair();
    let mut pc = crate::pc::Pc::start(&dir, &Options::default());
    let other = format!("0.2.0-dev+p{}", ours + 1);
    match FakeHeadset::connect_as(pc.addr, &mut keys, &other) {
        Err(ConnectError::Incompatible { protocol, app }) => {
            assert_eq!(protocol, prostir_proto::PROTOCOL_VERSION);
            assert_eq!(prostir_proto::revision_of(&app), Some(ours));
        }
        Err(e) => panic!("expected Incompatible, got {e}"),
        Ok(_) => panic!("a session with another build"),
    }
    pc.wait_log(SOON, "reads other messages than this one");
    assert_eq!(pc.count_log("fake desktop: created"), 0, "no monitors made");
}

/// Commands for the headset (from the PC's tray, later `prostir ctl`) reach it and come
/// back with what came of them: done, unsupported for a kind it does not know, and
/// "does not answer" from a headset that hangs, after 30 s. Without a headset, none.
#[test]
fn commands_reach_the_headset_and_come_back() {
    use prostir_proto::request::{Asked, Command};
    let (_dir, mut pc, _keys, mut headset) = crate::session("commands", &Options::default());
    assert_eq!(pc.command("recenter", SOON), "done");
    headset
        .wait(SOON, "the command", |v| {
            v.commands.contains(&Asked::Known(Command::Recenter))
        })
        .unwrap();
    assert_eq!(pc.command("raw 200", SOON), "failed 1: ");
    // From the tray too.
    pc.tray("recenter");
    headset
        .wait(SOON, "the tray's command", |v| v.commands.len() == 3)
        .unwrap();
    headset.stop_answering();
    let answer = std::thread::scope(|s| {
        let waiting = s.spawn(|| pc.command("lineup", std::time::Duration::from_secs(40)));
        // The session goes on meanwhile.
        headset
            .take_for(std::time::Duration::from_secs(32))
            .unwrap();
        waiting.join().unwrap()
    });
    assert_eq!(answer, "the headset does not answer");
    headset.disconnect();
    // Once the PC saw the session end.
    pc.wait_log(SOON, "headset disconnected");
    assert_eq!(pc.command("recenter", SOON), "no headset");
}
