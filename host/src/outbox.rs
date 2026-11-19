//! What goes to the headset, packet by packet, most important first: control messages,
//! then sound, then video, the streams taking turns a packet each. A big keyframe goes
//! out in packets of [`FRAGMENT`] bytes, so sound and the answers of the menu wait at
//! most one packet behind it, not the whole frame.
//!
//! A video frame that waited too long is dropped instead of being sent late (and the
//! frames after it, up to the next keyframe, since the decoder cannot use them), so a
//! slow moment on Wi-Fi costs a few frames rather than seconds of added latency. A
//! keyframe makes the older frames of its stream useless, so it drops them.
//!
//! Who does what:
//! - The encoders, the sound capture and the session loop queue messages here
//!   ([`Outbox::control`], [`Outbox::audio`], [`Outbox::video`]), never waiting.
//! - The one writer thread takes the next packet to send with [`Outbox::next`]. A
//!   message it started moves out of the queues into its [`Lanes`], so its packets are
//!   cut and encrypted without holding the queues' lock. A message started is always
//!   finished: there is no cancelling half way.
//!
//! It also counts what it does (see [`OutboxStats`]): how long frames and sound wait,
//! what was sent, and what was dropped and why. The router logs that now and then.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use log::{info, warn};
use prostir_proto::link::RecordSender;
use prostir_proto::packet::{Encoded, FRAGMENT, Outgoing, lane};
use prostir_proto::stats::{Histogram, Pings, per_minute};
use prostir_proto::{HostMsg, MIRROR_ID};

/// Sound queued at most: 8 slices of 10 ms. Older slices make room for newer ones.
const MAX_AUDIO: usize = 8;
/// Video frames older than this are not worth sending any more.
const MAX_AGE: Duration = Duration::from_millis(120);
/// Memory guard for queued video: beyond it only keyframes are taken.
const MAX_VIDEO_BYTES: usize = 16 << 20;
/// Control messages are never dropped; this much of them waiting means the headset
/// reads nothing any more, and the session ends.
const MAX_CONTROL_BYTES: usize = 4 << 20;
/// A ping goes to the headset this often: it shows the round trip, and keeps the
/// connection from falling silent (which ends a session after a few seconds).
pub const PING_EVERY: Duration = Duration::from_secs(1);

// Every stream id the PC makes has a lane of its own (the mirror's is the highest).
const _: () = assert!(MIRROR_ID <= u8::MAX - lane::VIDEO);

struct Frame {
    queued: Instant,
    keyframe: bool,
    msg: Encoded,
}

/// What waits in a stream's lane: frames, and the start and end of the stream, which
/// go in the same lane so they never overtake a frame of the old size that is being
/// sent already.
enum Item {
    Announce(Encoded),
    Frame(Frame),
}

#[derive(Default)]
struct StreamQueue {
    items: VecDeque<Item>,
    /// It lost a frame: its next frames are useless until a keyframe.
    broken: bool,
}

/// Frames of one stream the outbox did not send, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Drops {
    /// Waited longer than [`MAX_AGE`].
    pub late: u32,
    /// Came after a lost frame, before the next keyframe (the decoder cannot use them).
    pub after_loss: u32,
    /// Not queued: too much video waits already.
    pub full: u32,
    /// Still waiting when their stream started over or ended.
    pub restart: u32,
    /// Still waiting when a keyframe of their stream came, which replaces them.
    pub replaced: u32,
}

impl Drops {
    pub fn total(&self) -> u32 {
        self.late + self.after_loss + self.full + self.restart + self.replaced
    }
}

/// One stream's frames since the stats were last taken.
#[derive(Debug, Clone, Default)]
pub struct VideoStats {
    /// From queued to its first packet going out. The socket keeps little unsent data
    /// (`TCP_NOTSENT_LOWAT`), so a frame started is on the network soon after.
    pub wait: Histogram,
    /// Frames sent, keyframes among them, and their bytes.
    pub frames: u32,
    pub bytes: u64,
    pub keyframes: u32,
    pub keyframe_bytes: u64,
    pub keyframe_max: usize,
    pub dropped: Drops,
}

impl VideoStats {
    /// `sent 300 frames (30.0/s), 12.3 Mbit/s; waited p50 … ms; keyframes 2 (12/min),
    /// avg 180 KB, max 210 KB; dropped 0 late, …` for stats taken over `window`.
    pub fn summary(&self, window: Duration) -> String {
        let secs = window.as_secs_f64().max(1e-3);
        let kb = |b: f64| b / 1024.0;
        let d = &self.dropped;
        format!(
            "sent {} frames ({:.1}/s), {:.1} Mbit/s; waited in outbox {}; keyframes {} \
             ({:.0}/min), avg {:.0} KB, max {:.0} KB; dropped {} late, {} after a loss, \
             {} queue full, {} at restart, {} for a newer keyframe",
            self.frames,
            f64::from(self.frames) / secs,
            self.bytes as f64 * 8.0 / secs / 1e6,
            self.wait.summary_ms(),
            self.keyframes,
            per_minute(self.keyframes, window),
            kb(self.keyframe_bytes as f64 / f64::from(self.keyframes.max(1))),
            kb(self.keyframe_max as f64),
            d.late,
            d.after_loss,
            d.full,
            d.restart,
            d.replaced,
        )
    }
}

/// What the outbox did since the stats were last taken.
#[derive(Debug, Clone, Default)]
pub struct OutboxStats {
    pub video: BTreeMap<u8, VideoStats>,
    /// Sound: from queued to going out (it waits behind at most one packet of video).
    pub audio_wait: Histogram,
    /// Sound slices dropped to make room for newer ones.
    pub audio_dropped: u32,
    /// From a ping going out to its pong arriving.
    pub round_trip: Histogram,
}

#[derive(Default)]
struct State {
    control: VecDeque<Encoded>,
    control_bytes: usize,
    /// Sound slices with when each was queued.
    audio: VecDeque<(Instant, Encoded)>,
    streams: BTreeMap<u8, StreamQueue>,
    video_bytes: usize,
    /// Streams that lost a frame since the router last looked: they need a keyframe.
    dropped: BTreeSet<u8>,
    stats: OutboxStats,
    /// How fast the connection may send, and whether the writer has to apply it.
    pace: u64,
    pace_changed: bool,
    pings: Pings,
    /// When the next ping is due (from the writer's first look on).
    next_ping: Option<Instant>,
    closed: bool,
}

#[derive(Default)]
pub struct Outbox {
    state: Mutex<State>,
    ready: Condvar,
}

/// What the writer does next: send a packet in a lane, or change how fast it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Control,
    Sound,
    Video(u8),
    /// Send at most this many bytes a second from now on.
    Pace(u64),
}

/// The messages the writer has started: at most one per lane.
#[derive(Default)]
pub struct Lanes {
    control: Option<Outgoing>,
    sound: Option<Outgoing>,
    video: BTreeMap<u8, Outgoing>,
    /// The stream that sent the last video packet: the next one goes to the stream
    /// after it.
    last_stream: Option<u8>,
}

impl Lanes {
    /// Sends the next packet in `pick`'s lane (or paces the connection).
    pub fn send(&mut self, pick: Pick, tx: &mut dyn RecordSender) -> io::Result<()> {
        match pick {
            Pick::Pace(bytes_per_sec) => {
                tx.set_pace(bytes_per_sec);
                Ok(())
            }
            Pick::Control => send_next(&mut self.control, tx),
            Pick::Sound => send_next(&mut self.sound, tx),
            Pick::Video(id) => {
                self.last_stream = Some(id);
                if let Some(o) = self.video.get_mut(&id)
                    && o.send_next(tx, FRAGMENT)?
                {
                    self.video.remove(&id);
                }
                Ok(())
            }
        }
    }
}

/// Sends the next packet of `out`; it is gone once its last packet went.
fn send_next(out: &mut Option<Outgoing>, tx: &mut dyn RecordSender) -> io::Result<()> {
    if let Some(o) = out
        && o.send_next(tx, FRAGMENT)?
    {
        *out = None;
    }
    Ok(())
}

/// The message's bytes, or why it cannot be sent (the PC's mistake: the session ends).
fn encode(msg: HostMsg) -> Option<Encoded> {
    msg.encode()
        .inspect_err(|e| warn!("a message could not be sent, ending the session: {e}"))
        .ok()
}

impl Outbox {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Queues a control message; `false` once the session is over. Starting or ending
    /// a stream goes in the stream's lane and drops its frames still waiting (they
    /// belong to the old size or codec).
    pub fn control(&self, msg: HostMsg) -> bool {
        let stream = match &msg {
            HostMsg::StreamStart { id, .. } | HostMsg::StreamEnd { id } => Some(*id),
            _ => None,
        };
        let mut s = self.lock();
        if s.closed {
            return false;
        }
        let Some(msg) = encode(msg) else {
            self.close_locked(&mut s);
            return false;
        };
        match stream {
            Some(id) => {
                let gone = s.drop_frames(id);
                if gone > 0 {
                    s.stats.video.entry(id).or_default().dropped.restart += gone;
                }
                let q = s.streams.entry(id).or_default();
                q.broken = false;
                q.items.push_back(Item::Announce(msg));
            }
            None => {
                s.control_bytes += msg.len();
                s.control.push_back(msg);
                if s.control_bytes > MAX_CONTROL_BYTES {
                    warn!(
                        "over {} MB of messages wait for the headset, which reads nothing: \
                         ending the session",
                        MAX_CONTROL_BYTES >> 20
                    );
                    self.close_locked(&mut s);
                    return false;
                }
            }
        }
        self.ready.notify_one();
        true
    }

    /// Queues a slice of sound; the oldest goes when too many wait.
    pub fn audio(&self, msg: HostMsg) {
        let mut s = self.lock();
        if s.closed {
            return;
        }
        let Some(msg) = encode(msg) else {
            self.close_locked(&mut s);
            return;
        };
        if s.audio.len() >= MAX_AUDIO {
            s.audio.pop_front();
            s.stats.audio_dropped += 1;
        }
        s.audio.push_back((Instant::now(), msg));
        self.ready.notify_one();
    }

    /// Queues an encoded frame of stream `id`. `false`: not queued, and the stream
    /// must wait for a keyframe. A keyframe replaces the frames of its stream still
    /// waiting.
    pub fn video(&self, id: u8, keyframe: bool, msg: HostMsg) -> bool {
        let mut s = self.lock();
        if s.closed {
            return false;
        }
        let Some(msg) = encode(msg) else {
            self.close_locked(&mut s);
            return false;
        };
        if keyframe {
            let gone = s.drop_frames(id);
            if gone > 0 {
                s.stats.video.entry(id).or_default().dropped.replaced += gone;
            }
        } else if s.streams.get(&id).is_some_and(|q| q.broken) {
            s.stats.video.entry(id).or_default().dropped.after_loss += 1;
            return false;
        } else if s.video_bytes + msg.len() > MAX_VIDEO_BYTES {
            s.stats.video.entry(id).or_default().dropped.full += 1;
            return false;
        }
        s.video_bytes += msg.len();
        let q = s.streams.entry(id).or_default();
        q.broken = false;
        q.items.push_back(Item::Frame(Frame {
            queued: Instant::now(),
            keyframe,
            msg,
        }));
        self.ready.notify_one();
        true
    }

    /// How fast the connection may send from now on (see `displays::pace_for`).
    pub fn set_pace(&self, bytes_per_sec: u64) {
        let mut s = self.lock();
        if s.pace != bytes_per_sec {
            info!(
                "pacing the connection at {} Mbit/s",
                bytes_per_sec * 8 / 1_000_000
            );
            s.pace = bytes_per_sec;
            s.pace_changed = true;
            self.ready.notify_one();
        }
    }

    /// The headset answered ping `seq`.
    pub fn pong(&self, seq: u32) {
        let mut s = self.lock();
        if let Some(rtt) = s.pings.pong(seq, Instant::now()) {
            s.stats.round_trip.add(rtt);
        }
    }

    /// What happened since the last call.
    pub fn take_stats(&self) -> OutboxStats {
        std::mem::take(&mut self.lock().stats)
    }

    /// Streams that lost frames since the last call.
    pub fn take_dropped(&self) -> BTreeSet<u8> {
        std::mem::take(&mut self.lock().dropped)
    }

    /// The session is over: nothing more is taken or sent.
    pub fn close(&self) {
        let mut s = self.lock();
        self.close_locked(&mut s);
    }

    fn close_locked(&self, s: &mut State) {
        s.closed = true;
        s.control.clear();
        s.audio.clear();
        s.streams.clear();
        s.video_bytes = 0;
        self.ready.notify_all();
    }

    /// The lane to send the next packet in, waiting until there is one; `None` once
    /// closed. A message taken from the queues moves into `lanes`.
    pub fn next(&self, lanes: &mut Lanes) -> Option<Pick> {
        let mut s = self.lock();
        loop {
            if s.closed {
                return None;
            }
            let now = Instant::now();
            let ping_at = *s.next_ping.get_or_insert(now + PING_EVERY);
            if now >= ping_at {
                s.next_ping = Some(now + PING_EVERY);
                let seq = s.pings.ping(now);
                if let Some(ping) = encode(HostMsg::Ping { seq }) {
                    s.control_bytes += ping.len();
                    s.control.push_back(ping);
                }
            }
            if let Some(pick) = s.pick(lanes) {
                return Some(pick);
            }
            let wait = ping_at.saturating_duration_since(now);
            s = self.ready.wait_timeout(s, wait).unwrap().0;
        }
    }

    /// As [`Outbox::next`], without waiting.
    #[cfg(test)]
    fn try_next(&self, lanes: &mut Lanes) -> Option<Pick> {
        let mut s = self.lock();
        if s.closed { None } else { s.pick(lanes) }
    }

    /// Everything waiting, sent and read back as the headset would: for tests.
    #[cfg(test)]
    pub fn sent(&self) -> Vec<HostMsg> {
        let (mut lanes, mut capture) = (Lanes::default(), tests::Capture::default());
        while let Some(pick) = self.try_next(&mut lanes) {
            lanes.send(pick, &mut capture).unwrap();
        }
        capture.messages()
    }
}

impl State {
    /// Drops the frames of stream `id` still waiting (not one being sent); how many.
    fn drop_frames(&mut self, id: u8) -> u32 {
        let Some(q) = self.streams.get_mut(&id) else {
            return 0;
        };
        let (mut freed, mut gone) = (0, 0);
        q.items.retain(|item| match item {
            Item::Frame(f) => {
                freed += f.msg.len();
                gone += 1;
                false
            }
            Item::Announce(_) => true,
        });
        self.video_bytes -= freed;
        gone
    }

    /// The next lane to send in, taking a message into `lanes` when its lane is free:
    /// control, then sound, then the streams in turn.
    fn pick(&mut self, lanes: &mut Lanes) -> Option<Pick> {
        if std::mem::take(&mut self.pace_changed) {
            return Some(Pick::Pace(self.pace));
        }
        if lanes.control.is_none()
            && let Some(msg) = self.control.pop_front()
        {
            self.control_bytes -= msg.len();
            lanes.control = Some(Outgoing::new(lane::CONTROL, msg));
        }
        if lanes.control.is_some() {
            return Some(Pick::Control);
        }
        if lanes.sound.is_none()
            && let Some((queued, msg)) = self.audio.pop_front()
        {
            self.stats.audio_wait.add(queued.elapsed());
            lanes.sound = Some(Outgoing::new(lane::SOUND, msg));
        }
        if lanes.sound.is_some() {
            return Some(Pick::Sound);
        }
        // The streams after the last one served first, then the rest.
        let ids: Vec<u8> = self.streams.keys().copied().collect();
        let first = lanes
            .last_stream
            .map_or(0, |last| ids.partition_point(|&id| id <= last));
        let turn = ids[first..].iter().chain(&ids[..first]);
        turn.copied()
            .find(|&id| lanes.video.contains_key(&id) || self.start_video(id, lanes))
            .map(Pick::Video)
    }

    /// Moves the next message of stream `id` worth sending into its lane; `false`
    /// if there is none.
    fn start_video(&mut self, id: u8, lanes: &mut Lanes) -> bool {
        let Some(q) = self.streams.get_mut(&id) else {
            return false;
        };
        while let Some(item) = q.items.pop_front() {
            let f = match item {
                Item::Announce(msg) => {
                    lanes.video.insert(id, Outgoing::new(lane::video(id), msg));
                    return true;
                }
                Item::Frame(f) => f,
            };
            self.video_bytes -= f.msg.len();
            let waited = f.queued.elapsed();
            let stats = self.stats.video.entry(id).or_default();
            if !f.keyframe && (q.broken || waited > MAX_AGE) {
                if q.broken {
                    stats.dropped.after_loss += 1;
                } else {
                    stats.dropped.late += 1;
                    q.broken = true;
                    self.dropped.insert(id);
                }
                continue;
            }
            q.broken = false;
            stats.wait.add(waited);
            stats.frames += 1;
            stats.bytes += f.msg.len() as u64;
            if f.keyframe {
                stats.keyframes += 1;
                stats.keyframe_bytes += f.msg.len() as u64;
                stats.keyframe_max = stats.keyframe_max.max(f.msg.len());
            }
            lanes
                .video
                .insert(id, Outgoing::new(lane::video(id), f.msg));
            return true;
        }
        false
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Takes the records the writer sends, and reads them back as the headset would.
    #[derive(Default)]
    pub(crate) struct Capture {
        records: Vec<Vec<u8>>,
    }

    impl RecordSender for Capture {
        fn send(&mut self, parts: &[&[u8]]) -> io::Result<()> {
            self.records.push(parts.concat());
            Ok(())
        }

        fn finish(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_pace(&mut self, _bytes_per_sec: u64) {}
    }

    impl Capture {
        /// The lane of each packet sent so far.
        fn lanes(&self) -> Vec<u8> {
            self.records.iter().map(|r| r[1]).collect()
        }

        /// The messages the packets make, in the order they completed.
        pub(crate) fn messages(&self) -> Vec<HostMsg> {
            let mut asm = HostMsg::assembler();
            asm.set_limits(HostMsg::limits(true));
            self.records
                .iter()
                .filter_map(|r| asm.push(r).unwrap())
                .filter_map(|m| HostMsg::from_message(m).unwrap())
                .collect()
        }
    }

    fn frame(id: u8, keyframe: bool, bytes: usize) -> HostMsg {
        HostMsg::Frame {
            id,
            pts_us: 0,
            keyframe,
            data: vec![id; bytes],
        }
    }

    /// Sends `n` packets (fewer if nothing is left).
    fn send(o: &Outbox, lanes: &mut Lanes, out: &mut Capture, n: usize) {
        for _ in 0..n {
            let Some(pick) = o.try_next(lanes) else {
                return;
            };
            lanes.send(pick, out).unwrap();
        }
    }

    #[test]
    fn control_and_sound_go_before_video() {
        let o = Outbox::default();
        assert!(o.video(0, true, frame(0, true, 10)));
        o.audio(HostMsg::Audio { pcm: vec![1] });
        o.control(HostMsg::Focus { id: Some(0) });
        assert_eq!(
            o.sent(),
            [
                HostMsg::Focus { id: Some(0) },
                HostMsg::Audio { pcm: vec![1] },
                frame(0, true, 10)
            ]
        );
    }

    /// Sound queued while a keyframe is going out goes between its packets; so does a
    /// control message.
    #[test]
    fn sound_goes_between_the_packets_of_a_keyframe() {
        let o = Outbox::default();
        let (mut lanes, mut out) = (Lanes::default(), Capture::default());
        assert!(o.video(3, true, frame(3, true, 10 * FRAGMENT)));
        send(&o, &mut lanes, &mut out, 2);
        o.audio(HostMsg::Audio { pcm: vec![7; 1920] });
        send(&o, &mut lanes, &mut out, 2);
        o.control(HostMsg::Recenter);
        send(&o, &mut lanes, &mut out, 100);
        let video = lane::video(3);
        assert_eq!(
            out.lanes()[..6],
            [video, video, lane::SOUND, video, lane::CONTROL, video]
        );
        assert_eq!(out.lanes().len(), 11 + 2);
        let msgs = out.messages();
        assert!(matches!(
            msgs[..],
            [
                HostMsg::Audio { .. },
                HostMsg::Recenter,
                HostMsg::Frame { .. }
            ]
        ));
    }

    /// Streams take turns, a packet each.
    #[test]
    fn streams_take_turns() {
        let o = Outbox::default();
        for id in [1, 4, 9] {
            assert!(o.video(id, true, frame(id, true, 3 * FRAGMENT)));
        }
        let (mut lanes, mut out) = (Lanes::default(), Capture::default());
        send(&o, &mut lanes, &mut out, 100);
        let [a, b, c] = [1, 4, 9].map(lane::video);
        assert_eq!(out.lanes(), [a, b, c, a, b, c, a, b, c, a, b, c]);
        assert_eq!(out.messages().len(), 3);
    }

    /// A keyframe drops the frames of its stream still waiting, not those of others;
    /// a frame already going out is finished first.
    #[test]
    fn a_keyframe_replaces_older_frames() {
        let o = Outbox::default();
        let (mut lanes, mut out) = (Lanes::default(), Capture::default());
        assert!(o.video(1, false, frame(1, false, 2 * FRAGMENT)));
        send(&o, &mut lanes, &mut out, 1);
        assert!(o.video(1, false, frame(1, false, 20)));
        assert!(o.video(2, false, frame(2, false, 20)));
        assert!(o.video(1, true, frame(1, true, 30)));
        send(&o, &mut lanes, &mut out, 100);
        let sent: Vec<(u8, usize)> = out
            .messages()
            .iter()
            .map(|m| match m {
                HostMsg::Frame { id, data, .. } => (*id, data.len()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sent, [(2, 20), (1, 2 * FRAGMENT), (1, 30)]);
        let stats = o.take_stats();
        assert_eq!(stats.video[&1].dropped.replaced, 1);
        assert_eq!(stats.video[&2].dropped.total(), 0);
        // Replaced by a keyframe: nothing to ask the encoder for.
        assert!(o.take_dropped().is_empty());
    }

    /// A stream's start goes in its own lane, after the frame of the old size that is
    /// going out, and drops those still waiting.
    #[test]
    fn a_stream_start_follows_the_frame_being_sent() {
        let o = Outbox::default();
        let (mut lanes, mut out) = (Lanes::default(), Capture::default());
        assert!(o.video(5, true, frame(5, true, 3 * FRAGMENT)));
        send(&o, &mut lanes, &mut out, 1);
        assert!(o.video(5, false, frame(5, false, 10)));
        let start = HostMsg::StreamStart {
            id: 5,
            width: 640,
            height: 360,
            codec: prostir_proto::Codec::H264,
            name: "Prostir-6".into(),
        };
        o.control(start.clone());
        send(&o, &mut lanes, &mut out, 100);
        assert!(out.lanes().iter().all(|&l| l == lane::video(5)));
        assert_eq!(out.messages(), [frame(5, true, 3 * FRAGMENT), start]);
        assert_eq!(o.take_stats().video[&5].dropped.restart, 1);
    }

    #[test]
    fn late_frames_are_dropped_until_a_keyframe() {
        let o = Outbox::default();
        o.video(1, false, frame(1, false, 10));
        o.video(1, false, frame(1, false, 10));
        // Another stream is not affected.
        std::thread::sleep(MAX_AGE + Duration::from_millis(20));
        o.video(2, false, frame(2, false, 10));
        assert_eq!(o.sent(), [frame(2, false, 10)]);
        assert_eq!(o.take_dropped(), BTreeSet::from([1]));
        // Lost: the stream takes only a keyframe now.
        assert!(!o.video(1, false, frame(1, false, 10)));
        assert!(o.video(1, true, frame(1, true, 10)));
        assert!(o.video(1, false, frame(1, false, 10)));
        assert_eq!(o.sent(), [frame(1, true, 10), frame(1, false, 10)]);
        let stats = o.take_stats();
        let one = &stats.video[&1];
        assert_eq!(
            one.dropped,
            Drops {
                late: 1,
                after_loss: 2,
                ..Drops::default()
            }
        );
        assert_eq!((one.frames, one.keyframes), (2, 1));
        assert_eq!(stats.video[&2].frames, 1);
        assert!(stats.video[&2].wait.max() < MAX_AGE);
        // Taken: counting starts over.
        assert!(o.take_stats().video.is_empty());
    }

    #[test]
    fn drops_are_counted_by_reason() {
        let o = Outbox::default();
        assert!(o.video(4, true, frame(4, true, MAX_VIDEO_BYTES)));
        // Full: only keyframes still get in.
        assert!(!o.video(4, false, frame(4, false, 10)));
        assert!(o.video(6, true, frame(6, true, 10)));
        o.control(HostMsg::StreamEnd { id: 6 });
        assert!(o.video(4, true, frame(4, true, 10)));
        let stats = o.take_stats();
        assert_eq!(
            stats.video[&4].dropped,
            Drops {
                full: 1,
                replaced: 1,
                ..Drops::default()
            }
        );
        assert_eq!(stats.video[&6].dropped.restart, 1);
        assert_eq!(stats.video[&4].dropped.total(), 2);
    }

    #[test]
    fn a_summary_names_rates_and_drops() {
        let mut v = VideoStats {
            frames: 300,
            bytes: 15_000_000,
            keyframes: 2,
            keyframe_bytes: 400 * 1024,
            keyframe_max: 300 * 1024,
            ..VideoStats::default()
        };
        v.wait.add(Duration::from_millis(1));
        v.dropped.late = 4;
        assert_eq!(
            v.summary(Duration::from_secs(10)),
            "sent 300 frames (30.0/s), 12.0 Mbit/s; waited in outbox p50 1.0 p95 1.0 max 1.0 ms; \
             keyframes 2 (12/min), avg 200 KB, max 300 KB; dropped 4 late, 0 after a loss, \
             0 queue full, 0 at restart, 0 for a newer keyframe"
        );
    }

    #[test]
    fn sound_keeps_the_newest_slices() {
        let o = Outbox::default();
        for i in 0..(MAX_AUDIO as u8 + 3) {
            o.audio(HostMsg::Audio { pcm: vec![i] });
        }
        let sent = o.sent();
        assert_eq!(sent.len(), MAX_AUDIO);
        assert_eq!(sent[0], HostMsg::Audio { pcm: vec![3] });
        let stats = o.take_stats();
        assert_eq!(stats.audio_dropped, 3);
        assert_eq!(stats.audio_wait.count(), MAX_AUDIO as u32);
    }

    /// A headset that reads nothing: control messages pile up, and past their limit
    /// the session ends instead of the PC holding them for ever.
    #[test]
    fn too_many_control_messages_end_the_session() {
        let o = Outbox::default();
        let notice = || HostMsg::Notice {
            text: "x".repeat(8 * 1024),
        };
        let fit = MAX_CONTROL_BYTES / (8 * 1024 + 7);
        for _ in 0..fit {
            assert!(o.control(notice()));
        }
        assert!(!o.control(notice()), "over the limit");
        assert_eq!(o.next(&mut Lanes::default()), None, "closed");
    }

    /// A new pace reaches the writer before the next packet, once.
    #[test]
    fn the_writer_paces_the_connection() {
        let o = Outbox::default();
        o.control(HostMsg::Recenter);
        o.set_pace(2_500_000);
        let mut lanes = Lanes::default();
        assert_eq!(o.try_next(&mut lanes), Some(Pick::Pace(2_500_000)));
        assert_eq!(o.try_next(&mut lanes), Some(Pick::Control));
        o.set_pace(2_500_000);
        let mut out = Capture::default();
        lanes.send(Pick::Control, &mut out).unwrap();
        assert_eq!(
            o.try_next(&mut lanes),
            None,
            "the same pace again changes nothing"
        );
    }

    /// A ping goes out every second, and its pong gives the round trip.
    #[test]
    fn pings_go_out_every_second() {
        let o = Outbox::default();
        let mut lanes = Lanes::default();
        let start = Instant::now();
        assert_eq!(o.next(&mut lanes), Some(Pick::Control));
        assert!(start.elapsed() >= PING_EVERY - Duration::from_millis(5));
        let mut out = Capture::default();
        lanes.send(Pick::Control, &mut out).unwrap();
        let [HostMsg::Ping { seq }] = out.messages()[..] else {
            panic!("a ping: {:?}", out.messages())
        };
        o.pong(seq);
        assert_eq!(o.take_stats().round_trip.count(), 1);
    }

    /// A message that cannot be sent ends the session, and nothing else is sent.
    #[test]
    fn a_message_over_its_limits_ends_the_session() {
        let o = Outbox::default();
        assert!(!o.control(HostMsg::Order {
            ids: (0..17).collect()
        }));
        assert!(!o.video(0, true, frame(0, true, 1)));
    }
}
