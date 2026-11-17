//! PC sound to the headset: a virtual output device ("Prostir — шолом") that is the
//! default while the headset is connected; whatever plays into it is captured as raw
//! PCM and sent over the (encrypted) connection.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use log::{info, warn};
use prostir_proto::{AUDIO_CHANNELS, AUDIO_RATE, MIC_CHANNELS};

use crate::displays::Router;

const SINK: &str = "prostir_headset";

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
        let out = Command::new("pw-cli")
            .args(["create-node", "adapter", &props])
            .output()
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
    let out = Command::new("pw-cli").args(["info", name]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("id: ")?.trim().parse().ok())
}

fn destroy_node(name: &str) {
    if let Some(id) = node_id(name) {
        let _ = Command::new("pw-cli")
            .args(["destroy", &id.to_string()])
            .output();
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
    let out = Command::new("pactl").args(args).output().ok()?;
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
    pub fn start(router: Arc<Router>) -> Result<Self> {
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
                        router.send_audio(map.as_slice().to_vec());
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

/// The headset microphone as a PC microphone ("Prostir — мікрофон шолома"), the
/// default input while the headset streams. A `pw-loopback` pairs an internal input
/// (which we play the headset's sound into) with the microphone apps record from.
pub struct MicIn {
    appsrc: gst_app::AppSrc,
    pipeline: gst::Pipeline,
    loopback: std::process::Child,
    previous_default: Option<String>,
}

impl MicIn {
    pub fn start() -> Result<Self> {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("pw-loopback");
        cmd.args([
            "-n",
            "prostir-mic",
            "--capture-props",
            &format!(
                "media.class=Audio/Sink audio.position=[ MONO ] node.name={MIC_FEED} \
                 node.description=\"{}\"",
                crate::i18n::t("Prostir (службовий)", "Prostir (internal)")
            ),
            "--playback-props",
            &format!(
                "media.class=Audio/Source audio.position=[ MONO ] node.name={MIC} \
                 node.description=\"{}\"",
                crate::i18n::t("Prostir — мікрофон шолома", "Prostir headset microphone")
            ),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        // SAFETY: prctl only; the loopback dies with us even if we crash.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut loopback = cmd
            .spawn()
            .context("pw-loopback is needed for the microphone")?;
        let ready = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            source_state(MIC).is_some()
        });
        if !ready {
            let _ = loopback.kill();
            let _ = loopback.wait();
            bail!("the virtual microphone did not appear");
        }
        let desc = format!(
            "appsrc name=src is-live=true do-timestamp=true format=time \
             caps=audio/x-raw,format=S16LE,rate={AUDIO_RATE},channels={MIC_CHANNELS},layout=interleaved \
             ! queue max-size-time=100000000 leaky=downstream \
             ! pulsesink device={MIC_FEED} buffer-time=60000 latency-time=10000 sync=false"
        );
        let pipeline = gst::parse::launch(&desc)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let appsrc = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| anyhow!("appsrc missing"))?;
        pipeline.set_state(gst::State::Playing)?;
        let previous_default = pactl(&["get-default-source"]).filter(|s| s != MIC);
        if pactl(&["set-default-source", MIC]).is_none() {
            warn!("cannot make {MIC} the default microphone");
        }
        info!("headset microphone is the PC's microphone");
        Ok(Self {
            appsrc,
            pipeline,
            loopback,
            previous_default,
        })
    }

    /// Plays a slice of the headset's microphone into the PC microphone.
    pub fn push(&self, pcm: Vec<u8>) {
        let _ = self.appsrc.push_buffer(gst::Buffer::from_slice(pcm));
    }

    /// Whether some PC app is recording from the microphone right now.
    pub fn in_use(&self) -> bool {
        source_state(MIC).is_some_and(|s| s == "RUNNING")
    }
}

impl Drop for MicIn {
    fn drop(&mut self) {
        if let Some(prev) = &self.previous_default
            && pactl(&["get-default-source"]).as_deref() == Some(MIC)
        {
            let _ = pactl(&["set-default-source", prev]);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        let _ = self.loopback.kill();
        let _ = self.loopback.wait();
        info!("microphone back on the PC");
    }
}

/// `RUNNING`, `IDLE` or `SUSPENDED` for a source, from `pactl list short sources`.
fn source_state(name: &str) -> Option<String> {
    let list = pactl(&["list", "short", "sources"])?;
    list.lines().find_map(|l| {
        let cols: Vec<&str> = l.split('\t').collect();
        (cols.get(1) == Some(&name)).then(|| cols.last().unwrap_or(&"").trim().to_owned())
    })
}
