//! The PC's control loop: asleep while no headset needs it, a headset back from a
//! lost connection taking over its own session at once, another headset turned away
//! while one has a session, and pairing that waits for the person at the PC without
//! holding anyone else up.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use prostir_testkit::{ConnectError, FakeHeadset, View};

use crate::pc::{Options, Pc, TestDir};
use crate::{SOON, START, TWO_MONITORS, connect, session};

/// How long an idle PC is watched for wakeups.
const WATCH: Duration = Duration::from_secs(3);

/// How many times each thread woke between two looks, by name and id (those that did).
fn woke(
    before: &BTreeMap<u32, (String, u64)>,
    after: &BTreeMap<u32, (String, u64)>,
) -> BTreeMap<String, u64> {
    after
        .iter()
        .map(|(tid, (name, n))| {
            let was = before.get(tid).map_or(0, |(_, n)| *n);
            (format!("{name} ({tid})"), n - was)
        })
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// Waits until what a change of state set going is over (threads winding down after
/// the monitors went, say): the same threads, none of them waking, for a second.
fn settle(pc: &Pc, when: &str) {
    let started = Instant::now();
    loop {
        let before = pc.wakeups();
        std::thread::sleep(Duration::from_secs(1));
        let after = pc.wakeups();
        let same = before.keys().eq(after.keys());
        let woke = woke(&before, &after);
        if same && woke.is_empty() {
            return;
        }
        assert!(
            started.elapsed() < SOON,
            "{when}: the PC app did not settle: {woke:?}, threads {:?} then {:?}",
            before.values().map(|(n, _)| n).collect::<Vec<_>>(),
            after.values().map(|(n, _)| n).collect::<Vec<_>>(),
        );
    }
}

/// Once settled, the PC app does not wake at all for `WATCH`: no thread goes to sleep
/// and wakes again (a loop on a clock would, every time round), and no thread starts
/// or ends (one started now and then to look would not show in the counts).
fn assert_asleep(pc: &Pc, when: &str) {
    settle(pc, when);
    let before = pc.wakeups();
    std::thread::sleep(WATCH);
    let after = pc.wakeups();
    assert!(
        before.keys().eq(after.keys()),
        "{when}: threads came or went in {WATCH:?}: {:?} then {:?}",
        before.values().map(|(n, _)| n).collect::<Vec<_>>(),
        after.values().map(|(n, _)| n).collect::<Vec<_>>(),
    );
    let woke = woke(&before, &after);
    assert!(woke.is_empty(), "{when}: woke in {WATCH:?}: {woke:?}");
}

/// Waiting for a headset, the PC sleeps: before the first session, while it keeps
/// the monitors for the headset to come back, and after that once they are gone; its
/// local API on the session bus too.
#[test]
fn an_idle_pc_sleeps() {
    const LINGER: u64 = 15;
    let dir = TestDir::new("idle");
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let options = Options {
        linger: LINGER,
        ..Options::default()
    };
    let mut pc = Pc::start(&dir, &options);
    // With the local API on its session bus (where there is one), asked once.
    if let Some(status) = pc.ctl(&["status"]) {
        assert_eq!(status.code, 0, "{status:?}");
    }
    assert_asleep(&pc, "before the first headset");
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    headset.disconnect();
    pc.wait_log(SOON, &format!("keeping virtual monitors for {LINGER} s"));
    assert_asleep(&pc, "keeping the monitors");
    pc.wait_log(
        Duration::from_secs(LINGER + 10),
        "the headset did not come back: removing virtual monitors",
    );
    assert_asleep(&pc, "after a session");
    // And it still wakes for a headset.
    let mut again = connect(&pc, &mut keys);
    again
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
}

/// A headset that lost its connection and came back before the PC noticed (its old
/// session has 5 s of silence to go) takes over at once: the new session opens, with
/// the same monitors, and the old one gives way without a goodbye.
#[test]
fn a_headset_back_replaces_its_old_session_at_once() {
    let (_dir, mut pc, mut keys, headset) = session("replaced", &Options::default());
    let ids = headset.view().order();
    // Its Wi-Fi went away: the old connection neither reads nor sends any more.
    headset.stall(Duration::from_secs(60));
    let started = Instant::now();
    let mut back = connect(&pc, &mut keys);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "the snapshot after {took:?}");
    back.wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(back.view().order(), ids, "the same monitors");
    pc.wait_log(SOON, "its earlier session gives way");
    assert_eq!(pc.count_log("fake desktop: created"), 2, "none made anew");
    assert_eq!(
        pc.count_log("saying goodbye"),
        0,
        "no goodbye to the old one"
    );
    // The new session goes on past the old one's silence.
    back.take_for(Duration::from_secs(6)).unwrap();
    assert!(!back.view().closed);
    assert_eq!(pc.count_log("session ended"), 0);
}

/// While one headset has a session, another one is turned away and told why; the
/// first one's session goes on, and the second gets its own once the first leaves.
#[test]
fn another_headset_is_turned_away_while_one_has_a_session() {
    let dir = TestDir::new("another");
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let mut other = dir.pair_headset("other-headset");
    let pc = Pc::start(&dir, &Options::default());
    let mut first = connect(&pc, &mut keys);
    first
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    match FakeHeadset::connect(pc.addr, &mut other, None) {
        Err(ConnectError::Refused(text)) => {
            assert!(text.contains("Another headset"), "{text}");
        }
        Err(e) => panic!("expected Refused, got {e}"),
        Ok(_) => panic!("a second headset got a session"),
    }
    first.take_for(Duration::from_secs(1)).unwrap();
    assert!(!first.view().closed, "the first session goes on");
    first.disconnect();
    let mut second = connect(&pc, &mut other);
    second
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
}

/// A headset pairing waits for the person at the PC to type its code (8 s here):
/// meanwhile a trusted headset connects and gets its monitors as quickly as ever.
#[test]
fn pairing_does_not_hold_up_a_trusted_headset() {
    const THINKING: Duration = Duration::from_secs(8);
    let dir = TestDir::new("pairing-aside");
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let mut stranger = dir.keys_of("stranger");
    let pc = Pc::start(
        &dir,
        &Options {
            typed_code: Some("577215".into()),
            typed_code_delay: THINKING,
            ..Options::default()
        },
    );
    let refused = std::thread::scope(|s| {
        let pairing =
            s.spawn(|| FakeHeadset::connect(pc.addr, &mut stranger, Some(&mut || "577215".into())));
        let asked = Instant::now();
        while pc.lines("types the code in").is_empty() {
            assert!(asked.elapsed() < SOON, "the PC never asked for the code");
            std::thread::sleep(Duration::from_millis(20));
        }
        let started = Instant::now();
        let mut headset = connect(&pc, &mut keys);
        headset
            .wait(START, "every monitor showing", View::all_monitors_show)
            .unwrap();
        let took = started.elapsed();
        assert!(
            !pairing.is_finished() && took < THINKING / 2,
            "the trusted headset's monitors after {took:?}, pairing still waiting"
        );
        let refused = pairing.join().unwrap();
        assert!(!headset.view().closed);
        refused
    });
    // Paired, then turned away: the trusted headset has the session.
    match refused {
        Err(ConnectError::Refused(text)) => assert!(text.contains("Another headset"), "{text}"),
        Err(e) => panic!("expected Refused after pairing, got {e}"),
        Ok(_) => panic!("a second headset got a session"),
    }
    assert_eq!(stranger.trusted.len(), 1, "paired all the same");
}

/// `--linger` as long as a number holds keeps the monitors for good: the PC neither
/// overflows its clock nor removes them, and the headset finds them when it is back.
#[test]
fn a_linger_too_long_to_reckon_keeps_the_monitors() {
    let options = Options {
        linger: u64::MAX,
        ..Options::default()
    };
    let (_dir, mut pc, mut keys, headset) = session("linger-forever", &options);
    headset.disconnect();
    pc.wait_log(
        SOON,
        &format!("keeping virtual monitors for {} s", u64::MAX),
    );
    let mut back = connect(&pc, &mut keys);
    back.wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    assert_eq!(
        pc.count_log("fake desktop: created"),
        2,
        "the same monitors"
    );
    assert_eq!(pc.count_log("panicked"), 0);
}

/// Eight strangers asking to pair at once: one gets the dialog, the others are told
/// another headset is pairing, so the wrong codes counted before each attempt hold
/// for all of them. After three wrong codes pairing pauses, as with one at a time.
#[test]
fn strangers_pairing_at_once_get_one_dialog_and_the_pause() {
    let dir = TestDir::new("pairing-crowd");
    dir.save_monitors(TWO_MONITORS);
    let pc = Pc::start(
        &dir,
        &Options {
            typed_code: Some("000000".into()),
            typed_code_delay: Duration::from_secs(2),
            ..Options::default()
        },
    );
    let wrong = || -> Result<FakeHeadset, ConnectError> {
        // A key of its own each time: strangers, not one headset trying again.
        let mut keys = dir.keys_of(&format!("stranger-{}", rand_name()));
        FakeHeadset::connect(pc.addr, &mut keys, Some(&mut || "111111".into()))
    };
    let said = |r: Result<FakeHeadset, ConnectError>| match r {
        Err(ConnectError::PairingFailed(why)) => why,
        Err(e) => panic!("expected a failed pairing, got {e}"),
        Ok(_) => panic!("a wrong code paired"),
    };
    let crowd: Vec<String> = std::thread::scope(|s| {
        let tries: Vec<_> = (0..8).map(|_| s.spawn(|| said(wrong()))).collect();
        tries.into_iter().map(|t| t.join().unwrap()).collect()
    });
    assert_eq!(pc.lines("types the code in").len(), 1, "{crowd:?}");
    let busy = crowd
        .iter()
        .filter(|why| why.contains("Another headset is pairing"))
        .count();
    assert_eq!(busy, 7, "{crowd:?}");
    assert_eq!(
        crowd
            .iter()
            .filter(|why| why.contains("did not match"))
            .count(),
        1,
        "{crowd:?}"
    );
    // Two more wrong codes are free (three in all), the fourth starts the pause.
    for _ in 0..3 {
        assert!(said(wrong()).contains("did not match"));
    }
    let paused = said(wrong());
    assert!(paused.contains("Too many wrong codes"), "{paused}");
    assert_eq!(
        pc.lines("types the code in").len(),
        4,
        "no dialog while paused"
    );
}

/// A name no other call of this test run gives.
fn rand_name() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A headset forgotten in the tray while its connection waited for the PC (making
/// the monitors for an earlier one takes a while here) gets no session: the trust
/// it was set up with is looked at again when its session would open.
#[test]
fn a_headset_forgotten_meanwhile_gets_no_session() {
    let dir = TestDir::new("forgotten-meanwhile");
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let mut again = dir.keys_of("headset");
    let mut pc = Pc::start(
        &dir,
        &Options {
            slow_monitors: Duration::from_millis(3500),
            ..Options::default()
        },
    );
    let second = std::thread::scope(|s| {
        let first = s.spawn(|| connect(&pc, &mut keys));
        // The PC is making the monitors for the first connection now.
        let asked = Instant::now();
        while pc.lines("headset connected via").is_empty() {
            assert!(asked.elapsed() < SOON, "the first connection never came");
            std::thread::sleep(Duration::from_millis(20));
        }
        pc.tray("forget");
        while pc.lines("fake tray: forget").is_empty() {
            assert!(asked.elapsed() < SOON, "the tray never forgot");
            std::thread::sleep(Duration::from_millis(20));
        }
        // Set up while the PC still trusted it, opened after it forgot.
        let second = FakeHeadset::connect(pc.addr, &mut again, None);
        let mut first = first.join().unwrap();
        first.wait_closed(SOON).unwrap();
        assert_eq!(
            first.view().goodbye,
            Some(prostir_proto::PcGoodbye::Forgotten.code())
        );
        second
    });
    match second {
        Err(ConnectError::Refused(text)) => assert!(text.contains("no longer trusts"), "{text}"),
        Err(e) => panic!("expected Refused, got {e}"),
        Ok(_) => panic!("a forgotten headset got a session"),
    }
    pc.wait_log(SOON, "was forgotten meanwhile");
}
