//! PC sound to the headset: a virtual output device ("Prostir — шолом") that is the
//! default while the headset is connected; whatever plays into it is captured as raw
//! PCM and sent over the (encrypted) connection. And back: the headset's microphone
//! as the PC's default input ([`MicIn`]).

use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use log::{info, warn};
use prostir_proto::{AUDIO_CHANNELS, AUDIO_RATE, MIC_CHANNELS};

use crate::platform::run;

const SINK: &str = "prostir_headset";

/// `pw-cli` and `pactl` answer at once while PipeWire is well.
const TIMEOUT: Duration = Duration::from_secs(3);

/// A PipeWire node created with `pw-cli`; destroyed on drop.
struct VirtualNode {
    name: &'static str,
}

impl VirtualNode {
    fn create(name: &'static str, description: &str, class: &str) -> Result<Self> {
        destroy_node(name); // left over from a crash
        let props = format!(
            "{{ factory.name=support.null-audio-sink node.name={name} \
             node.description=\"{description}\" media.class={class} \
             audio.position=[ FL FR ] audio.rate={AUDIO_RATE} object.linger=true }}"
        );
        let out = run(
            Command::new("pw-cli").args(["create-node", "adapter", &props]),
            TIMEOUT,
        )
        .context("pw-cli is needed for sound (PipeWire)")?;
        if !out.status.success() {
            bail!("pw-cli: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        // The node shows up asynchronously.
        for _ in 0..20 {
            if node_id(name).is_some() {
                return Ok(Self { name });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("PipeWire did not create {name}")
    }
}

impl Drop for VirtualNode {
    fn drop(&mut self) {
        destroy_node(self.name);
    }
}

fn node_id(name: &str) -> Option<u32> {
    let out = run(Command::new("pw-cli").args(["info", name]), TIMEOUT).ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("id: ")?.trim().parse().ok())
}

fn destroy_node(name: &str) {
    if let Some(id) = node_id(name) {
        let _ = run(
            Command::new("pw-cli").args(["destroy", &id.to_string()]),
            TIMEOUT,
        );
    }
}

/// Removes the sound devices a previous run (killed, crashed) left behind. The sink
/// lingers by design, and while it is gone PipeWire falls back to a real output.
pub fn restore_after_crash() {
    for name in [SINK, MIC, MIC_FEED] {
        destroy_node(name);
    }
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = run(Command::new("pactl").args(args), TIMEOUT).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Sound going to the headset while this exists.
pub struct AudioOut {
    pipeline: gst::Pipeline,
    previous_default: Option<String>,
    // Dropped last: the device disappears after capture stops.
    _sink: VirtualNode,
}

impl AudioOut {
    /// `on_sound` gets each slice of PC sound (raw PCM) as it is captured.
    pub fn start(on_sound: impl Fn(Vec<u8>) + Send + Sync + 'static) -> Result<Self> {
        let sink = VirtualNode::create(
            SINK,
            crate::i18n::t("Prostir — шолом", "Prostir headset"),
            "Audio/Sink",
        )?;
        let previous_default = pactl(&["get-default-sink"]).filter(|s| s != SINK);
        // Low latency: 10 ms slices, at most 40 ms buffered on the PC.
        let desc = format!(
            "pulsesrc device={SINK}.monitor buffer-time=40000 latency-time=10000 \
             ! audio/x-raw,format=S16LE,rate={AUDIO_RATE},channels={AUDIO_CHANNELS} \
             ! appsink name=sink sync=false max-buffers=8 drop=true"
        );
        let pipeline = gst::parse::launch(&desc)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let appsink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("appsink missing"))?;
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(buffer) = sample.buffer()
                        && let Ok(map) = buffer.map_readable()
                    {
                        on_sound(map.as_slice().to_vec());
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.set_state(gst::State::Playing)?;
        // Like plugging in headphones: sound follows the headset.
        if pactl(&["set-default-sink", SINK]).is_none() {
            warn!("cannot make {SINK} the default output");
        }
        info!("PC sound goes to the headset");
        Ok(Self {
            pipeline,
            previous_default,
            _sink: sink,
        })
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        // Give the sound back to the PC, unless the user picked something else meanwhile.
        if let Some(prev) = &self.previous_default
            && pactl(&["get-default-sink"]).as_deref() == Some(SINK)
        {
            let _ = pactl(&["set-default-sink", prev]);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        info!("PC sound back on the PC");
    }
}

const MIC: &str = "prostir_mic";
const MIC_FEED: &str = "prostir_mic_feed";

/// The longest the headset's sound waits in the feed stream (`pacat`'s latency).
const FEED_LATENCY_MS: u32 = 20;
/// The pipe to `pacat`: one page, about 40 ms of sound. When it is full, the PC
/// records slower than the headset sends, and new sound is dropped instead of piling
/// up. Writes of up to this size go through whole or not at all.
const FEED_PIPE: usize = 4096;

/// The headset microphone as a PC microphone ("Prostir — мікрофон шолома"), the
/// default input while the headset streams. A `pw-loopback` pairs an internal input
/// with the microphone apps record from; while an app records, a `pacat` plays the
/// headset's sound into that input.
///
/// No sound comes out late: each recording gets a new `pacat`, so nothing from an
/// earlier one comes out and nothing waits in between, and during a recording at most
/// ~60 ms waits on the PC. The internal input is always at full volume and never
/// muted: WirePlumber does not remember its settings (it used to, keyed by its name,
/// so a mute once set silenced the microphone on every later run), and they are set
/// again at the start of each recording.
/// The volume of the sound device for the headset ("Prostir: headset"), as the PC's
/// slider and keys show it. Followed with `pactl subscribe` from 0.2.0 step 6; until
/// then there is none, and the PC says it cannot share the volume.
pub struct SinkVolume;

impl SinkVolume {
    pub fn open() -> Option<Self> {
        None
    }

    /// Puts the device at `level` percent, `muted` or not.
    pub fn set(&self, _level: u8, _muted: bool) {}

    /// A change of the device since the last call, if there was one, and when it
    /// happened.
    pub fn changed(&self) -> Option<(u8, bool, std::time::Instant)> {
        None
    }

    /// Where the device is now, if known.
    pub fn level(&self) -> Option<(u8, bool)> {
        None
    }
}

pub struct MicIn {
    loopback: Child,
    mic: String,
    feed_sink: String,
    /// The way in while an app records; none in between.
    feed: Mutex<Option<Feed>>,
    previous_default: Option<String>,
}

impl MicIn {
    pub fn start() -> Result<Self> {
        Self::start_as(MIC, MIC_FEED, true)
    }

    /// `make_default`: the microphone becomes the PC's default input (and the old
    /// default comes back when it goes).
    fn start_as(mic: &str, feed: &str, make_default: bool) -> Result<Self> {
        // Needed for each recording; missing, the microphone waits and tries later.
        run(Command::new("pacat").arg("--version"), TIMEOUT)
            .context("pacat is needed for the microphone")?;
        let mut loopback = spawn_tied(&mut pw_loopback(mic, feed))
            .context("pw-loopback is needed for the microphone")?;
        let ready = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            source_state(mic).is_some()
        });
        if !ready {
            let _ = loopback.kill();
            let _ = loopback.wait();
            bail!("the virtual microphone did not appear");
        }
        let previous_default = if make_default {
            let prev = pactl(&["get-default-source"]).filter(|s| s != mic);
            if pactl(&["set-default-source", mic]).is_none() {
                warn!("cannot make {mic} the default microphone");
            }
            prev
        } else {
            None
        };
        info!("headset microphone is the PC's microphone");
        Ok(Self {
            loopback,
            mic: mic.to_owned(),
            feed_sink: feed.to_owned(),
            feed: Mutex::new(None),
            previous_default,
        })
    }

    /// Plays a slice of the headset's microphone into the PC microphone, if an app
    /// records; dropped otherwise.
    pub fn push(&self, pcm: Vec<u8>) {
        let mut feed = self.feed.lock().unwrap();
        if let Some(f) = feed.as_mut()
            && !f.write(&pcm)
        {
            warn!("headset microphone: pacat stopped");
            *feed = None;
        }
    }

    /// Whether some PC app is recording from the microphone right now (asked every
    /// half second). A recording that begins gets a fresh way in; one that ends takes
    /// whatever still waits with it.
    pub fn recording(&self) -> bool {
        let on = source_state(&self.mic).is_some_and(|s| s == "RUNNING");
        let mut feed = self.feed.lock().unwrap();
        if !on {
            *feed = None;
        } else if feed.is_none() {
            // Full volume, not muted: whatever someone set on the internal input.
            let _ = pactl(&["set-sink-mute", &self.feed_sink, "0"]);
            let _ = pactl(&["set-sink-volume", &self.feed_sink, "100%"]);
            *feed = Feed::start(&self.feed_sink)
                .inspect_err(|e| warn!("no headset microphone: {e:#}"))
                .ok();
        }
        on
    }
}

impl Drop for MicIn {
    fn drop(&mut self) {
        if let Some(prev) = &self.previous_default
            && pactl(&["get-default-source"]).as_deref() == Some(self.mic.as_str())
        {
            let _ = pactl(&["set-default-source", prev]);
        }
        *self.feed.get_mut().unwrap() = None;
        let _ = self.loopback.kill();
        let _ = self.loopback.wait();
        info!("microphone back on the PC");
    }
}

/// One recording's way in: `pacat` plays what comes through a small pipe into the
/// loopback's internal input. Its stream waits for sound after a gap (before the
/// headset's microphone comes on, a Wi-Fi hiccup). GStreamer's `pulsesink`, used
/// before, asks for one that never waits: the sound system skips ahead over the gap,
/// and about as much of the sound that comes next was thrown away (up to half a
/// second at the start of a recording).
struct Feed {
    pacat: Child,
    pipe: std::process::ChildStdin,
    /// Bytes dropped because the pipe was full.
    dropped: usize,
}

impl Feed {
    fn start(sink: &str) -> Result<Self> {
        Self::spawn(&mut pacat(sink)).context("pacat is needed for the microphone")
    }

    /// Starts `cmd`, which plays what comes on its standard input.
    fn spawn(cmd: &mut Command) -> Result<Self> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut pacat = spawn_tied(cmd)?;
        let pipe = pacat.stdin.take().ok_or_else(|| anyhow!("no pipe"))?;
        let fd = pipe.as_raw_fd();
        // SAFETY: fcntl on a pipe this process owns.
        unsafe {
            libc::fcntl(fd, libc::F_SETPIPE_SZ, FEED_PIPE as libc::c_int);
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Ok(Self {
            pacat,
            pipe,
            dropped: 0,
        })
    }

    /// `false` once `pacat` is gone.
    fn write(&mut self, pcm: &[u8]) -> bool {
        use std::io::Write;
        // Whole samples only, in pieces the pipe takes whole or not at all.
        for piece in pcm[..pcm.len() & !1].chunks(FEED_PIPE) {
            match self.pipe.write(piece) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.dropped += piece.len();
                }
                Err(_) => return false,
            }
        }
        true
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        let _ = self.pacat.kill();
        let _ = self.pacat.wait();
        if self.dropped > 0 {
            let ms = self.dropped * 1000 / (AUDIO_RATE as usize * 2 * MIC_CHANNELS as usize);
            info!("headset microphone: {ms} ms dropped (the PC took it slower than it came)");
        }
    }
}

/// `pw-loopback` making the microphone `mic` and its internal input `feed`.
fn pw_loopback(mic: &str, feed: &str) -> Command {
    let mut cmd = Command::new("pw-loopback");
    cmd.args([
        "-n",
        "prostir-mic",
        "--capture-props",
        // Never restored (see `MicIn`); the microphone itself keeps what the user sets.
        &format!(
            "media.class=Audio/Sink audio.position=[ MONO ] node.name={feed} \
             node.description=\"{}\" state.restore-props=false",
            crate::i18n::t("Prostir (службовий)", "Prostir (internal)")
        ),
        "--playback-props",
        &format!(
            "media.class=Audio/Source audio.position=[ MONO ] node.name={mic} \
             node.description=\"{}\"",
            crate::i18n::t("Prostir — мікрофон шолома", "Prostir headset microphone")
        ),
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    cmd
}

/// `pacat` playing raw headset sound from its standard input into `sink`.
fn pacat(sink: &str) -> Command {
    let mut cmd = Command::new("pacat");
    cmd.args([
        "--playback",
        "--raw",
        &format!("--device={sink}"),
        "--format=s16le",
        &format!("--rate={AUDIO_RATE}"),
        &format!("--channels={MIC_CHANNELS}"),
        &format!("--latency-msec={FEED_LATENCY_MS}"),
        // Full volume, whatever the sound system remembers for "prostir" (a near-zero
        // volume once stuck to Prostir's stream and came back on every run).
        "--volume=65536",
        "--property=state.restore-props=false",
        "--client-name=prostir",
        "--stream-name=headset microphone",
    ]);
    cmd
}

/// Spawns a helper that dies with Prostir, even if Prostir crashes.
fn spawn_tied(cmd: &mut Command) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: prctl only.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    cmd.spawn()
}

/// `RUNNING`, `IDLE` or `SUSPENDED` for a source, from `pactl list short sources`.
fn source_state(name: &str) -> Option<String> {
    let list = pactl(&["list", "short", "sources"])?;
    list.lines().find_map(|l| {
        let cols: Vec<&str> = l.split('\t').collect();
        (cols.get(1) == Some(&name)).then(|| cols.last().unwrap_or(&"").trim().to_owned())
    })
}

#[cfg(test)]
mod tests {
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
}

#[cfg(test)]
mod mic_harness {
    //! Measures the headset microphone path on this PC's PipeWire (with its PulseAudio
    //! server, and `parecord`): `cargo test -p prostir-host -- --ignored mic_harness
    //! --nocapture`. It makes its own devices (`prostir_test_mic_<pid>`, gone when it
    //! ends), leaves Prostir's and the default input alone, and can run while Prostir
    //! does. It prints, for each case, how much sound was lost, old or late, and the
    //! latency the PC adds (with the recording app's 10 ms).
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;

    const BLOCK: usize = AUDIO_RATE as usize / 100;

    /// 10 ms of sound whose every sample is the block number (never 0).
    fn block(n: u32) -> Vec<u8> {
        let v = (n % 30000 + 1) as i16;
        (0..BLOCK)
            .flat_map(|i| if i % 2 == 0 { v } else { -v }.to_le_bytes())
            .collect()
    }

    /// Samples, by when they came.
    type Recorded = Vec<(Instant, Vec<i16>)>;

    /// An app recording from `device` (`parecord`, 10 ms fragments): what arrives
    /// and when.
    struct Reader {
        child: std::process::Child,
        got: Arc<Mutex<Recorded>>,
    }

    impl Reader {
        fn start(device: &str) -> Self {
            use std::io::Read;
            let mut child = Command::new("parecord")
                .args([
                    "-d",
                    device,
                    "--raw",
                    "--format=s16le",
                    "--rate=48000",
                    "--channels=1",
                    "--latency-msec=10",
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut out = child.stdout.take().unwrap();
            let got = Arc::new(Mutex::new(Vec::new()));
            let store = got.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                // A sample split between two reads waits for its second byte.
                let mut have = 0;
                while let Ok(n) = out.read(&mut buf[have..]) {
                    if n == 0 {
                        break;
                    }
                    have += n;
                    let (samples, _) = buf[..have].as_chunks::<2>();
                    let pcm = samples
                        .iter()
                        .map(|&c| i16::from_le_bytes(c).saturating_abs())
                        .collect();
                    store.lock().unwrap().push((Instant::now(), pcm));
                    buf.copy_within(have & !1..have, 0);
                    have %= 2;
                }
            });
            Self { child, got }
        }

        fn stop(mut self) -> Recorded {
            let _ = self.child.kill();
            let _ = self.child.wait();
            std::mem::take(&mut self.got.lock().unwrap())
        }
    }

    /// The headset: pushes block after block every 10 ms.
    struct Headset {
        next: u32,
        pushed: Vec<Instant>,
    }

    impl Headset {
        fn talk(&mut self, mic: &MicIn, for_ms: u64) {
            let start = Instant::now();
            let first = self.next;
            while start.elapsed() < Duration::from_millis(for_ms) {
                let due = start + Duration::from_millis(10 * u64::from(self.next - first));
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                self.pushed.push(Instant::now());
                mic.push(block(self.next));
                self.next += 1;
            }
        }
    }

    /// Whether the way in is open, and how much waits in its pipe.
    fn levels(mic: &MicIn) -> String {
        let feed = mic.feed.lock().unwrap();
        let Some(f) = feed.as_ref() else {
            return "no feed".into();
        };
        let mut queued: libc::c_int = 0;
        // SAFETY: FIONREAD into an int.
        unsafe { libc::ioctl(f.pipe.as_raw_fd(), libc::FIONREAD, &mut queued) };
        format!(
            "feed open, {} ms in the pipe, {} ms dropped",
            queued / 96,
            f.dropped / 96
        )
    }

    /// What a recording got of the blocks in `window`, pushed while it recorded.
    struct Got {
        /// Never came.
        lost_ms: usize,
        /// Of those, at the start of the window.
        lost_first_ms: usize,
        /// Came, but from before `earlier` (an earlier recording, or while nobody
        /// recorded).
        stale_ms: usize,
        /// Came more than 100 ms after it was pushed.
        late_ms: usize,
        median_ms: f64,
        max_ms: f64,
    }

    fn report(
        what: &str,
        got: &Recorded,
        hs: &Headset,
        earlier: u32,
        window: std::ops::Range<u32>,
    ) -> Got {
        // When the last sample of each block came.
        let mut arrived = std::collections::BTreeMap::new();
        for (at, pcm) in got {
            for &s in pcm.iter().filter(|&&s| s > 0) {
                arrived.insert(s as u32 - 1, *at);
            }
        }
        let mut lat: Vec<f64> = arrived
            .iter()
            .filter(|(n, _)| window.contains(*n))
            .map(|(n, at)| at.duration_since(hs.pushed[*n as usize]).as_secs_f64() * 1e3)
            .collect();
        lat.sort_by(f64::total_cmp);
        let pct = |p: f64| {
            lat.get(((lat.len().max(1) - 1) as f64 * p) as usize)
                .copied()
        };
        let r = Got {
            lost_ms: 10 * window.clone().filter(|n| !arrived.contains_key(n)).count(),
            lost_first_ms: 10
                * window
                    .clone()
                    .take_while(|n| !arrived.contains_key(n))
                    .count(),
            stale_ms: 10 * arrived.keys().filter(|&&n| n < earlier).count(),
            late_ms: 10 * lat.iter().filter(|&&l| l > 100.0).count(),
            median_ms: pct(0.5).unwrap_or(f64::NAN),
            max_ms: pct(1.0).unwrap_or(f64::NAN),
        };
        eprintln!(
            "{what}: {} ms sent, {} ms lost ({} ms at the start), {} ms from before, \
             {} ms later than 100 ms; latency ms min {:.0} median {:.0} p95 {:.0} max {:.0}",
            window.len() * 10,
            r.lost_ms,
            r.lost_first_ms,
            r.stale_ms,
            r.late_ms,
            pct(0.0).unwrap_or(f64::NAN),
            r.median_ms,
            pct(0.95).unwrap_or(f64::NAN),
            r.max_ms,
        );
        r
    }

    /// A recording that went as it should: everything came, nothing from before,
    /// nothing late.
    fn assert_clean(g: &Got) {
        assert!(g.lost_ms <= 20, "lost {} ms", g.lost_ms);
        assert_eq!(g.stale_ms, 0, "old sound came out");
        assert_eq!(g.late_ms, 0, "late sound came out");
        assert!(g.median_ms < 50.0, "latency {} ms", g.median_ms);
    }

    /// The whole path on this PC's PipeWire, with throwaway devices (it does not touch
    /// Prostir's own, nor the default input). A test app (`parecord`) records while a
    /// fake headset pushes 10 ms blocks numbered in their samples, so every block
    /// that comes out tells how late it is. `recording()` is called where the PC's
    /// half-second tick would notice the change. Run by hand:
    /// `cargo test -p prostir-host -- --ignored mic_harness --nocapture`.
    #[test]
    #[ignore]
    fn mic_harness() {
        let _ = env_logger::try_init();
        let tag = std::process::id();
        let (name, feed) = (
            format!("prostir_test_mic_{tag}"),
            format!("prostir_test_feed_{tag}"),
        );
        let mic = MicIn::start_as(&name, &feed, false).unwrap();
        let mut hs = Headset {
            next: 0,
            pushed: Vec::new(),
        };
        let tick =
            |mic: &MicIn| eprintln!("   tick: recording {}, {}", mic.recording(), levels(mic));

        // 1. The first recording: the PC notices it, the headset's microphone follows.
        let r = Reader::start(&name);
        std::thread::sleep(Duration::from_millis(250));
        tick(&mic);
        std::thread::sleep(Duration::from_millis(300));
        let from = hs.next;
        hs.talk(&mic, 3000);
        std::thread::sleep(Duration::from_millis(200));
        assert_clean(&report(
            "1 first recording",
            &r.stop(),
            &hs,
            from,
            from..hs.next - 10,
        ));

        // 2. Nobody records, yet sound keeps coming; then an app records while it
        // still comes.
        for _ in 0..3 {
            hs.talk(&mic, 1000);
            tick(&mic);
            assert!(mic.feed.lock().unwrap().is_none());
        }
        let r = Reader::start(&name);
        hs.talk(&mic, 250);
        tick(&mic);
        let from = hs.next;
        hs.talk(&mic, 2000);
        std::thread::sleep(Duration::from_millis(200));
        let what = "2 sound while nobody records, then a recording";
        assert_clean(&report(what, &r.stop(), &hs, from, from..hs.next - 10));
        tick(&mic);

        // 3. The on-demand cycle, three times: the app starts, the PC notices, the
        // headset follows; the app stops, the PC notices, the headset stops.
        for round in 0..3 {
            let r = Reader::start(&name);
            std::thread::sleep(Duration::from_millis(250));
            tick(&mic);
            std::thread::sleep(Duration::from_millis(300));
            let from = hs.next;
            hs.talk(&mic, 2000);
            let to = hs.next;
            let got = r.stop();
            hs.talk(&mic, 250);
            tick(&mic);
            hs.talk(&mic, 300);
            let what = format!("3.{round} on-demand cycle");
            assert_clean(&report(&what, &got, &hs, from, from..to - 10));
            std::thread::sleep(Duration::from_secs(2));
        }

        // 4. A gap (Wi-Fi) in the middle of a recording.
        let r = Reader::start(&name);
        std::thread::sleep(Duration::from_millis(250));
        tick(&mic);
        std::thread::sleep(Duration::from_millis(300));
        let start = hs.next;
        hs.talk(&mic, 1000);
        std::thread::sleep(Duration::from_millis(300));
        let from = hs.next;
        hs.talk(&mic, 1500);
        std::thread::sleep(Duration::from_millis(200));
        assert_clean(&report(
            "4 after a 300 ms gap",
            &r.stop(),
            &hs,
            start,
            from..hs.next - 10,
        ));
        tick(&mic);

        // 5. A burst: a second of sound held up on the way comes at once. At most a
        // few tens of ms of it may still come out; after it, all is as before.
        let r = Reader::start(&name);
        std::thread::sleep(Duration::from_millis(250));
        tick(&mic);
        std::thread::sleep(Duration::from_millis(300));
        let start = hs.next;
        hs.talk(&mic, 1000);
        std::thread::sleep(Duration::from_secs(1));
        let burst = hs.next;
        for _ in 0..100 {
            hs.pushed.push(Instant::now() - Duration::from_secs(1));
            mic.push(block(hs.next));
            hs.next += 1;
        }
        let from = hs.next;
        hs.talk(&mic, 1500);
        std::thread::sleep(Duration::from_millis(200));
        let got = r.stop();
        let held = report("5 the burst itself", &got, &hs, start, burst..from);
        assert!(
            held.late_ms <= 70,
            "{} ms of the burst came out",
            held.late_ms
        );
        let after = report("5 after a burst", &got, &hs, start, from..hs.next - 10);
        assert!(after.lost_first_ms <= 70 && after.median_ms < 50.0 && after.late_ms == 0);
        tick(&mic);
        drop(mic);
    }
}
