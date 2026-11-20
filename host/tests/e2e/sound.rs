//! One volume for the PC's slider and the headset's buttons, and the headset's
//! microphone as an up stream.

use std::time::{Duration, Instant};

use prostir_proto::ClientMsg;
use prostir_proto::state::{UpKind, UpState, permission};
use prostir_testkit::{FIRST_VOLUME, FakeHeadset, quest_step};

use crate::pc::{Options, Pc};
use crate::{SOON, session};

/// The levels the PC put its sound device at, in order (from the log).
fn written(pc: &Pc) -> Vec<String> {
    pc.lines("fake desktop: volume set to ")
        .iter()
        .filter_map(|l| l.split("volume set to ").nth(1).map(str::to_owned))
        .collect()
}

/// Takes what the PC sends until the device was last set to `level`, for up to `SOON`.
fn wait_written(pc: &Pc, headset: &mut FakeHeadset, level: u8) {
    let until = Instant::now() + SOON;
    while written(pc).last().map(String::as_str) != Some(&level.to_string()) {
        assert!(
            Instant::now() < until,
            "the device at {level}: {:?}",
            written(pc)
        );
        headset.take_for(Duration::from_millis(50)).unwrap();
    }
}

/// The headset's level comes in `Open` and goes to the device, its buttons too, and
/// nothing comes back for them; the slider asks the headset once and ends on its step;
/// dragged, the device is not set until it rests, and then once; quiet after.
#[test]
fn one_volume_for_the_slider_and_the_buttons() {
    let (_dir, pc, _keys, mut headset) = session("volume", &Options::default());
    wait_written(&pc, &mut headset, FIRST_VOLUME);
    headset.press_volume(73).unwrap();
    wait_written(&pc, &mut headset, 73);
    headset.take_for(Duration::from_millis(500)).unwrap();
    assert!(
        headset.view().volume_asks.is_empty(),
        "nothing asked for the headset's own changes"
    );

    // The slider.
    pc.desktop("volume 21");
    wait_written(&pc, &mut headset, quest_step(21));
    assert_eq!(headset.volume(), Some(quest_step(21)));
    assert_eq!(headset.view().volume_asks.len(), 1);

    // Dragged from 0 to 100 in a second: the slider never jumps back while it moves,
    // and rests on the headset's level. (A level may be written on the way where the
    // drag itself stood still a moment, a busy test machine, say: what the slider
    // must never do is go back.)
    let before = written(&pc).len();
    let asks_before = headset.view().volume_asks.len();
    pc.desktop("drag 0 100 1000");
    wait_written(&pc, &mut headset, 100);
    let during: Vec<u8> = written(&pc)[before..]
        .iter()
        .map(|l| l.parse().unwrap())
        .collect();
    assert!(
        during.windows(2).all(|w| w[0] <= w[1]),
        "the slider went back: {during:?}"
    );
    assert_eq!(during.last(), Some(&100));
    let asks = headset.view().volume_asks.len() - asks_before;
    assert!((1..=25).contains(&asks), "{asks} requests");
    assert_eq!(headset.volume(), Some(100));
    // Quiet once it rests.
    let asks = headset.view().volume_asks.len();
    headset.take_for(Duration::from_secs(2)).unwrap();
    assert_eq!(headset.view().volume_asks.len(), asks);
    assert_eq!(written(&pc).len(), before + during.len());
}

/// The slider and the buttons at the same moment (the PC's request and the headset's
/// change cross on the way): the headset's level wins.
#[test]
fn at_the_same_moment_the_headset_wins() {
    let (_dir, mut pc, _keys, mut headset) = session("volume-race", &Options::default());
    wait_written(&pc, &mut headset, FIRST_VOLUME);
    // The headset reads nothing for a moment: the PC's request waits on the way.
    headset.stall(Duration::from_secs(1));
    pc.desktop("volume 90");
    pc.wait_log(SOON, "fake desktop: volume moved to 90");
    // The PC looks at the device at least every 150 ms: its request is out by then.
    std::thread::sleep(Duration::from_millis(400));
    headset.press_volume(13).unwrap();
    wait_written(&pc, &mut headset, 13);
    headset.take_for(Duration::from_secs(1)).unwrap();
    assert_eq!(headset.volume(), Some(13));
    assert_eq!(written(&pc).last().map(String::as_str), Some("13"));
}

/// While a program on the PC records, the PC asks for the microphone, and the
/// headset's stream reaches the PC's end of it; a microphone the system does not let
/// record shows on the PC.
#[test]
fn the_microphone_streams_while_a_program_records() {
    let (_dir, mut pc, _keys, mut headset) = session("microphone", &Options::default());
    let wanted = |h: &FakeHeadset| h.view().pc.up_wanted.get(&UpKind::Mic).map(|u| u.on);
    assert_eq!(wanted(&headset), Some(false));
    pc.desktop("record on");
    headset
        .wait(SOON, "the microphone wanted", |v| {
            v.pc.up_wanted.get(&UpKind::Mic).is_some_and(|u| u.on)
        })
        .unwrap();
    headset.send_mic(150).unwrap();
    pc.wait_log(SOON, "fake desktop: the microphone got");
    pc.wait_log(SOON, "the headset's microphone stopped");
    pc.desktop("record off");
    headset
        .wait(SOON, "the microphone no longer wanted", |v| {
            v.pc.up_wanted.get(&UpKind::Mic).is_some_and(|u| !u.on)
        })
        .unwrap();

    let denied = UpState {
        kind: UpKind::Mic,
        switch: true,
        permission: permission::DENIED,
    };
    headset.send(&ClientMsg::UpState { part: denied }).unwrap();
    pc.wait_log(SOON, "the headset's microphone has no permission");
}
