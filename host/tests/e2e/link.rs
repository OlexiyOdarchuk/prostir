//! How late PC sound reaches the headset while big keyframes go out, on a link shaped
//! like a home Wi-Fi network. `scripts/e2e-link.sh` runs this test inside a private
//! network namespace whose loopback `tc netem` limits (100 Mbit/s and 2 ms by
//! default); on a plain loopback it runs too, but the numbers then say little.
//!
//! The targets, for 100 Mbit/s and 2 ms: 95 % of the slices within 12 ms, none later
//! than 25 ms, and no pause in the sound over 30 ms. Before packets and pacing (0.2.0
//! step 3) the same link gave 26 to 32 ms, 70 to 90 ms and 13 to 23 such pauses.

use std::time::{Duration, Instant};

use prostir_proto::ClientMsg;
use prostir_testkit::View;

use crate::pc::{Options, Pc, TestDir};
use crate::{START, connect};

/// Three monitors with a busy picture.
const THREE_MONITORS: &str = "row\n1280x720\n1280x720\n1280x720\nmirror off\n";
/// How long the sound is measured.
const MEASURE: Duration = Duration::from_secs(15);
/// Every this often, every stream is asked for a keyframe at once (as when a session
/// opens or monitors are added).
const KEYFRAMES_EVERY: Duration = Duration::from_secs(2);

/// What sound may take at most, on the default shaped link: 95 % of the slices, and
/// all of them.
const TARGET_P95: Duration = Duration::from_millis(12);
const TARGET_MAX: Duration = Duration::from_millis(25);

/// The value below which a share `p` of `sorted` lies.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

#[test]
#[ignore = "measures a shaped link: run scripts/e2e-link.sh"]
fn sound_under_keyframes_on_a_shaped_link() {
    let options = Options {
        fps: 30,
        noise: true,
        bitrate: 20,
        ..Options::default()
    };
    let dir = TestDir::new("link");
    dir.save_monitors(THREE_MONITORS);
    let mut keys = dir.pair();
    let mut pc = Pc::start(&dir, &options);
    // Over the network, as on Wi-Fi: the script gives the loopback an address of
    // that kind (127.0.0.1 is how a headset on USB comes, and that is not paced).
    if let Ok(ip) = std::env::var("PROSTIR_LINK_IP") {
        pc.addr
            .set_ip(ip.parse().expect("PROSTIR_LINK_IP: an IP address"));
    }
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    // The keyframes of the start are out of the way.
    headset.take_for(Duration::from_secs(2)).unwrap();
    headset.reset_counts();
    let ids = headset.view().order.clone();
    let reports_before = pc.count_log(": sent ");
    let start = Instant::now();
    let mut next_keyframes = start;
    while start.elapsed() < MEASURE {
        if Instant::now() >= next_keyframes {
            for &id in &ids {
                headset.send(&ClientMsg::RequestKeyframe { id }).unwrap();
            }
            next_keyframes += KEYFRAMES_EVERY;
        }
        let until = next_keyframes.min(start + MEASURE);
        headset
            .recv(until.saturating_duration_since(Instant::now()))
            .unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    let view = headset.view();
    let mut delays = view.sound_delays.clone();
    delays.sort_unstable();
    assert!(delays.len() > 1000, "sound came: {} slices", delays.len());
    let over = view.sound_gaps.over();
    let streams = view.streams.values();
    let video_bytes: u64 = streams.clone().map(|s| s.bytes).sum();
    let keyframes: u64 = streams.clone().map(|s| s.keyframes).sum();
    let keyframe_bytes: u64 = streams.clone().map(|s| s.keyframe_bytes).sum();
    let keyframe_max = streams.map(|s| s.keyframe_max).max().unwrap_or(0);
    eprintln!(
        "link {}: sound delay p50 {:.1} p95 {:.1} p99 {:.1} max {:.1} ms ({} slices); \
         pauses over 30/50/100/250 ms: {}/{}/{}/{}; video {:.1} Mbit/s, {keyframes} \
         keyframes, avg {:.0} KB, max {:.0} KB",
        std::env::var("PROSTIR_LINK").unwrap_or_else(|_| "loopback".into()),
        ms(percentile(&delays, 0.5)),
        ms(percentile(&delays, 0.95)),
        ms(percentile(&delays, 0.99)),
        ms(percentile(&delays, 1.0)),
        delays.len(),
        over[0],
        over[1],
        over[2],
        over[3],
        video_bytes as f64 * 8.0 / secs / 1e6,
        keyframe_bytes as f64 / keyframes.max(1) as f64 / 1024.0,
        keyframe_max as f64 / 1024.0,
    );
    // How the frames fared, from the PC's reports (every 10 s): the ones from the
    // measurement on, up to one logged after it.
    let last = pc.count_log("Prostir-3: sent");
    pc.wait_log_after(Duration::from_secs(15), "Prostir-3: sent", last);
    for line in pc.lines(": sent ").iter().skip(reports_before) {
        let report = line.split_once("] ").map_or(line.as_str(), |(_, r)| r);
        eprintln!("  {report}");
    }
    for line in pc.lines("pacing") {
        let report = line.split_once("] ").map_or(line.as_str(), |(_, r)| r);
        eprintln!("  {report}");
    }
    // Only the default link has targets; another rate or delay is for looking.
    if std::env::var("PROSTIR_LINK").is_ok_and(|l| l == "100mbit 2ms") {
        assert!(
            percentile(&delays, 0.95) <= TARGET_P95,
            "sound p95 over the target"
        );
        assert!(
            percentile(&delays, 1.0) <= TARGET_MAX,
            "sound later than the target"
        );
        assert_eq!(over[0], 0, "pauses in the sound over 30 ms");
    }
    pc.quit();
}
