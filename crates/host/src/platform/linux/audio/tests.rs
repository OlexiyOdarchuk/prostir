use std::time::Instant;

use super::*;

fn args(cmd: &Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// A remembered mute on the internal input, or a remembered near-zero volume on
/// the stream into it, silenced the microphone on every run.
#[test]
fn the_internal_path_takes_no_remembered_volume() {
    let lb = args(&pw_loopback("mic", "feed"));
    let after = |flag: &str| &lb[lb.iter().position(|a| a == flag).unwrap() + 1];
    assert!(after("--capture-props").contains("state.restore-props=false"));
    // The microphone apps see keeps the user's volume.
    assert!(!after("--playback-props").contains("restore-props"));
    let pa = args(&pacat("feed"));
    assert!(pa.iter().any(|a| a == "--volume=65536"));
    assert!(
        pa.iter()
            .any(|a| a == "--property=state.restore-props=false")
    );
    assert!(pa.iter().any(|a| a == "--device=feed"));
}

/// WirePlumber stores nothing of the headset's sound device (its level is the
/// headset's, a stored one would fight it), nor of a harness's.
#[test]
fn the_sound_device_is_not_remembered() {
    let props = node_props(SINK, "Prostir", "Audio/Sink");
    assert!(props.contains("state.restore-props=false"), "{props}");
    assert!(props.contains("node.name=prostir_headset "), "{props}");
}

/// Nobody takes the sound: the session loop does not wait, at most a pipe's worth
/// waits, and the rest is dropped in whole slices.
#[test]
fn a_full_feed_drops_instead_of_waiting() {
    let mut feed = Feed::spawn(Command::new("sleep").arg("10")).unwrap();
    let slice = vec![1u8; 960];
    let started = Instant::now();
    for _ in 0..100 {
        assert!(feed.write(&slice));
    }
    assert!(started.elapsed() < Duration::from_millis(500));
    let kept = 100 * slice.len() - feed.dropped;
    assert!(kept > 0 && kept <= FEED_PIPE, "{kept} bytes wait");
    assert_eq!(feed.dropped % slice.len(), 0);
}

#[test]
fn a_stopped_player_is_noticed() {
    let mut feed = Feed::spawn(&mut Command::new("true")).unwrap();
    let _ = feed.pacat.wait();
    // Another test's child forked at the same moment holds the pipe's other end
    // until it runs its program: give it that moment.
    let until = Instant::now() + Duration::from_secs(2);
    while feed.write(&[0; 960]) {
        assert!(Instant::now() < until, "the stopped player was not noticed");
        std::thread::sleep(Duration::from_millis(10));
    }
}
