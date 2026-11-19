//! End-to-end tests: the real PC app (built with the fake desktop) driven by a fake
//! headset over a real encrypted connection. Run with `cargo e2e` (an alias for
//! `cargo test -p prostir-host --features fake-desktop --test e2e`). Needs GStreamer
//! with x264enc (plugins-ugly), h264parse (plugins-bad) and the base plugins.

mod pc;

use std::collections::BTreeMap;
use std::time::Duration;

use pc::{Options, Pc, TestDir};
use prostir_proto::{ClientMsg, Codec, PcSettings};
use prostir_testkit::{ConnectError, FakeHeadset, Keys, View};

/// Long enough for a slow CI machine to start GStreamer and the encoders.
const START: Duration = Duration::from_secs(30);
/// A change asked for by the headset shows within this.
const SOON: Duration = Duration::from_secs(10);

/// Two small monitors, so the CPU encoder keeps up anywhere.
const TWO_MONITORS: &str = "row\n640x360\n480x272\nmirror off\n";

/// Every fake monitor's source started exactly once more between `before` and `after`
/// (see [`Pc::source_starts`]).
fn assert_started_once(before: &BTreeMap<String, usize>, after: &BTreeMap<String, usize>) {
    assert!(!before.is_empty(), "no monitor source started");
    let once: BTreeMap<String, usize> = before.iter().map(|(k, n)| (k.clone(), n + 1)).collect();
    assert_eq!(
        after, &once,
        "each monitor's stream starts over exactly once"
    );
}

fn connect(pc: &Pc, keys: &mut Keys) -> FakeHeadset {
    FakeHeadset::connect(pc.addr, keys, None).unwrap_or_else(|e| panic!("connecting: {e}"))
}

/// A started PC app and a headset it already trusts, connected and showing every
/// monitor.
fn session(name: &str, options: &Options) -> (TestDir, Pc, Keys, FakeHeadset) {
    let dir = TestDir::new(name);
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let pc = Pc::start(&dir, options);
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    (dir, pc, keys, headset)
}

#[test]
fn a_trusted_headset_gets_its_monitors_and_sound() {
    let (_dir, pc, _keys, mut headset) = session("trusted", &Options::default());
    assert_eq!(headset.language, "en");
    let view = headset.view();
    assert_eq!(view.order.len(), 2);
    for id in &view.order {
        let s = &view.streams[id];
        assert_eq!(s.codec, Codec::H264, "the CPU encoder makes H.264");
        assert_eq!(
            s.first_is_key,
            Some(true),
            "{} starts with a keyframe",
            s.name
        );
        assert_eq!(s.bad_keyframes, 0);
    }
    assert_eq!(
        (
            view.streams[&view.order[0]].width,
            view.streams[&view.order[1]].width
        ),
        (640, 480)
    );
    let (settings, _) = view.pc_settings.expect("the PC says how it encodes");
    assert_eq!((settings.codec, settings.software), (Codec::H264, true));
    headset.wait(SOON, "sound", |v| v.audio_bytes > 0).unwrap();
    pc.quit();
}

#[test]
fn an_unknown_headset_must_pair_first() {
    let dir = TestDir::new("pairing");
    dir.save_monitors(TWO_MONITORS);
    let pc = Pc::start(
        &dir,
        &Options {
            typed_code: Some("314159".into()),
            ..Options::default()
        },
    );
    let mut keys = dir.headset_keys();
    // Not paired, and no code offered: no session.
    let refused = FakeHeadset::connect(pc.addr, &mut keys, None);
    assert!(matches!(refused, Err(ConnectError::NotPaired)));
    // The person at the PC types another code than the headset shows.
    let wrong = FakeHeadset::connect(pc.addr, &mut keys, Some(&mut || "271828".into()));
    assert!(
        matches!(wrong, Err(ConnectError::PairingFailed(_))),
        "{:?}",
        wrong.err()
    );
    assert!(keys.trusted.is_empty());
    // The right code: paired, and trusted from then on without a code.
    let mut headset = FakeHeadset::connect(pc.addr, &mut keys, Some(&mut || "314159".into()))
        .unwrap_or_else(|e| panic!("pairing: {e}"));
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    headset.disconnect();
    let mut again = connect(&pc, &mut keys);
    again
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
}

#[test]
fn monitors_are_added_and_removed() {
    let (_dir, _pc, _keys, mut headset) = session("add-remove", &Options::default());
    headset
        .send(&ClientMsg::AddMonitor {
            width: 800,
            height: 450,
        })
        .unwrap();
    headset
        .wait(SOON, "a third monitor showing", |v| {
            v.order.len() == 3 && v.all_monitors_show()
        })
        .unwrap();
    let added = *headset.view().order.last().unwrap();
    assert_eq!(headset.view().streams[&added].width, 800);
    headset
        .send(&ClientMsg::RemoveMonitor { id: added })
        .unwrap();
    headset
        .wait(SOON, "the monitor gone", |v| {
            v.ended.contains(&added) && v.order.len() == 2
        })
        .unwrap();
    // The last one stays: the PC says why.
    let first = headset.view().order[0];
    headset
        .send(&ClientMsg::RemoveMonitor { id: first })
        .unwrap();
    let last = headset.view().order[1];
    headset
        .send(&ClientMsg::RemoveMonitor { id: last })
        .unwrap();
    headset
        .wait(SOON, "an error for the last monitor", |v| {
            v.errors.iter().any(|e| e.contains("last monitor"))
        })
        .unwrap();
    assert_eq!(headset.view().order, [last]);
}

/// Taken off for a moment, the monitors pause and come back; for longer than 10 s, the
/// PC gives the desktop back and makes them anew when the headset returns. Meanwhile
/// the PC logs its numbers for each stream. A moving screen, so a pause that still
/// sends frames shows (a still screen coming back is the next test).
#[test]
fn taking_the_headset_off_pauses_and_then_ends_the_session() {
    let options = Options {
        fps: 15,
        ..Options::default()
    };
    let (_dir, mut pc, mut keys, mut headset) = session("presence", &options);
    headset.send(&ClientMsg::Presence { worn: false }).unwrap();
    // Frames already on their way still arrive; after that, nothing.
    headset.take_for(Duration::from_secs(1)).unwrap();
    headset.reset_counts();
    headset.take_for(Duration::from_secs(2)).unwrap();
    let paused: u64 = headset.view().streams.values().map(|s| s.frames).sum();
    assert_eq!(paused, 0, "no frames while the headset is off");
    // Back on: a fresh picture on every monitor (resumed streams start with a keyframe).
    headset.send(&ClientMsg::Presence { worn: true }).unwrap();
    headset
        .wait(SOON, "pictures after putting it on", |v| {
            v.streams.len() == 2 && v.streams.values().all(|s| s.has_picture())
        })
        .unwrap();
    // The numbers each stream reports every 10 s.
    let line = pc.wait_log(Duration::from_secs(15), "Prostir-1: sent");
    assert!(line.contains("waited in outbox p50"), "{line}");
    let reports: u32 = line
        .rsplit_once(" reports)")
        .and_then(|(head, _)| head.rsplit_once('('))
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0);
    assert!(reports > 0, "the headset reported shown frames: {line}");
    pc.wait_log(Duration::from_secs(1), "sound: sent");
    // Off for good: the session ends, and the monitors go.
    headset.send(&ClientMsg::Presence { worn: false }).unwrap();
    headset.wait_closed(Duration::from_secs(15)).unwrap();
    pc.wait_log(
        SOON,
        "removing virtual monitors until the headset is worn again",
    );
    pc.wait_log(SOON, "fake desktop: removed Fake-1");
    // Worn again, the headset connects and the monitors are made anew.
    let mut back = connect(&pc, &mut keys);
    back.wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(pc.count_log("fake desktop: created Fake-1 "), 2);
}

/// Put on again after a moment, a still screen shows on every monitor again though
/// nothing on it changed: the PC starts each stream over once. Before 0.1.1 the
/// picture in the headset stayed frozen.
#[test]
fn a_still_screen_shows_again_after_putting_the_headset_on() {
    let (_dir, mut pc, _keys, mut headset) = session("still", &Options::default());
    headset.send(&ClientMsg::Presence { worn: false }).unwrap();
    pc.wait_log(SOON, "paused: headset not worn or gone");
    headset.take_for(Duration::from_secs(1)).unwrap();
    headset.reset_counts();
    let before = pc.source_starts();
    headset.send(&ClientMsg::Presence { worn: true }).unwrap();
    headset
        .wait(SOON, "pictures after putting it on", |v| {
            v.streams.len() == 2 && v.streams.values().all(|s| s.has_picture())
        })
        .unwrap();
    headset.take_for(Duration::from_secs(1)).unwrap();
    assert_started_once(&before, &pc.source_starts());
}

/// The monitors kept after the headset left send their picture to the headset that
/// comes back, though the (still) screen has not changed: the PC starts their streams
/// over, once each, with the headset there to get the picture. Before 0.1.1 the
/// headset got no monitors at all; in 0.1.1 at first each stream started over twice,
/// once before the headset was attached.
#[test]
fn monitors_kept_after_a_disconnect_show_again() {
    let (_dir, mut pc, mut keys, headset) = session("linger", &Options::default());
    headset.disconnect();
    pc.wait_log(SOON, "keeping virtual monitors for 30 s");
    let before = pc.source_starts();
    let mut back = connect(&pc, &mut keys);
    back.wait(SOON, "every kept monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(back.view().order.len(), 2);
    // Time for a late second start to show in the log.
    back.take_for(Duration::from_secs(1)).unwrap();
    assert_started_once(&before, &pc.source_starts());
    // The same monitors, not new ones.
    assert_eq!(pc.count_log("fake desktop: created"), 2);
    assert_eq!(pc.count_log("fake desktop: removed"), 0);
}

/// Another codec from the headset menu restarts every stream with what the PC really
/// encodes with now: H.265 where the graphics card can, else the CPU's H.264 again.
/// Asking for what is in use already changes nothing.
#[test]
fn a_codec_switch_restarts_every_stream() {
    let (_dir, _pc, _keys, mut headset) = session("codec", &Options::default());
    let starts = |headset: &FakeHeadset| -> Vec<(u8, u32)> {
        let streams = &headset.view().streams;
        streams.iter().map(|(id, s)| (*id, s.starts)).collect()
    };
    let ask = |headset: &mut FakeHeadset, codec, software| {
        let settings = PcSettings {
            quality: 8,
            codec,
            software,
            sharp: false,
        };
        headset
            .send(&ClientMsg::SetPcSettings { settings })
            .unwrap();
    };
    let restarted = |headset: &mut FakeHeadset, before: &[(u8, u32)]| {
        headset
            .wait(SOON, "every stream started over with the new codec", |v| {
                let Some((now, _)) = v.pc_settings else {
                    return false;
                };
                before.iter().all(|(id, n)| {
                    v.streams
                        .get(id)
                        .is_some_and(|s| s.starts > *n && s.codec == now.codec && s.has_picture())
                })
            })
            .unwrap();
        headset.view().pc_settings.unwrap().0
    };

    let before = starts(&headset);
    ask(&mut headset, Codec::H265, false);
    let gpu = restarted(&mut headset, &before);
    eprintln!("asked for H.265 on the graphics card, got {gpu:?}");
    let before = starts(&headset);
    ask(&mut headset, Codec::H264, true);
    if gpu.software {
        // No graphics card encoder (CI): still the CPU's H.264, nothing to restart.
        headset.take_for(Duration::from_secs(2)).unwrap();
        assert_eq!(starts(&headset), before);
    } else {
        assert_eq!(gpu.codec, Codec::H265);
        let cpu = restarted(&mut headset, &before);
        assert_eq!((cpu.codec, cpu.software), (Codec::H264, true));
    }
}
