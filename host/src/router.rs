//! Delivery of what the encoders and the sound capture make to every headset with a
//! session: each session's connection (its outbox) gets the streams on its own terms.
//!
//! A stream is encoded once and fans out. What is shared belongs to the stream (its
//! size and codec, when a keyframe was last asked for); what each headset has of it
//! belongs to the session (whether the stream was announced to it and in which
//! generation, the frame numbers, whether it waits for a keyframe, how long frames took
//! to show). So one headset that drops frames, is taken off, or cannot decode a stream
//! changes nothing for the others: the encoder is asked for a keyframe that all get,
//! and a stream too big for one headset still goes to the rest.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use prostir_proto::state::{self, Gen};
use prostir_proto::stats::{Histogram, Period};
use prostir_proto::{Codec, HostMsg};

use crate::encoder::EncoderEvent;
use crate::events::SessionId;
use crate::outbox::Outbox;
use crate::screen::ScreenId;

/// How often the router logs what each stream sent (see [`Router::report`]).
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// A keyframe is asked of an encoder at most this often, whichever headset waits.
const KEYFRAME_EVERY: Duration = Duration::from_millis(300);

/// The generation of the next stream start: one counter for every stream of this run
/// of the app, so a frame or a request of an older start is never taken for a new one,
/// across sessions too.
static NEXT_GEN: AtomicU32 = AtomicU32::new(1);

fn next_gen() -> Gen {
    Gen(NEXT_GEN.fetch_add(1, Ordering::Relaxed))
}

/// A stream as its encoder makes it, the same for every headset.
struct StreamSlot {
    /// The screen's name, for the log.
    name: String,
    /// A display of the PC: a broken stream is started again, not the end of all.
    physical: bool,
    size: Option<(u32, u32)>,
    /// What the encoder makes, announced with the stream.
    codec: Codec,
    /// When a keyframe was last asked for: asking again on every frame while waiting
    /// would make the encoder emit one huge keyframe after another.
    keyframe_asked: Option<Instant>,
    /// When the encoder started, for the log of how long its first picture took.
    since: Instant,
}

impl StreamSlot {
    /// Whether to ask the encoder for a keyframe now (at most every 300 ms).
    fn ask_keyframe(&mut self) -> bool {
        let due = self
            .keyframe_asked
            .is_none_or(|t| t.elapsed() >= KEYFRAME_EVERY);
        if due {
            self.keyframe_asked = Some(Instant::now());
        }
        due
    }
}

/// What one headset has of one stream.
struct Delivery {
    /// The current size and codec were announced to this headset.
    announced: bool,
    /// The headset has this stream (announced on this connection, perhaps by an
    /// encoder since replaced), so it is told when the stream ends.
    known: bool,
    /// The generation announced last, and the number of its next frame.
    generation: Gen,
    seq: u32,
    need_keyframe: bool,
    /// Frame number → when the encoded frame was queued for sending.
    sent: BTreeMap<u32, Instant>,
    /// Encoded on the PC → handed to the headset's compositor, as the headset reports
    /// back with `FrameShown` (for some frames only).
    shown: Histogram,
    /// Frames not queued because the stream waited for a keyframe.
    skipped: u32,
    /// Frames came that this headset did not get (its display not in its list yet, or
    /// the headset taken off), so it needs a fresh picture.
    held: bool,
    /// The picture is larger than this headset decodes in its codec: nothing of it
    /// goes to it.
    too_big: bool,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            announced: false,
            known: false,
            generation: Gen(0),
            seq: 0,
            need_keyframe: true,
            sent: BTreeMap::new(),
            shown: Histogram::default(),
            skipped: 0,
            held: false,
            too_big: false,
        }
    }
}

/// One session's headset, as the router sends to it.
struct Receiver {
    tx: Arc<Outbox>,
    /// What it decodes: no stream larger goes to it.
    headset: state::HeadsetCaps,
    /// On the head: frames and sound go to it only then.
    worn: bool,
    /// The displays this headset was told about in its session (`Displays`): a stream
    /// starts only once its display is among them, since the start goes in a lane of
    /// its own and must not overtake the list.
    published: BTreeSet<ScreenId>,
    /// What it has of each stream.
    streams: BTreeMap<ScreenId, Delivery>,
}

impl Receiver {
    /// Whether the picture of `slot` is larger than this headset decodes.
    fn too_big(&self, slot: &StreamSlot) -> bool {
        slot.size
            .is_some_and(|(w, h)| !self.headset.fits(slot.codec, w, h))
    }
}

struct RouterInner {
    streams: BTreeMap<ScreenId, StreamSlot>,
    /// Every session's headset, by the session's number.
    receivers: BTreeMap<SessionId, Receiver>,
    report: Period,
}

impl RouterInner {
    /// The streams larger than every headset decodes (none without a headset): their
    /// display stands failed, since nobody could show it.
    fn too_big_for_all(&self) -> Vec<TooBig> {
        if self.receivers.is_empty() {
            return Vec::new();
        }
        self.streams
            .iter()
            .filter(|(_, s)| self.receivers.values().all(|r| r.too_big(s)))
            .filter_map(|(id, s)| Some((*id, s.size?, s.codec)))
            .collect()
    }
}

/// A stream larger than every headset decodes: its display, size and codec.
pub type TooBig = (ScreenId, (u32, u32), Codec);

/// A frame to queue for one headset.
struct Out {
    session: SessionId,
    /// The stream's first frame for this headset: its start goes before it.
    start: Option<HostMsg>,
    /// The frame goes too (not when the headset waits for a keyframe).
    frame: bool,
}

/// Delivers encoder output and sound to every headset with a session.
pub struct Router {
    /// A screenshot waiting for the next keyframe of stream `id` (see `screenshot`).
    pub snapshot: Mutex<Option<(ScreenId, std::sync::mpsc::SyncSender<Vec<u8>>)>>,
    inner: Mutex<RouterInner>,
    /// A stream that is no display of the PC broke: the monitors are made anew.
    pub failed: AtomicBool,
    /// Displays of the PC whose stream broke, to start again.
    broken: Mutex<Vec<ScreenId>>,
    /// Streams larger than every headset decodes, with their size and codec, for
    /// `Displays` to mark them failed.
    too_big: Mutex<Vec<TooBig>>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RouterInner {
                streams: BTreeMap::new(),
                receivers: BTreeMap::new(),
                report: Period::new(REPORT_EVERY),
            }),
            failed: AtomicBool::new(false),
            broken: Mutex::new(Vec::new()),
            too_big: Mutex::new(Vec::new()),
            snapshot: Mutex::new(None),
        }
    }

    /// The headset of session `session`, which decodes what `headset` says and is on
    /// the head or not (`worn`), gets the streams through `tx`, each only as large as
    /// it decodes. One larger never starts there (no `StreamStart`): its `Displays`
    /// says it failed instead (see `Displays::displays_part`), and one larger than
    /// every headset decodes [`crate::displays::Displays`] marks failed for all, so
    /// one display too big is not the end of anyone's session.
    pub fn attach(
        &self,
        session: SessionId,
        tx: Arc<Outbox>,
        headset: state::HeadsetCaps,
        worn: bool,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if inner.receivers.is_empty() {
            inner.report = Period::new(REPORT_EVERY);
        }
        let mut receiver = Receiver {
            tx,
            headset,
            worn,
            published: BTreeSet::new(),
            streams: BTreeMap::new(),
        };
        for (id, s) in &inner.streams {
            let too_big = receiver.too_big(s);
            if let (true, Some((w, h))) = (too_big, s.size) {
                info!(
                    "{}: {w}x{h} in {:?} is larger than one headset decodes: not sent to it",
                    s.name, s.codec
                );
            }
            let delivery = Delivery {
                too_big,
                ..Delivery::default()
            };
            receiver.streams.insert(*id, delivery);
        }
        inner.receivers.insert(session, receiver);
        // What was found for the headsets before is looked at again with this one.
        *self.too_big.lock().unwrap() = inner.too_big_for_all();
    }

    /// The session ended: nothing more goes to its headset. A stream only it decoded
    /// is now larger than every headset left decodes, and found so.
    pub fn detach(&self, session: SessionId) {
        let mut inner = self.inner.lock().unwrap();
        inner.receivers.remove(&session);
        *self.too_big.lock().unwrap() = inner.too_big_for_all();
    }

    /// The headset of `session` is on the head (`worn`) or not: frames and sound go to
    /// it only while it is. Put on again, it returns the streams that sent meanwhile,
    /// which it needs a fresh picture of.
    pub fn set_worn(&self, session: SessionId, worn: bool) -> Vec<ScreenId> {
        let mut inner = self.inner.lock().unwrap();
        let Some(r) = inner.receivers.get_mut(&session) else {
            return Vec::new();
        };
        if r.worn == worn {
            return Vec::new();
        }
        r.worn = worn;
        if !worn {
            return Vec::new();
        }
        r.streams
            .iter_mut()
            .filter(|(_, d)| !d.too_big)
            .filter_map(|(id, d)| {
                d.need_keyframe = true;
                std::mem::take(&mut d.held).then_some(*id)
            })
            .collect()
    }

    /// Whether stream `id` is still larger than every headset decodes (a stream found
    /// too big may have started over since, in another size or codec, or another
    /// headset that decodes it came).
    pub fn still_too_big(&self, id: ScreenId) -> bool {
        let inner = self.inner.lock().unwrap();
        let Some(slot) = inner.streams.get(&id) else {
            return false;
        };
        !inner.receivers.is_empty() && inner.receivers.values().all(|r| r.too_big(slot))
    }

    /// Whether a headset with a session decodes a picture of `size` in `codec`
    /// (without one, a display too big for the last ones waits for the next).
    pub fn fits(&self, codec: Codec, (w, h): (u32, u32)) -> bool {
        self.inner
            .lock()
            .unwrap()
            .receivers
            .values()
            .any(|r| r.headset.fits(codec, w, h))
    }

    /// What the headset of every session decodes.
    pub fn headsets(&self) -> Vec<state::HeadsetCaps> {
        let inner = self.inner.lock().unwrap();
        inner
            .receivers
            .values()
            .map(|r| r.headset.clone())
            .collect()
    }

    /// Streams found larger than every headset decodes since the last call.
    pub fn take_too_big(&self) -> Vec<TooBig> {
        std::mem::take(&mut *self.too_big.lock().unwrap())
    }

    /// The headset of `session` was told about these displays (`Displays` is queued):
    /// their streams may start for it. Returns the streams that may start only now
    /// and sent frames before: they need a fresh picture.
    pub fn publish(&self, session: SessionId, ids: &BTreeSet<ScreenId>) -> Vec<ScreenId> {
        let mut inner = self.inner.lock().unwrap();
        let Some(r) = inner.receivers.get_mut(&session) else {
            return Vec::new();
        };
        let new: Vec<ScreenId> = ids.difference(&r.published).copied().collect();
        r.published = ids.clone();
        new.into_iter()
            .filter(|id| {
                r.streams
                    .get_mut(id)
                    .is_some_and(|d| std::mem::take(&mut d.held))
            })
            .collect()
    }

    /// Logs, every [`REPORT_EVERY`] while a headset is connected, one line per stream
    /// and headset: what the outbox sent and dropped, how long frames waited there,
    /// keyframes, and encoded → shown in the headset. Then one line for sound and one
    /// for the round trip of the pings, for each headset. With more than one headset,
    /// each line names its session.
    pub fn report(&self) {
        struct Lines {
            session: SessionId,
            stats: crate::outbox::OutboxStats,
            streams: Vec<(ScreenId, String, Histogram, u32)>,
        }
        let (window, lines) = {
            let mut inner = self.inner.lock().unwrap();
            let RouterInner {
                streams,
                receivers,
                report,
            } = &mut *inner;
            if receivers.is_empty() {
                return;
            }
            let Some(window) = report.due(Instant::now()) else {
                return;
            };
            let lines: Vec<Lines> = receivers
                .iter_mut()
                .map(|(session, r)| Lines {
                    session: *session,
                    stats: r.tx.take_stats(),
                    streams: r
                        .streams
                        .iter_mut()
                        .map(|(id, d)| {
                            let name = streams.get(id).map_or_else(String::new, |s| s.name.clone());
                            let shown = std::mem::take(&mut d.shown);
                            let skipped = std::mem::take(&mut d.skipped);
                            (*id, name, shown, skipped)
                        })
                        .collect(),
                })
                .collect();
            (window, lines)
        };
        // Logged without the lock, which every encoded frame takes.
        let several = lines.len() > 1;
        for Lines {
            session,
            mut stats,
            streams,
        } in lines
        {
            let whose = if several {
                format!(" (session {session})")
            } else {
                String::new()
            };
            for (id, name, shown, skipped) in streams {
                let video = stats.video.remove(&id.wire()).unwrap_or_default();
                if video.frames == 0 && video.dropped.total() == 0 && skipped == 0 {
                    continue;
                }
                let shown = match shown.count() {
                    0 => "no reports".to_owned(),
                    n => format!("{} ({n} reports)", shown.summary_ms()),
                };
                info!(
                    "{name}: {}; {skipped} skipped waiting for a keyframe; encoded → shown in headset {shown}{whose}",
                    video.summary(window),
                );
            }
            if stats.audio_wait.count() > 0 || stats.audio_dropped > 0 {
                info!(
                    "sound: sent {} slices, waited in outbox {}; dropped {}{whose}",
                    stats.audio_wait.count(),
                    stats.audio_wait.summary_ms(),
                    stats.audio_dropped
                );
            }
            info!(
                "link: round trip {} ({} pings answered){whose}",
                stats.round_trip.summary_ms(),
                stats.round_trip.count()
            );
        }
    }

    /// Sends a control message to every headset with a session. Never blocks.
    pub fn send_all(&self, msg: HostMsg) {
        let mut inner = self.inner.lock().unwrap();
        let mut over = Vec::new();
        for (session, r) in &inner.receivers {
            if !r.tx.control(msg.clone()) {
                over.push(*session);
            }
        }
        for session in over {
            inner.receivers.remove(&session);
        }
    }

    /// The size and codec stream `id` goes out in, once its encoder has said its size.
    pub fn stream_format(&self, id: ScreenId) -> Option<((u32, u32), Codec)> {
        let inner = self.inner.lock().unwrap();
        let slot = inner.streams.get(&id)?;
        Some((slot.size?, slot.codec))
    }

    /// The size stream `id` goes out at, once its encoder has said.
    pub fn stream_size(&self, id: ScreenId) -> Option<(u32, u32)> {
        self.inner.lock().unwrap().streams.get(&id)?.size
    }

    /// Whether `generation` is what stream `id` sends now to the headset of `session`
    /// (a request about an older one is for frames long gone).
    pub fn is_current(&self, session: SessionId, id: ScreenId, generation: Gen) -> bool {
        self.inner
            .lock()
            .unwrap()
            .receivers
            .get(&session)
            .and_then(|r| r.streams.get(&id))
            .is_some_and(|d| d.announced && d.generation == generation)
    }

    /// Stream `id` (named `name`, a display of the PC with `physical`) starts, or
    /// starts over with a new encoder making `codec`: every headset gets it announced
    /// anew, and one that had it is told when it ends.
    pub fn add_stream(&self, id: ScreenId, codec: Codec, name: String, physical: bool) {
        let mut inner = self.inner.lock().unwrap();
        let slot = StreamSlot {
            name,
            physical,
            size: None,
            codec,
            keyframe_asked: None,
            since: Instant::now(),
        };
        inner.streams.insert(id, slot);
        for r in inner.receivers.values_mut() {
            let (known, generation) = r
                .streams
                .get(&id)
                .map_or((false, Gen(0)), |d| (d.known, d.generation));
            let delivery = Delivery {
                known,
                generation,
                ..Delivery::default()
            };
            r.streams.insert(id, delivery);
        }
    }

    /// Displays of the PC whose stream broke since the last call.
    pub fn take_broken(&self) -> Vec<ScreenId> {
        std::mem::take(&mut *self.broken.lock().unwrap())
    }

    /// Stream `id` stops: every headset that had it hears so.
    pub fn remove_stream(&self, id: ScreenId) {
        let mut inner = self.inner.lock().unwrap();
        if inner.streams.remove(&id).is_none() {
            return;
        }
        let mut over = Vec::new();
        for (session, r) in &mut inner.receivers {
            let Some(d) = r.streams.remove(&id) else {
                continue;
            };
            let end = HostMsg::StreamEnd {
                id: id.wire(),
                generation: d.generation,
            };
            if d.known && !r.tx.control(end) {
                over.push(*session);
            }
        }
        for session in over {
            inner.receivers.remove(&session);
        }
    }

    /// Forwards a slice of PC sound to every headset on the head; dropped for one
    /// whose network is backed up (a tiny gap in sound beats growing latency).
    pub fn send_audio(&self, pcm: Vec<u8>) {
        let inner = self.inner.lock().unwrap();
        let mut worn = inner.receivers.values().filter(|r| r.worn).peekable();
        let mut pcm = Some(pcm);
        while let Some(r) = worn.next() {
            let pcm = if worn.peek().is_some() {
                pcm.clone()
            } else {
                pcm.take()
            };
            if let Some(pcm) = pcm {
                r.tx.audio(HostMsg::Audio { pcm });
            }
        }
    }

    /// The headset of `session` showed frame `seq` of `generation` of stream `id`.
    pub fn frame_shown(&self, session: SessionId, id: ScreenId, generation: Gen, seq: u32) {
        let mut inner = self.inner.lock().unwrap();
        let Some(d) = inner
            .receivers
            .get_mut(&session)
            .and_then(|r| r.streams.get_mut(&id))
        else {
            return;
        };
        if d.generation != generation {
            return;
        }
        match d.sent.remove(&seq) {
            Some(sent) => d.shown.add(sent.elapsed()),
            None => debug!("shown frame {seq} of {id} is unknown"),
        }
        // The headset reports only some frames: older ones are done too.
        d.sent = d.sent.split_off(&seq);
    }

    /// What the encoder of stream `id` says. Returns `true` when it should produce a
    /// keyframe.
    pub fn on_event(&self, id: ScreenId, event: EncoderEvent) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let RouterInner {
            streams, receivers, ..
        } = &mut *inner;
        // Streams whose queued frames were too late to send: they need a keyframe.
        for r in receivers.values_mut() {
            for lost in r.tx.take_dropped() {
                if let Some(d) = r.streams.get_mut(&ScreenId::from_wire(lost)) {
                    d.need_keyframe = true;
                }
            }
        }
        let Some(slot) = streams.get_mut(&id) else {
            return false;
        };
        match event {
            EncoderEvent::Size { width, height } => {
                info!("{}: streaming {width}x{height}", slot.name);
                slot.size = Some((width, height));
                let mut fit_one = false;
                for r in receivers.values_mut() {
                    let too_big = r.too_big(slot);
                    let d = r.streams.entry(id).or_default();
                    d.announced = false;
                    d.need_keyframe = true;
                    d.too_big = too_big;
                    fit_one |= !too_big;
                }
                let too_big = !receivers.is_empty() && !fit_one;
                if too_big {
                    warn!(
                        "{}: {width}x{height} in {:?} is larger than the headset decodes: not sent",
                        slot.name, slot.codec
                    );
                    self.too_big
                        .lock()
                        .unwrap()
                        .push((id, (width, height), slot.codec));
                } else if receivers.values().any(|r| r.too_big(slot)) {
                    info!(
                        "{}: {width}x{height} in {:?} is larger than one headset decodes: not sent to it",
                        slot.name, slot.codec
                    );
                }
                !too_big
            }
            EncoderEvent::Failed(reason) => {
                error!("{}: {reason}", slot.name);
                // A display of the PC is started again by `Displays::tick`; anything
                // else ends the sessions.
                if slot.physical {
                    self.broken.lock().unwrap().push(id);
                } else {
                    self.failed.store(true, Ordering::Relaxed);
                }
                false
            }
            EncoderEvent::Frame {
                pts_us,
                keyframe,
                data,
            } => {
                if keyframe {
                    let mut wanted = self.snapshot.lock().unwrap();
                    if wanted.as_ref().is_some_and(|(want, _)| *want == id)
                        && let Some((_, tx)) = wanted.take()
                    {
                        let _ = tx.try_send(data.clone());
                    }
                }
                Self::fan_out(slot, receivers, id, pts_us, keyframe, data)
            }
        }
    }

    /// A frame of stream `id` to every headset that takes it now; whether to ask the
    /// encoder for a keyframe.
    fn fan_out(
        slot: &mut StreamSlot,
        receivers: &mut BTreeMap<SessionId, Receiver>,
        id: ScreenId,
        pts_us: u64,
        keyframe: bool,
        data: Vec<u8>,
    ) -> bool {
        // Asked at once (a stream without its size yet), or at most every 300 ms.
        let mut key_now = false;
        let mut want_key = false;
        let mut outs: Vec<Out> = Vec::new();
        for (session, r) in receivers.iter_mut() {
            let d = r.streams.entry(id).or_default();
            // Larger than this headset decodes: nothing of it goes there.
            if d.too_big {
                continue;
            }
            // Taken off, or its display not in the list it has yet: nothing of it
            // goes, and it gets a fresh picture once it is worn and listed.
            if !r.worn || !r.published.contains(&id) {
                d.need_keyframe = true;
                d.held = true;
                d.skipped += u32::from(r.worn);
                continue;
            }
            let start = if d.announced {
                None
            } else {
                let Some((w, h)) = slot.size else {
                    key_now = true;
                    continue;
                };
                d.generation = next_gen();
                d.seq = 0;
                d.sent.clear();
                Some(HostMsg::StreamStart {
                    id: id.wire(),
                    generation: d.generation,
                    w,
                    h,
                    codec: slot.codec,
                })
            };
            // Waiting for a keyframe: this frame cannot start its pictures.
            let frame = keyframe || !d.need_keyframe;
            if !frame {
                d.skipped += 1;
                want_key = true;
            }
            if start.is_some() || frame {
                outs.push(Out {
                    session: *session,
                    start,
                    frame,
                });
            }
        }
        let mut data = Some(data);
        let mut frames = outs.iter().filter(|o| o.frame).count();
        let mut over = Vec::new();
        for out in outs {
            let Some(r) = receivers.get_mut(&out.session) else {
                continue;
            };
            let d = r.streams.entry(id).or_default();
            if let Some(start) = out.start {
                if !r.tx.control(start) {
                    over.push(out.session);
                    continue;
                }
                let (w, h) = slot.size.unwrap_or_default();
                info!(
                    "{}: stream {w}x{h} starts in the headset ({:?}), {} ms after its encoder",
                    slot.name,
                    d.generation,
                    slot.since.elapsed().as_millis()
                );
                d.announced = true;
                d.known = true;
            }
            if !out.frame {
                continue;
            }
            // The last one takes the frame as it is, the others a copy.
            frames -= 1;
            let bytes = if frames == 0 {
                data.take()
            } else {
                data.clone()
            };
            let seq = d.seq;
            let msg = HostMsg::Frame {
                id: id.wire(),
                generation: d.generation,
                seq,
                pts_us,
                keyframe,
                data: bytes.unwrap_or_default(),
            };
            if r.tx.video(id.wire(), keyframe, msg) {
                d.need_keyframe = false;
                d.seq += 1;
                if d.sent.len() > 256 {
                    d.sent.clear();
                }
                d.sent.insert(seq, Instant::now());
            } else {
                // Too much waiting already: drop until the next keyframe so the
                // decoder never sees a broken reference chain.
                d.need_keyframe = true;
                want_key = true;
            }
        }
        for session in over {
            receivers.remove(&session);
        }
        if keyframe && !want_key {
            slot.keyframe_asked = None;
        }
        key_now || (want_key && slot.ask_keyframe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A headset that decodes H.264 and H.265 up to 8K.
    fn headset() -> state::HeadsetCaps {
        let decoder = |codec| state::Decoder {
            codec,
            max_w: 7680,
            max_h: 4320,
        };
        state::HeadsetCaps {
            decoders: vec![decoder(0), decoder(1)],
            ..state::HeadsetCaps::default()
        }
    }

    /// One that decodes H.264 only up to 32 pixels wide.
    fn small() -> state::HeadsetCaps {
        let mut small = headset();
        small.decoders[0].max_w = 32;
        small
    }

    fn sent(outbox: &Outbox) -> Vec<HostMsg> {
        outbox.sent()
    }

    fn id(n: u32) -> ScreenId {
        ScreenId::from_wire(state::DisplayId(n))
    }

    fn size(router: &Router, id: ScreenId) {
        router.on_event(
            id,
            EncoderEvent::Size {
                width: 64,
                height: 32,
            },
        );
    }

    fn frame(router: &Router, id: ScreenId, keyframe: bool) -> bool {
        router.on_event(
            id,
            EncoderEvent::Frame {
                pts_us: 0,
                keyframe,
                data: vec![0],
            },
        )
    }

    fn keyframe(router: &Router, id: ScreenId) {
        size(router, id);
        frame(router, id, true);
    }

    /// A session's outbox, attached to `router` as session `session` and worn.
    fn attach(router: &Router, session: SessionId, caps: state::HeadsetCaps) -> Arc<Outbox> {
        let outbox = Arc::new(Outbox::default());
        router.attach(session, outbox.clone(), caps, true);
        outbox
    }

    /// The frames in `msgs`: their generation and number.
    fn frames(msgs: &[HostMsg]) -> Vec<(Gen, u32)> {
        msgs.iter()
            .filter_map(|m| match m {
                HostMsg::Frame {
                    generation, seq, ..
                } => Some((*generation, *seq)),
                _ => None,
            })
            .collect()
    }

    /// A stream that starts over with another encoder is announced with its codec in a
    /// new generation, and the headset hears when it ends, even before the new
    /// encoder sent anything.
    #[test]
    fn a_restarted_stream_is_announced_and_ended() {
        let router = Router::new();
        let outbox = attach(&router, 1, headset());
        let (s0, s1) = (id(0), id(1));
        router.publish(1, &BTreeSet::from([s0, s1]));
        router.add_stream(s0, Codec::H265, "test".into(), false);
        keyframe(&router, s0);
        let [
            HostMsg::StreamStart {
                codec: Codec::H265,
                generation: first,
                ..
            },
            HostMsg::Frame {
                generation: g,
                seq: 0,
                ..
            },
        ] = sent(&outbox)[..]
        else {
            panic!("a start and its first frame")
        };
        assert_eq!(first, g);
        assert!(router.is_current(1, s0, first));
        router.add_stream(s0, Codec::H264, "test".into(), false);
        keyframe(&router, s0);
        let [
            HostMsg::StreamStart {
                codec: Codec::H264,
                generation: second,
                ..
            },
            HostMsg::Frame { .. },
        ] = sent(&outbox)[..]
        else {
            panic!("a start and its first frame")
        };
        assert!(second > first, "a new generation");
        assert!(!router.is_current(1, s0, first), "the old one is over");
        router.add_stream(s0, Codec::H265, "test".into(), false);
        router.remove_stream(s0);
        assert_eq!(
            sent(&outbox),
            [HostMsg::StreamEnd {
                id: s0.wire(),
                generation: second
            }]
        );
        // Never shown: nothing to end.
        router.add_stream(s1, Codec::H265, "test".into(), false);
        router.remove_stream(s1);
        assert!(sent(&outbox).is_empty());
    }

    /// "Encoded → shown" is the time from a frame queued to the headset saying it
    /// showed it: one sample a report, for the frame of the current generation it
    /// names, and the frames before it are done too.
    #[test]
    fn a_shown_frame_is_timed_from_when_it_was_sent() {
        let router = Router::new();
        let outbox = attach(&router, 1, headset());
        let s = id(3);
        router.publish(1, &BTreeSet::from([s]));
        router.add_stream(s, Codec::H264, "test".into(), false);
        keyframe(&router, s);
        for _ in 0..3 {
            frame(&router, s, false);
        }
        let generation = match sent(&outbox)[0] {
            HostMsg::StreamStart { generation, .. } => generation,
            ref other => panic!("{other:?}"),
        };
        let shown = |router: &Router| {
            let inner = router.inner.lock().unwrap();
            let d = &inner.receivers[&1].streams[&s];
            (d.shown.count(), d.shown.max(), d.sent.len())
        };
        // Another generation says nothing.
        router.frame_shown(1, s, Gen(generation.0 + 1), 1);
        assert_eq!(shown(&router).0, 0);
        router.frame_shown(1, s, generation, 2);
        let (count, max, waiting) = shown(&router);
        assert_eq!(count, 1);
        assert!(max < Duration::from_secs(1), "{max:?}");
        assert_eq!(waiting, 1, "only the frame after it is still out");
        // Nor does a frame never sent, nor another session's report.
        router.frame_shown(1, s, generation, 99);
        router.frame_shown(2, s, generation, 3);
        assert_eq!(shown(&router).0, 1);
    }

    /// A stream larger than the headset decodes in its codec never starts: no
    /// `StreamStart`, no frames, and `Displays` hears of it to mark it failed.
    #[test]
    fn a_stream_too_big_for_the_headset_never_starts() {
        let router = Router::new();
        let outbox = attach(&router, 1, small());
        let s = id(5);
        router.publish(1, &BTreeSet::from([s]));
        router.add_stream(s, Codec::H264, "test".into(), false);
        keyframe(&router, s);
        assert!(sent(&outbox).is_empty(), "nothing of it goes");
        assert!(router.still_too_big(s));
        // What was found for a headset that left is let go.
        router.detach(1);
        assert!(router.take_too_big().is_empty());
        assert!(!router.still_too_big(s), "nobody to be too big for");
        let outbox = attach(&router, 2, small());
        assert_eq!(router.take_too_big(), [(s, (64, 32), Codec::H264)]);
        // A stream started over is not too big until its encoder says its size.
        router.add_stream(s, Codec::H264, "test".into(), false);
        assert!(!router.still_too_big(s));
        // The same stream fits another headset.
        router.detach(2);
        let outbox_3 = attach(&router, 3, headset());
        router.publish(3, &BTreeSet::from([s]));
        keyframe(&router, s);
        assert!(matches!(sent(&outbox_3)[0], HostMsg::StreamStart { .. }));
        assert!(router.take_too_big().is_empty());
        assert!(sent(&outbox).is_empty());
    }

    /// A stream starts only once the headset has its display in the list: before,
    /// its frames go nowhere, and the start waits for a keyframe after.
    #[test]
    fn a_stream_waits_for_its_display_in_the_list() {
        let router = Router::new();
        let outbox = attach(&router, 1, headset());
        let s = id(7);
        router.add_stream(s, Codec::H264, "test".into(), false);
        keyframe(&router, s);
        assert!(sent(&outbox).is_empty(), "not listed yet");
        assert_eq!(
            router.publish(1, &BTreeSet::from([s])),
            [s],
            "a picture now"
        );
        assert!(router.publish(1, &BTreeSet::from([s])).is_empty());
        keyframe(&router, s);
        assert!(matches!(
            sent(&outbox)[..],
            [HostMsg::StreamStart { .. }, HostMsg::Frame { seq: 0, .. }]
        ));
        // A new session lists everything anew.
        router.detach(1);
        let outbox = attach(&router, 2, headset());
        keyframe(&router, s);
        assert!(sent(&outbox).is_empty());
    }

    /// Two headsets get the same stream, each announced in its own generation and
    /// numbered from 0; the one that joins later gets it from the next keyframe, and
    /// one that leaves takes nothing from the other.
    #[test]
    fn a_stream_fans_out_to_every_headset() {
        let router = Router::new();
        let s = id(1);
        let a = attach(&router, 1, headset());
        router.publish(1, &BTreeSet::from([s]));
        router.add_stream(s, Codec::H264, "test".into(), false);
        keyframe(&router, s);
        frame(&router, s, false);
        assert_eq!(frames(&sent(&a)).len(), 2);
        let b = attach(&router, 2, headset());
        router.publish(2, &BTreeSet::from([s]));
        // Not a keyframe: the new one waits for one, and the encoder is asked.
        assert!(frame(&router, s, false));
        assert!(matches!(sent(&b)[..], [HostMsg::StreamStart { .. }]));
        assert_eq!(frames(&sent(&a)).len(), 1, "the first one goes on");
        frame(&router, s, true);
        let (ga, gb) = (frames(&sent(&a)), frames(&sent(&b)));
        assert_eq!(gb.len(), 1);
        assert_eq!(gb[0].1, 0, "numbered from 0 for the new one");
        assert_ne!(ga[0].0, gb[0].0, "a generation each");
        assert_eq!(ga[0].1, 3, "the first one counts on");
        router.detach(1);
        frame(&router, s, false);
        assert_eq!(frames(&sent(&b)).len(), 1, "the other one goes on");
        assert!(sent(&a).is_empty());
    }

    /// A stream too big for one headset goes to the other, and fails for all only
    /// once the one that decodes it leaves.
    #[test]
    fn a_stream_too_big_for_one_headset_goes_to_the_other() {
        let router = Router::new();
        let s = id(2);
        let big = attach(&router, 1, headset());
        let little = attach(&router, 2, small());
        for session in [1, 2] {
            router.publish(session, &BTreeSet::from([s]));
        }
        router.add_stream(s, Codec::H264, "test".into(), false);
        keyframe(&router, s);
        assert_eq!(frames(&sent(&big)).len(), 1);
        assert!(sent(&little).is_empty(), "nothing of it to the small one");
        assert!(router.take_too_big().is_empty(), "not failed for all");
        assert!(!router.still_too_big(s));
        assert!(router.fits(Codec::H264, (64, 32)));
        router.detach(1);
        assert_eq!(router.take_too_big(), [(s, (64, 32), Codec::H264)]);
        assert!(router.still_too_big(s));
        assert!(!router.fits(Codec::H264, (64, 32)));
    }

    /// A headset taken off gets neither frames nor sound while the other does; put on
    /// again, it gets a fresh picture of what went meanwhile.
    #[test]
    fn a_headset_taken_off_gets_nothing_and_a_fresh_picture_after() {
        let router = Router::new();
        let s = id(4);
        let on = attach(&router, 1, headset());
        let off = attach(&router, 2, headset());
        for session in [1, 2] {
            router.publish(session, &BTreeSet::from([s]));
        }
        router.add_stream(s, Codec::H264, "test".into(), false);
        keyframe(&router, s);
        assert!(router.set_worn(2, false).is_empty());
        let _ = (sent(&on), sent(&off));
        frame(&router, s, true);
        router.send_audio(vec![1, 2]);
        assert_eq!(sent(&on).len(), 2, "a frame and sound");
        assert!(sent(&off).is_empty());
        assert_eq!(router.set_worn(2, true), [s], "a fresh picture");
        assert!(router.set_worn(2, true).is_empty(), "once");
        // Waits for that keyframe.
        frame(&router, s, false);
        assert!(sent(&off).is_empty());
        frame(&router, s, true);
        assert_eq!(frames(&sent(&off)).len(), 1);
    }

    /// A message for every headset goes to each.
    #[test]
    fn a_problem_goes_to_every_headset() {
        let router = Router::new();
        let (a, b) = (attach(&router, 1, headset()), attach(&router, 2, headset()));
        let problem = HostMsg::Problem {
            error: state::ErrorCode::DESKTOP,
            detail: "test".into(),
            display: None,
        };
        router.send_all(problem.clone());
        assert_eq!(sent(&a), std::slice::from_ref(&problem));
        assert_eq!(sent(&b), [problem]);
        assert_eq!(router.headsets().len(), 2);
    }
}
