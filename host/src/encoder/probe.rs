//! What this PC's encoders can do: each candidate encodes a few frames in a tiny
//! pipeline of the same elements and settings a stream uses, since an element being
//! there does not mean its driver encodes (a VA driver without H.265, an NVIDIA plugin
//! without the NVIDIA driver). A probe that does not end in [`PROBE_TIMEOUT`] counts as
//! not working and holds nothing up: probes run side by side, each on a thread of its
//! own, which is left behind when it hangs.
//!
//! What they found is kept in the user's cache directory (`prostir/encoders.txt`) with
//! a fingerprint of GStreamer and the graphics drivers, and probed again when that
//! changes. It also keeps the session limits learned on the way: an encoder that does
//! not open while others of it run has as many sessions as those (see
//! [`Catalog::learn_limit`]). An encoder that did not open with nothing else running
//! drops out for the rest of the run and the cache is made anew at the next start.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use log::{info, warn};
use prostir_proto::Codec;

use super::choice::{Backend, CodecCaps, EncoderCaps, Kind, Ranked, Wanted, rank};
use super::{EncoderConfig, Input, encoding};
use crate::platform::{AdapterId, Memory};

/// How long one probe may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The probe's picture: small, but not below the smallest size any encoder takes
/// (NVENC H.265 wants more than 128 pixels across).
const PROBE_SIZE: (u32, u32) = (256, 144);

/// The codecs a probe tries.
const CODECS: [Codec; 2] = [Codec::H265, Codec::H264];

/// An encoder of this build to probe: Linux has VA-API, NVENC and x264.
fn candidates() -> Vec<(Backend, Option<AdapterId>)> {
    #[cfg_attr(
        not(feature = "fake-desktop"),
        expect(unused_mut, reason = "only the tests add one")
    )]
    let mut all = vec![
        (Backend::Va, None),
        (Backend::Nvenc, None),
        (Backend::X264, None),
    ];
    #[cfg(feature = "fake-desktop")]
    if fake::mode().is_some() {
        all.insert(0, (Backend::Fake, None));
    }
    all
}

/// What became of probing one encoder in one codec.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probed {
    Works {
        max_w: u32,
        max_h: u32,
    },
    /// Not there or not working, and why.
    No(String),
}

/// The encoders this PC has, as probed, and what was learned of them since.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// The ones that encode at least one codec, in the order of [`candidates`].
    pub encoders: Vec<EncoderCaps>,
    /// Why each encoder and codec that does not work does not, for the log.
    pub missing: Vec<(Backend, Codec, String)>,
    /// Encoders and codecs that did not open this run, with why.
    pub failed: Vec<(Kind, Codec, String)>,
    /// The fingerprint it was probed with.
    key: String,
    /// Where it is kept; none where there is no cache directory.
    path: Option<PathBuf>,
}

impl Catalog {
    /// From the cache when its fingerprint is the current one, else probed (and kept).
    pub fn load_or_probe() -> Self {
        let key = fingerprint();
        let path = crate::platform::cache_home().map(|d| d.join("prostir").join("encoders.txt"));
        if let Some(path) = &path
            && let Some(mut catalog) = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| Self::parse(&text, &key))
        {
            catalog.path = Some(path.clone());
            info!(
                "video encoders as found before ({}): {}",
                path.display(),
                catalog.summary()
            );
            return catalog;
        }
        let started = Instant::now();
        let mut catalog = Self::probe();
        catalog.key = key;
        catalog.path = path;
        info!(
            "video encoders probed in {} ms: {}",
            started.elapsed().as_millis(),
            catalog.summary()
        );
        for (backend, codec, why) in &catalog.missing {
            info!("{} {codec:?}: {why}", backend.label());
        }
        catalog.save();
        catalog
    }

    /// Probes every candidate in every codec, side by side.
    fn probe() -> Self {
        let deadline = Instant::now() + PROBE_TIMEOUT;
        let (tx, rx) = mpsc::channel();
        let mut asked = Vec::new();
        for (backend, adapter) in candidates() {
            for codec in CODECS {
                if backend.elements(codec).is_none() {
                    continue;
                }
                let index = asked.len();
                asked.push((backend, adapter, codec));
                let sender = tx.clone();
                let spawned = std::thread::Builder::new()
                    .name(format!("probe-{}", backend.name()))
                    .spawn(move || {
                        let _ = sender.send((index, probe_one(backend, codec)));
                    });
                if let Err(e) = spawned {
                    let _ = tx.send((index, Probed::No(e.to_string())));
                }
            }
        }
        drop(tx);
        let mut results: Vec<Option<Probed>> = vec![None; asked.len()];
        let mut left = asked.len();
        while left > 0 {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((index, probed)) => {
                    results[index] = Some(probed);
                    left -= 1;
                }
                Err(_) => break,
            }
        }
        let mut catalog = Self::default();
        for ((backend, adapter, codec), probed) in asked.into_iter().zip(results) {
            let probed = probed.unwrap_or_else(|| {
                warn!(
                    "{} {codec:?}: the probe did not end in {PROBE_TIMEOUT:?}",
                    backend.label()
                );
                Probed::No(format!("did not answer in {} s", PROBE_TIMEOUT.as_secs()))
            });
            catalog.add(backend, adapter, codec, probed);
        }
        catalog
    }

    fn add(&mut self, backend: Backend, adapter: Option<AdapterId>, codec: Codec, probed: Probed) {
        match probed {
            Probed::Works { max_w, max_h } => {
                let caps = CodecCaps {
                    codec,
                    max_w,
                    max_h,
                };
                match self
                    .encoders
                    .iter_mut()
                    .find(|e| e.kind() == (backend, adapter))
                {
                    Some(e) => e.codecs.push(caps),
                    None => self.encoders.push(EncoderCaps {
                        backend,
                        adapter,
                        codecs: vec![caps],
                        sessions: None,
                    }),
                }
            }
            Probed::No(why) => self.missing.push((backend, codec, why)),
        }
    }

    /// `va h265 h264`, for the log.
    fn summary(&self) -> String {
        if self.encoders.is_empty() {
            return "none".into();
        }
        self.encoders
            .iter()
            .map(|e| {
                let codecs: Vec<String> =
                    e.codecs.iter().map(|c| format!("{:?}", c.codec)).collect();
                let limit = e
                    .sessions
                    .map_or_else(String::new, |n| format!(" (at most {n} at once)"));
                format!("{} {}{limit}", e.backend.label(), codecs.join(" "))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Each codec this PC encodes, and whether on the graphics card (for the headset's
    /// menu): hardware first, as before.
    pub fn available(&self) -> Vec<(Codec, bool)> {
        let mut choices = Vec::new();
        for codec in [Codec::H265, Codec::H264, Codec::Av1] {
            for hardware in [true, false] {
                if self
                    .encoders
                    .iter()
                    .any(|e| e.backend.hardware() == hardware && e.codec(codec).is_some())
                {
                    choices.push((codec, hardware));
                }
            }
        }
        choices
    }

    /// The encoder and codec this PC encodes `prefer` with when nothing else is known
    /// (where the frames are, how large): the first hardware one in [`Backend::ORDER`]
    /// with the codecs in their order, else x264; only x264 with `software`.
    pub fn pick(&self, prefer: Codec, software: bool) -> Result<(Backend, Codec)> {
        let mut codecs = vec![prefer];
        codecs.extend(
            [Codec::H265, Codec::H264]
                .into_iter()
                .filter(|c| *c != prefer),
        );
        rank(
            &self.encoders,
            Memory::System,
            &codecs,
            software,
            &|_| 0,
            &self.failed,
        )
        .choices
        .first()
        .map(|c| (c.backend, c.codec))
        .ok_or_else(no_encoder)
    }

    /// The encoders for an output whose frames are in `memory` (see [`rank`]).
    pub fn rank(&self, memory: Memory, wanted: Wanted, running: &dyn Fn(Kind) -> u32) -> Ranked {
        let codecs = wanted.codecs();
        rank(
            &self.encoders,
            memory,
            &codecs,
            wanted.software,
            running,
            &self.failed,
        )
    }

    /// Encoder `kind` did not open one more session while `open` of it ran: it opens
    /// at most `open` (the least seen, if it was lower before), kept in the cache.
    pub fn learn_limit(&mut self, kind: Kind, open: u32) {
        let Some(e) = self.encoders.iter_mut().find(|e| e.kind() == kind) else {
            return;
        };
        let max = e.sessions.map_or(open, |n| n.min(open)).max(1);
        if e.sessions != Some(max) {
            warn!("{} opens at most {max} sessions at once", kind.0.label());
            e.sessions = Some(max);
            self.save();
        }
    }

    /// Encoder `kind` did not open in `codec` with no session of it running: not
    /// tried again this run, and probed again at the next start.
    pub fn failed(&mut self, kind: Kind, codec: Codec, why: String) {
        warn!(
            "{} {codec:?} did not start: {why}; not used again this run",
            kind.0.label()
        );
        if !self
            .failed
            .iter()
            .any(|(k, c, _)| *k == kind && *c == codec)
        {
            self.failed.push((kind, codec, why));
        }
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The cache file's text.
    fn text(&self) -> String {
        let mut out = format!(
            "# The video encoders Prostir found on this PC, and how many sessions each opens at\n\
             # once. Probed again when GStreamer or a graphics driver changes; safe to delete.\n\
             key {}\n",
            self.key
        );
        for e in &self.encoders {
            let codecs: Vec<String> = e
                .codecs
                .iter()
                .map(|c| format!("{} {}x{}", codec_name(c.codec), c.max_w, c.max_h))
                .collect();
            out.push_str(&format!(
                "encoder {} {} {}\n",
                e.backend.name(),
                adapter_text(e.adapter),
                codecs.join(" ")
            ));
            if let Some(n) = e.sessions {
                out.push_str(&format!(
                    "sessions {} {} {n}\n",
                    e.backend.name(),
                    adapter_text(e.adapter)
                ));
            }
        }
        for (backend, codec, why) in &self.missing {
            let why = why.replace(['\n', '\r'], " ");
            out.push_str(&format!(
                "missing {} {} {why}\n",
                backend.name(),
                codec_name(*codec)
            ));
        }
        out
    }

    /// The cache file's catalog, when it was made with fingerprint `key`.
    fn parse(text: &str, key: &str) -> Option<Self> {
        let mut catalog = Self {
            key: key.to_owned(),
            ..Self::default()
        };
        let mut keyed = false;
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            match words.next()? {
                "key" => {
                    keyed = words.next()? == key;
                    if !keyed {
                        return None;
                    }
                }
                "encoder" => {
                    let backend = Backend::named(words.next()?)?;
                    let adapter = parse_adapter(words.next()?)?;
                    let mut codecs = Vec::new();
                    while let Some(codec) = words.next() {
                        let (w, h) = words.next()?.split_once('x')?;
                        codecs.push(CodecCaps {
                            codec: parse_codec(codec)?,
                            max_w: w.parse().ok()?,
                            max_h: h.parse().ok()?,
                        });
                    }
                    catalog.encoders.push(EncoderCaps {
                        backend,
                        adapter,
                        codecs,
                        sessions: None,
                    });
                }
                "sessions" => {
                    let kind = (
                        Backend::named(words.next()?)?,
                        parse_adapter(words.next()?)?,
                    );
                    let n = words.next()?.parse().ok()?;
                    catalog
                        .encoders
                        .iter_mut()
                        .find(|e| e.kind() == kind)?
                        .sessions = Some(n);
                }
                "missing" => {
                    let backend = Backend::named(words.next()?)?;
                    let codec = parse_codec(words.next()?)?;
                    catalog
                        .missing
                        .push((backend, codec, words.collect::<Vec<_>>().join(" ")));
                }
                _ => return None,
            }
        }
        keyed.then_some(catalog)
    }

    /// Writes the cache file (whole, through a file next to it, so a crash or another
    /// start writing at the same time never leaves half of one).
    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
                std::fs::write(&tmp, self.text())?;
                std::fs::rename(&tmp, path)
            });
        if let Err(e) = written {
            warn!(
                "cannot keep what the encoders can do in {}: {e}",
                path.display()
            );
        }
    }
}

fn no_encoder() -> anyhow::Error {
    anyhow!(
        "{}",
        crate::i18n::t(
            "На ПК немає відеокодувальника. Intel чи AMD: встановіть VA-плагін GStreamer \
             (gst-plugin-va або gst-plugins-bad) і VA-драйвер. NVIDIA: драйвер NVIDIA і \
             nvcodec з gst-plugins-bad. Будь-яка відеокарта: gst-plugins-ugly (x264, \
             кодування процесором).",
            "No video encoder on the PC. Intel or AMD: install the GStreamer VA plugin \
             (gst-plugin-va or gst-plugins-bad) and a VA driver. NVIDIA: the NVIDIA driver \
             and nvcodec from gst-plugins-bad. Any graphics: gst-plugins-ugly (x264, \
             encoding on the CPU).",
        )
    )
}

fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::H265 => "h265",
        Codec::Av1 => "av1",
    }
}

fn parse_codec(name: &str) -> Option<Codec> {
    [Codec::H264, Codec::H265, Codec::Av1]
        .into_iter()
        .find(|c| codec_name(*c) == name)
}

fn adapter_text(adapter: Option<AdapterId>) -> String {
    adapter.map_or_else(|| "-".into(), |a| format!("{:x}", a.0))
}

fn parse_adapter(text: &str) -> Option<Option<AdapterId>> {
    if text == "-" {
        return Some(None);
    }
    u64::from_str_radix(text, 16)
        .ok()
        .map(|a| Some(AdapterId(a)))
}

/// What decides what the encoders do: GStreamer's version, the plugin files of every
/// candidate, and the graphics drivers as the platform describes them. A short hash of
/// it, so a change anywhere makes the cache stale.
fn fingerprint() -> String {
    let mut parts = vec![gst::version_string().to_string()];
    let mut plugins: Vec<String> = candidates()
        .into_iter()
        .flat_map(|(backend, _)| CODECS.iter().filter_map(move |c| backend.elements(*c)))
        .flatten()
        .filter_map(|e| gst::ElementFactory::find(e)?.plugin()?.filename())
        .map(|f| file_identity(&f))
        .collect();
    plugins.sort();
    plugins.dedup();
    parts.extend(plugins);
    parts.extend(crate::platform::encoder_drivers());
    format!("{:016x}", fnv1a(parts.join("\n").as_bytes()))
}

/// A file's path, size and modification time: what changes when it is replaced.
pub fn file_identity(path: &Path) -> String {
    let meta = std::fs::metadata(path).ok();
    let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    format!("{} {size} {modified}", path.display())
}

/// 64-bit FNV-1a: stable across Rust versions, unlike the standard hasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The largest picture the element `name` takes on its sink, from its pad template.
fn max_size(name: &str) -> Option<(u32, u32)> {
    let factory = gst::ElementFactory::find(name)?;
    let template = factory
        .static_pad_templates()
        .into_iter()
        .find(|t| t.direction() == gst::PadDirection::Sink)?;
    let caps = template.caps();
    let s = caps.structure(0)?;
    let max = |field: &str| -> Option<u32> {
        match s.value(field).ok()? {
            v if v.get::<gst::IntRange<i32>>().is_ok() => {
                u32::try_from(v.get::<gst::IntRange<i32>>().ok()?.max()).ok()
            }
            v => u32::try_from(v.get::<i32>().ok()?).ok(),
        }
    };
    Some((max("width")?, max("height")?))
}

/// Encodes a few frames of a test picture with `backend` in `codec`, through the same
/// elements and settings as a stream.
fn probe_one(backend: Backend, codec: Codec) -> Probed {
    #[cfg(feature = "fake-desktop")]
    if backend == Backend::Fake
        && let Some(made) = fake::probe()
    {
        return made;
    }
    let Some(elements) = backend.elements(codec) else {
        return Probed::No("no such codec".into());
    };
    if let Some(gone) = elements
        .iter()
        .chain([&super::parser(codec)])
        .find(|e| gst::ElementFactory::find(e).is_none())
    {
        return Probed::No(format!("{gone} is not installed"));
    }
    match run_probe(backend, codec) {
        Ok(()) => {
            let encoder = elements.last().copied().unwrap_or_default();
            let (max_w, max_h) = max_size(encoder).unwrap_or((u32::MAX, u32::MAX));
            Probed::Works { max_w, max_h }
        }
        Err(e) => Probed::No(format!("{e:#}")),
    }
}

fn run_probe(backend: Backend, codec: Codec) -> Result<()> {
    let (w, h) = PROBE_SIZE;
    let input = Input {
        source: format!(
            "videotestsrc num-buffers=3 ! video/x-raw,width={w},height={h},framerate=60/1"
        ),
        label: format!("probe {}", backend.name()),
    };
    let cfg = EncoderConfig {
        codec,
        bitrate_kbps: 2000,
        backend,
        sharp: false,
    };
    // A fakesink: an appsink would hold the end of the stream back until its frames are
    // taken.
    let description = format!(
        "{} ! fakesink name=sink sync=false",
        encoding(&input, &cfg)?
    );
    let pipeline = gst::parse::launch(&description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let frames = Arc::new(AtomicU32::new(0));
    if let Some(pad) = pipeline.by_name("sink").and_then(|s| s.static_pad("sink")) {
        let frames = frames.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            frames.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
    let result = pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| anyhow!("does not start"))
        .and_then(|_| {
            let ended = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(PROBE_TIMEOUT.as_millis() as u64),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
            match ended.as_ref().map(|m| m.view()) {
                Some(gst::MessageView::Eos(_)) => Ok(()),
                Some(gst::MessageView::Error(e)) => bail!("{}", e.error()),
                _ => bail!("no end of the test frames"),
            }
        })
        .and_then(|()| {
            anyhow::ensure!(frames.load(Ordering::Relaxed) > 0, "no frame came out");
            Ok(())
        });
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// The catalog of this run, probed (or read from the cache) the first time it is asked
/// for: at the first session, not at the start of the program, so an idle PC never
/// runs a probe.
pub fn catalog() -> MutexGuard<'static, Catalog> {
    static CATALOG: OnceLock<Mutex<Catalog>> = OnceLock::new();
    CATALOG
        .get_or_init(|| Mutex::new(Catalog::load_or_probe()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The fake hardware encoder of the end-to-end tests, as `PROSTIR_FAKE_ENCODER` says:
/// `works` (any number of sessions), `sessions=N` (one more does not open), `broken`
/// (its probe fails), `hangs` (its probe never ends).
#[cfg(feature = "fake-desktop")]
pub mod fake {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::Probed;

    /// Fake hardware encoders open now.
    pub static OPEN: AtomicU32 = AtomicU32::new(0);

    pub fn mode() -> Option<String> {
        std::env::var("PROSTIR_FAKE_ENCODER")
            .ok()
            .filter(|m| !m.is_empty())
    }

    /// The sessions it opens at once; none: any number.
    pub fn sessions() -> Option<u32> {
        mode()?.strip_prefix("sessions=")?.parse().ok()
    }

    /// Its probe's result, where the mode makes one up; none: probed as x264.
    pub(super) fn probe() -> Option<Probed> {
        match mode()?.as_str() {
            "broken" => Some(Probed::No("broken on purpose".into())),
            "hangs" => {
                std::thread::sleep(std::time::Duration::from_secs(600));
                Some(Probed::No("hung".into()))
            }
            _ => None,
        }
    }

    /// Whether one more would open now.
    pub fn opens() -> bool {
        sessions().is_none_or(|max| OPEN.load(Ordering::Relaxed) < max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(backend: Backend, codecs: &[(Codec, u32)]) -> EncoderCaps {
        EncoderCaps {
            backend,
            adapter: None,
            codecs: codecs
                .iter()
                .map(|&(codec, max)| CodecCaps {
                    codec,
                    max_w: max,
                    max_h: max,
                })
                .collect(),
            sessions: None,
        }
    }

    fn sample() -> Catalog {
        let mut va = caps(Backend::Va, &[(Codec::H265, 8192), (Codec::H264, 4096)]);
        va.sessions = Some(3);
        Catalog {
            encoders: vec![va, caps(Backend::X264, &[(Codec::H264, 16384)])],
            missing: vec![(
                Backend::Nvenc,
                Codec::H264,
                "nvh264enc is not installed".into(),
            )],
            failed: Vec::new(),
            key: "0123456789abcdef".into(),
            path: None,
        }
    }

    /// What the cache file says comes back the same, under the same fingerprint only.
    #[test]
    fn the_cache_reads_back_what_it_wrote() {
        let catalog = sample();
        let text = catalog.text();
        let back = Catalog::parse(&text, "0123456789abcdef").unwrap();
        assert_eq!(back.encoders, catalog.encoders);
        assert_eq!(back.missing, catalog.missing);
        assert_eq!(back.text(), text);
        // Another GStreamer or driver: probed again.
        assert!(Catalog::parse(&text, "fedcba9876543210").is_none());
        // A file of another version, or half of one, is no catalog.
        assert!(Catalog::parse("encoder va - h264 4096x4096\n", "0123456789abcdef").is_none());
        assert!(Catalog::parse(&format!("{text}what is this\n"), "0123456789abcdef").is_none());
        assert!(
            Catalog::parse(
                "key 0123456789abcdef\nencoder va - h264 4096\n",
                "0123456789abcdef"
            )
            .is_none()
        );
    }

    /// A file in the cache directory: written whole, read back, and gone once an
    /// encoder did not start, so the next start probes again.
    #[test]
    fn the_cache_file_is_kept_and_dropped() {
        let dir = std::env::temp_dir().join(format!("prostir-encoders-{}", std::process::id()));
        let path = dir.join("prostir").join("encoders.txt");
        let mut catalog = Catalog {
            path: Some(path.clone()),
            ..sample()
        };
        catalog.save();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("sessions va - 3"), "{text}");
        // Fewer sessions seen: the lower one is kept, and written.
        catalog.learn_limit((Backend::Va, None), 2);
        let back = Catalog::parse(&std::fs::read_to_string(&path).unwrap(), &catalog.key).unwrap();
        assert_eq!(back.encoders[0].sessions, Some(2));
        // More is never learned from a failure: the least seen stays.
        catalog.learn_limit((Backend::Va, None), 5);
        assert_eq!(catalog.encoders[0].sessions, Some(2));
        catalog.failed((Backend::Va, None), Codec::H265, "no".into());
        assert!(!path.exists(), "probed again at the next start");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The headset's menu: each codec on the graphics card and on the CPU, as found.
    #[test]
    fn codecs_offered_are_those_found() {
        assert_eq!(
            sample().available(),
            vec![
                (Codec::H265, true),
                (Codec::H264, true),
                (Codec::H264, false)
            ]
        );
        assert!(Catalog::default().available().is_empty());
    }

    /// Without regard to where frames are: VA-API in the codec asked for, else in the
    /// next; x264 with `--software` and when there is nothing else.
    #[test]
    fn the_pick_for_the_pc_as_before() {
        let catalog = sample();
        assert_eq!(
            catalog.pick(Codec::H265, false).unwrap(),
            (Backend::Va, Codec::H265)
        );
        assert_eq!(
            catalog.pick(Codec::H264, false).unwrap(),
            (Backend::Va, Codec::H264)
        );
        assert_eq!(
            catalog.pick(Codec::Av1, false).unwrap(),
            (Backend::Va, Codec::H265)
        );
        assert_eq!(
            catalog.pick(Codec::H265, true).unwrap(),
            (Backend::X264, Codec::H264)
        );
        let cpu = Catalog {
            encoders: vec![caps(Backend::X264, &[(Codec::H264, 16384)])],
            ..Catalog::default()
        };
        assert_eq!(
            cpu.pick(Codec::H265, false).unwrap(),
            (Backend::X264, Codec::H264)
        );
        assert!(Catalog::default().pick(Codec::H264, false).is_err());
    }

    /// The fingerprint is a short, stable hash.
    #[test]
    fn the_fingerprint_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fnv1a(b"1.28.9"), fnv1a(b"1.28.8"));
    }
}
