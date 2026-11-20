//! The desktop's worker: a desktop or an encoder that hangs holds up neither the
//! requests nor the pings of the headset; the PC says the desktop does not respond
//! after 15 s, is fine again once it does, and ends after too long, to be started
//! again.

use std::time::{Duration, Instant};

use prostir_proto::request::{Outcome, Request};
use prostir_proto::state::{DisplayState, Mode};

use crate::pc::Options;
use crate::{SOON, session};

/// A request, answered within a second: its outcome.
fn ask_at_once(headset: &mut prostir_testkit::FakeHeadset, request: Request) -> Outcome {
    let started = Instant::now();
    let outcome = headset.ask(request).unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(1), "answered after {took:?}");
    outcome
}

/// Laying the desktop out hangs for 20 s (KScreen stuck): a monitor asked for meanwhile
/// is added at once and streams, other requests, the tray and the pings go on; after
/// 15 s the headset, the tray and the local API hear the desktop does not respond, and
/// once it does again, all is as before.
#[test]
fn a_hung_desktop_holds_nothing_up() {
    let (_dir, mut pc, _keys, mut headset) = session("desk-hung", &Options::default());
    headset.take_for(Duration::from_secs(1)).unwrap();
    pc.desktop("hang place 20");
    pc.wait_log(SOON, "the next place hangs for 20 s");
    let added = ask_at_once(
        &mut headset,
        Request::AddDisplay {
            mode: Mode::new(320, 240),
        },
    );
    let Outcome::Added { id } = added else {
        panic!("added, not {added:?}")
    };
    // Listed before it is made: starting.
    assert_eq!(
        headset.view().display(id).map(|d| d.state.clone()),
        Some(DisplayState::Starting)
    );
    pc.wait_log(SOON, "fake desktop: place hangs for 20 s");
    let hung = Instant::now();
    // It streams already: only laying the desktop out hangs.
    headset
        .wait(SOON, "the new monitor shows", |v| {
            v.streams.get(&id).is_some_and(|s| s.has_picture())
        })
        .unwrap();
    // Meanwhile requests are answered at once, the tray answers, the pings go on.
    let pings = headset.view().pings;
    for _ in 0..4 {
        let done = ask_at_once(&mut headset, Request::SetSound { to_headset: true });
        assert_eq!(done, Outcome::Done);
        headset.take_for(Duration::from_millis(500)).unwrap();
    }
    pc.tray("quality 15");
    pc.wait_log(Duration::from_secs(2), "video quality: 15 Mbit/s");
    if let Some(status) = pc.ctl(&["status", "--json"]) {
        assert_eq!(status.code, 0, "{status:?}");
    }
    assert!(
        headset.view().pings > pings,
        "no ping while the desktop hung"
    );
    assert!(
        hung.elapsed() < Duration::from_secs(14),
        "all that was quick"
    );
    // 15 s into the step: the desktop does not respond.
    headset
        .wait(
            Duration::from_secs(20),
            "told the desktop does not respond",
            |v| v.errors.iter().any(|e| e.contains("does not respond")),
        )
        .unwrap();
    let line = pc.wait_log(SOON, "the desktop does not respond:");
    assert!(line.contains("placing the outputs"), "{line}");
    pc.wait_log(SOON, "desktop: not responding");
    if let Some(status) = pc.ctl(&["status", "--json"]) {
        let problems = status.json()["problems"].to_string();
        assert!(problems.contains("Desktop"), "{problems}");
    }
    // Back after 20 s: fine again, the session went on all along.
    pc.wait_log(Duration::from_secs(15), "the desktop responds again");
    let fine = pc.count_log("desktop: fine");
    pc.wait_log_after(SOON, "desktop: fine", fine.saturating_sub(1));
    if let Some(status) = pc.ctl(&["status", "--json"]) {
        let problems = status.json()["problems"].to_string();
        assert!(!problems.contains("Desktop"), "{problems}");
    }
    let done = ask_at_once(&mut headset, Request::SetSound { to_headset: true });
    assert_eq!(done, Outcome::Done);
    assert!(!headset.view().closed);
    assert_eq!(pc.count_log("session ended"), 0);
    assert_eq!(pc.count_log("panicked"), 0);
}

/// An encoder that does not stop (a driver hanging in it) is left behind after 5 s:
/// the monitor is gone at once for the headset, the desktop never counts as not
/// responding, and the next monitor is made and streams.
#[test]
fn an_encoder_that_does_not_stop_is_left_behind() {
    let (_dir, mut pc, _keys, mut headset) = session("desk-encoder-stop", &Options::default());
    let ids = headset.view().order();
    pc.desktop("hang encoder-stop 30");
    pc.wait_log(SOON, "the next encoder-stop hangs for 30 s");
    let done = ask_at_once(&mut headset, Request::RemoveDisplay { id: ids[1] });
    assert_eq!(done, Outcome::Done);
    assert!(headset.view().display(ids[1]).is_none(), "gone at once");
    pc.wait_log(SOON, "fake desktop: encoder-stop hangs for 30 s");
    pc.wait_log(SOON, "did not end within 5s: going on without it");
    let added = ask_at_once(
        &mut headset,
        Request::AddDisplay {
            mode: Mode::new(320, 240),
        },
    );
    let Outcome::Added { id } = added else {
        panic!("added, not {added:?}")
    };
    headset
        .wait(SOON, "the new monitor shows", |v| {
            v.streams.get(&id).is_some_and(|s| s.has_picture())
        })
        .unwrap();
    assert_eq!(pc.count_log("does not respond"), 0);
    assert!(!headset.view().closed);
}

/// A desktop that does not respond for too long (2 min; 5 s here) ends the program
/// with an error, so that systemd or the watcher starts it again; the headset is told
/// goodbye first.
#[test]
fn a_desktop_hung_too_long_ends_the_program() {
    let options = Options {
        desk_limits: Some((2, 5)),
        ..Options::default()
    };
    let (_dir, mut pc, _keys, mut headset) = session("desk-give-up", &options);
    headset.take_for(Duration::from_secs(1)).unwrap();
    pc.desktop("hang place 60");
    pc.wait_log(SOON, "the next place hangs for 60 s");
    ask_at_once(
        &mut headset,
        Request::AddDisplay {
            mode: Mode::new(320, 240),
        },
    );
    pc.wait_log(SOON, "the desktop does not respond:");
    headset.wait_closed(SOON).unwrap();
    assert_eq!(
        headset.view().goodbye,
        Some(prostir_proto::PcGoodbye::Failed.code())
    );
    let status = pc.wait_end(SOON);
    assert_eq!(status.code(), Some(1), "{status}");
    pc.wait_log(SOON, "quitting, to be started again");
}
