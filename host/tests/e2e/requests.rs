//! Requests and their replies: each gets exactly one, with its number; what failed
//! says why with a code; what made displays answers with all of them, also those that
//! did not start; only the main headset's choices touch what the PC allows.

use prostir_proto::ClientMsg;
use prostir_proto::request::{Asked, Outcome, Primary, Request};
use prostir_proto::state::{DisplayId, DisplayState, ErrorCode, Mode, Placed, Prefs, ReqId, Row};
use prostir_testkit::View;

use crate::pc::{Options, Pc, TestDir};
use crate::{SOON, START, connect, session};

/// The code a failed reply carries.
fn failed_with(outcome: &Outcome) -> Option<ErrorCode> {
    match outcome {
        Outcome::Failed { error, .. } => Some(error.known()),
        _ => None,
    }
}

/// Over the limit, a size the PC does not make, the last monitor, a display that is
/// not there, a request nobody knows: each answered under its own number with its
/// code, and nothing changed.
#[test]
fn failures_say_why() {
    let (_dir, _pc, _keys, mut headset) = session("failures", &Options::default());
    let limit = usize::from(headset.view().pc.limits.as_ref().unwrap().virtual_max);
    let small = Mode::new(320, 240);
    for _ in 2..limit {
        let added = headset.ask(Request::AddDisplay { mode: small }).unwrap();
        assert!(matches!(added, Outcome::Added { .. }), "{added:?}");
    }
    let req = headset
        .request(Request::AddDisplay { mode: small })
        .unwrap();
    let over = headset.reply(req, SOON).unwrap();
    assert_eq!(failed_with(&over), Some(ErrorCode::LIMIT), "{over:?}");
    assert_eq!(headset.view().order().len(), limit);
    let odd = headset
        .ask(Request::AddDisplay {
            mode: Mode {
                scale: 133,
                ..Mode::new(1920, 1080)
            },
        })
        .unwrap();
    assert_eq!(failed_with(&odd), Some(ErrorCode::BAD_SIZE));
    let tiny = headset
        .ask(Request::AddDisplay {
            mode: Mode::new(10, 10),
        })
        .unwrap();
    assert_eq!(failed_with(&tiny), Some(ErrorCode::BAD_SIZE));
    let nobody = headset
        .ask(Request::RemoveDisplay {
            id: DisplayId(4_000_000),
        })
        .unwrap();
    assert_eq!(failed_with(&nobody), Some(ErrorCode::UNKNOWN_DISPLAY));
    let order = headset.view().order();
    for id in &order[1..] {
        assert_eq!(
            headset.ask(Request::RemoveDisplay { id: *id }).unwrap(),
            Outcome::Done
        );
    }
    let last = headset
        .ask(Request::RemoveDisplay { id: order[0] })
        .unwrap();
    assert_eq!(failed_with(&last), Some(ErrorCode::LAST_DISPLAY));
    // Unknown kind: its number and UNSUPPORTED.
    let unknown = ReqId(9_999);
    headset
        .send(&ClientMsg::Request {
            req: unknown,
            asked: Asked::Unknown { kind: 77 },
        })
        .unwrap();
    let outcome = headset.reply(unknown, SOON).unwrap();
    assert_eq!(failed_with(&outcome), Some(ErrorCode::UNSUPPORTED));
    // Nothing but the replies asked for.
    assert!(
        headset
            .view()
            .replies
            .keys()
            .all(|r| r.0 <= 64 || *r == unknown)
    );
    assert!(
        headset.view().broken.is_empty(),
        "{:?}",
        headset.view().broken
    );
}

/// A monitor that cannot be made is still added: its number comes back, and it stands
/// in the list as failed, with why.
#[test]
fn a_monitor_that_cannot_start_is_added_failed() {
    let options = Options {
        // The third output the fake desktop makes.
        broken_monitors: "3".into(),
        ..Options::default()
    };
    let (_dir, _pc, _keys, mut headset) = session("added-failed", &options);
    let outcome = headset
        .ask(Request::AddDisplay {
            mode: Mode::new(640, 360),
        })
        .unwrap();
    let Outcome::Added { id } = outcome else {
        panic!("added, not {outcome:?}")
    };
    let d = headset.view().display(id).cloned().unwrap();
    let DisplayState::Failed { error, detail } = d.state else {
        panic!("failed, not {:?}", d.state)
    };
    assert_eq!(error.known(), ErrorCode::DESKTOP);
    assert!(detail.contains("broken"), "{detail}");
    assert_eq!(headset.view().order().len(), 3, "it keeps its place");
}

/// A profile's monitors come back numbered in the order of the sizes, with the PC's own
/// displays shown as asked and one of them the primary display; the headset's
/// arrangement moves them on the desktop.
#[test]
fn a_profile_gets_its_numbers_in_order() {
    let options = Options {
        physical: "eDP-1=BOE 0x0a1c 0@1920x1080".into(),
        ..Options::default()
    };
    let (_dir, _pc, _keys, mut headset) = session("profile", &options);
    headset
        .wait(SOON, "the laptop's panel listed", |v| {
            v.displays().iter().any(|d| d.is_physical())
        })
        .unwrap();
    let laptop = headset
        .view()
        .displays()
        .iter()
        .find(|d| d.is_physical())
        .unwrap()
        .id;
    let sizes = [
        Mode::new(800, 450),
        Mode::new(640, 360),
        Mode::new(480, 272),
    ];
    let outcome = headset
        .ask(Request::SetDisplays {
            modes: sizes.to_vec(),
            primary: Some(Primary::Display { id: laptop }),
            shown_physical: vec![laptop],
        })
        .unwrap();
    let Outcome::Ids { ids } = outcome else {
        panic!("numbers, not {outcome:?}")
    };
    assert_eq!(ids.len(), 3);
    let view = headset.view();
    for (id, size) in ids.iter().zip(sizes) {
        let d = view.display(*id).unwrap();
        assert_eq!((d.mode.w, d.mode.h), (size.w, size.h));
    }
    assert_eq!(view.order(), ids, "in the order of the sizes");
    assert_eq!(view.primary(), Some(laptop));
    assert!(view.display(laptop).unwrap().is_shown_physical());
    headset
        .wait(SOON, "every monitor and the laptop's panel showing", |v| {
            v.all_monitors_show() && v.streams.get(&laptop).is_some_and(|s| s.has_picture())
        })
        .unwrap();

    // The arrangement: the laptop's panel over the first monitor.
    let at = |id, middle| Placed {
        id,
        center: Some(0),
        middle: Some(middle),
    };
    headset
        .send(&ClientMsg::Arrange {
            rows: vec![
                Row {
                    entries: vec![at(laptop, -600)],
                },
                Row {
                    entries: ids.iter().map(|id| at(*id, 0)).collect(),
                },
            ],
        })
        .unwrap();
    headset
        .wait(
            SOON,
            "the laptop's panel over the monitors on the desktop",
            |v| {
                let (Some(top), Some(first)) = (
                    v.display(laptop).and_then(|d| d.desk),
                    v.display(ids[0]).and_then(|d| d.desk),
                ) else {
                    return false;
                };
                top.y + top.h as i32 <= first.y
            },
        )
        .unwrap();
}

/// "No limits" of the main headset raises what the PC allows, and the PC makes that
/// many; a guest's (a headset with a line of its own in `settings.txt`) does not, and
/// a guest may not change the monitors.
#[test]
fn only_the_main_headset_changes_the_limits() {
    let (_dir, _pc, _keys, mut headset) = session("no-limits", &Options::default());
    let before = headset.view().pc.limits.clone().unwrap();
    let free = Prefs {
        no_limits: true,
        ..Prefs::default()
    };
    headset.send(&ClientMsg::Prefs { part: free }).unwrap();
    headset
        .wait(SOON, "more allowed", |v| {
            v.pc.limits.as_ref().unwrap().virtual_max > before.virtual_max
        })
        .unwrap();
    let max = headset.view().pc.limits.as_ref().unwrap().virtual_max;
    let made = headset
        .ask(Request::SetDisplays {
            modes: vec![Mode::new(640, 360); usize::from(max)],
            primary: None,
            shown_physical: Vec::new(),
        })
        .unwrap();
    let Outcome::Ids { ids } = made else {
        panic!("{made:?}")
    };
    assert_eq!(ids.len(), usize::from(max));

    let dir = TestDir::new("guest");
    dir.save_screens("next 200\nrow\nscreen 0 640x360\n");
    let mut keys = dir.pair();
    let key: String = keys
        .identity
        .public
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    dir.save_settings(&format!("access {key} guest input\n"));
    let pc = Pc::start(&dir, &Options::default());
    let mut guest = connect(&pc, &mut keys);
    guest
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert!(!guest.view().pc.access.unwrap().is_main());
    guest.send(&ClientMsg::Prefs { part: free }).unwrap();
    guest.take_for(std::time::Duration::from_secs(1)).unwrap();
    assert_eq!(
        guest.view().pc.limits.as_ref().unwrap().virtual_max,
        before.virtual_max
    );
    let refused = guest
        .ask(Request::AddDisplay {
            mode: Mode::new(640, 360),
        })
        .unwrap();
    assert_eq!(failed_with(&refused), Some(ErrorCode::NOT_ALLOWED));
}
