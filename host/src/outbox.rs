//! What goes to the headset, most important first: control messages, then sound, then
//! video. A video frame that waited too long is dropped instead of being sent late (and
//! the frames after it, up to the next keyframe, since the decoder cannot use them), so
//! a slow moment on Wi-Fi costs a few frames rather than seconds of added latency.
//!
//! It also counts what it does (see [`OutboxStats`]): how long frames and sound wait,
//! what was sent, and what was dropped and why. The router logs that now and then.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use prostir_proto::HostMsg;
use prostir_proto::stats::{Histogram, per_minute};

/// Sound queued at most: 8 slices of 10 ms. Older slices make room for newer ones.
const MAX_AUDIO: usize = 8;
/// Video frames older than this are not worth sending any more.
const MAX_AGE: Duration = Duration::from_millis(120);
/// Memory guard for queued video.
const MAX_VIDEO_BYTES: usize = 16 << 20;

struct Frame {
    queued: Instant,
    id: u8,
    keyframe: bool,
    bytes: usize,
    msg: HostMsg,
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
}

impl Drops {
    pub fn total(&self) -> u32 {
        self.late + self.after_loss + self.full + self.restart
    }
}

/// One stream's frames since the stats were last taken.
#[derive(Debug, Clone, Default)]
pub struct VideoStats {
    /// From queued to handed to the connection. The socket keeps little unsent data
    /// (`TCP_NOTSENT_LOWAT`), so a frame handed over is on the network soon after.
    pub wait: Histogram,
    /// Frames handed to the connection, keyframes among them, and their bytes.
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
             {} queue full, {} at restart",
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
        )
    }
}

/// What the outbox did since the stats were last taken.
#[derive(Debug, Clone, Default)]
pub struct OutboxStats {
    pub video: BTreeMap<u8, VideoStats>,
    /// Sound: from queued to handed to the connection (it waits behind any frame
    /// already being written).
    pub audio_wait: Histogram,
    /// Sound slices dropped to make room for newer ones.
    pub audio_dropped: u32,
}

#[derive(Default)]
struct State {
    control: VecDeque<HostMsg>,
    /// Sound slices with when each was queued.
    audio: VecDeque<(Instant, HostMsg)>,
    video: VecDeque<Frame>,
    video_bytes: usize,
    /// Streams that lost a frame: their next frames are dropped until a keyframe.
    broken: BTreeSet<u8>,
    /// Streams that lost a frame since the router last looked: they need a keyframe.
    dropped: BTreeSet<u8>,
    stats: OutboxStats,
    closed: bool,
}

#[derive(Default)]
pub struct Outbox {
    state: Mutex<State>,
    ready: Condvar,
}

impl Outbox {
    /// Queues a control message. Starting or ending a stream also drops the frames of
    /// that stream still waiting (they belong to the old size).
    pub fn control(&self, msg: HostMsg) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return false;
        }
        if let HostMsg::StreamStart { id, .. } | HostMsg::StreamEnd { id } = &msg {
            let id = *id;
            let (mut freed, mut gone) = (0, 0);
            s.video.retain(|f| {
                let keep = f.id != id;
                if !keep {
                    freed += f.bytes;
                    gone += 1;
                }
                keep
            });
            s.video_bytes -= freed;
            s.broken.remove(&id);
            if gone > 0 {
                s.stats.video.entry(id).or_default().dropped.restart += gone;
            }
        }
        s.control.push_back(msg);
        self.ready.notify_one();
        true
    }

    /// Queues a slice of sound; the oldest goes when too many wait.
    pub fn audio(&self, msg: HostMsg) {
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return;
        }
        if s.audio.len() >= MAX_AUDIO {
            s.audio.pop_front();
            s.stats.audio_dropped += 1;
        }
        s.audio.push_back((Instant::now(), msg));
        self.ready.notify_one();
    }

    /// Queues an encoded frame. `false`: not queued (the stream must wait for a
    /// keyframe).
    pub fn video(&self, id: u8, keyframe: bool, bytes: usize, msg: HostMsg) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return false;
        }
        if s.video_bytes + bytes > MAX_VIDEO_BYTES && !keyframe {
            s.stats.video.entry(id).or_default().dropped.full += 1;
            return false;
        }
        s.video_bytes += bytes;
        s.video.push_back(Frame {
            queued: Instant::now(),
            id,
            keyframe,
            bytes,
            msg,
        });
        self.ready.notify_one();
        true
    }

    /// What happened since the last call.
    pub fn take_stats(&self) -> OutboxStats {
        std::mem::take(&mut self.state.lock().unwrap().stats)
    }

    /// Streams that lost frames since the last call.
    pub fn take_dropped(&self) -> BTreeSet<u8> {
        std::mem::take(&mut self.state.lock().unwrap().dropped)
    }

    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        s.control.clear();
        s.audio.clear();
        s.video.clear();
        s.video_bytes = 0;
        self.ready.notify_all();
    }

    /// Nothing is waiting to be sent.
    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.control.is_empty() && s.audio.is_empty() && s.video.is_empty()
    }

    /// The next message to send, waiting for one; `None` once closed.
    pub fn next(&self) -> Option<HostMsg> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.closed {
                return None;
            }
            if let Some(msg) = s.control.pop_front() {
                return Some(msg);
            }
            if let Some((queued, msg)) = s.audio.pop_front() {
                s.stats.audio_wait.add(queued.elapsed());
                return Some(msg);
            }
            while let Some(f) = s.video.pop_front() {
                s.video_bytes -= f.bytes;
                let waited = f.queued.elapsed();
                let broken = s.broken.contains(&f.id);
                let late = !f.keyframe && !broken && waited > MAX_AGE;
                if late {
                    s.broken.insert(f.id);
                    s.dropped.insert(f.id);
                } else if f.keyframe {
                    s.broken.remove(&f.id);
                }
                let stats = s.stats.video.entry(f.id).or_default();
                if late {
                    stats.dropped.late += 1;
                    continue;
                }
                if broken && !f.keyframe {
                    stats.dropped.after_loss += 1;
                    continue;
                }
                stats.wait.add(waited);
                stats.frames += 1;
                stats.bytes += f.bytes as u64;
                if f.keyframe {
                    stats.keyframes += 1;
                    stats.keyframe_bytes += f.bytes as u64;
                    stats.keyframe_max = stats.keyframe_max.max(f.bytes);
                }
                return Some(f.msg);
            }
            s = self.ready.wait(s).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u8, keyframe: bool) -> HostMsg {
        HostMsg::Frame {
            id,
            pts_us: 0,
            keyframe,
            data: vec![0; 10],
        }
    }

    #[test]
    fn control_and_sound_go_before_video() {
        let o = Outbox::default();
        assert!(o.video(0, true, 10, frame(0, true)));
        o.audio(HostMsg::Audio { pcm: vec![1] });
        o.control(HostMsg::Focus { id: Some(0) });
        assert_eq!(o.next(), Some(HostMsg::Focus { id: Some(0) }));
        assert_eq!(o.next(), Some(HostMsg::Audio { pcm: vec![1] }));
        assert_eq!(o.next(), Some(frame(0, true)));
    }

    #[test]
    fn late_frames_are_dropped_until_a_keyframe() {
        let o = Outbox::default();
        o.video(1, false, 10, frame(1, false));
        o.video(1, false, 10, frame(1, false));
        // Another stream is not affected.
        std::thread::sleep(MAX_AGE + Duration::from_millis(20));
        o.video(2, false, 10, frame(2, false));
        o.video(1, false, 10, frame(1, false));
        o.video(1, true, 10, frame(1, true));
        assert_eq!(o.next(), Some(frame(2, false)));
        // The late frame went, and the fresh P-frame after it is useless too.
        assert_eq!(o.next(), Some(frame(1, true)));
        assert_eq!(o.take_dropped(), BTreeSet::from([1]));
        assert!(o.is_idle());
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
        assert_eq!((one.frames, one.keyframes, one.bytes), (1, 1, 10));
        assert_eq!(stats.video[&2].frames, 1);
        assert!(stats.video[&2].wait.max() < MAX_AGE);
        // Taken: counting starts over.
        assert!(o.take_stats().video.is_empty());
    }

    #[test]
    fn drops_are_counted_by_reason() {
        let o = Outbox::default();
        assert!(o.video(4, true, MAX_VIDEO_BYTES, frame(4, true)));
        // Full: only keyframes still get in.
        assert!(!o.video(4, false, 10, frame(4, false)));
        assert!(o.video(4, true, 10, frame(4, true)));
        o.control(HostMsg::StreamEnd { id: 4 });
        let stats = o.take_stats();
        assert_eq!(
            stats.video[&4].dropped,
            Drops {
                full: 1,
                restart: 2,
                ..Drops::default()
            }
        );
        assert_eq!(stats.video[&4].dropped.total(), 3);
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
             0 queue full, 0 at restart"
        );
    }

    #[test]
    fn a_new_stream_start_drops_its_old_frames() {
        let o = Outbox::default();
        o.video(3, false, 10, frame(3, false));
        o.control(HostMsg::StreamEnd { id: 3 });
        assert_eq!(o.next(), Some(HostMsg::StreamEnd { id: 3 }));
        assert!(o.is_idle());
    }

    #[test]
    fn sound_keeps_the_newest_slices() {
        let o = Outbox::default();
        for i in 0..(MAX_AUDIO as u8 + 3) {
            o.audio(HostMsg::Audio { pcm: vec![i] });
        }
        assert_eq!(o.next(), Some(HostMsg::Audio { pcm: vec![3] }));
        let stats = o.take_stats();
        assert_eq!(stats.audio_dropped, 3);
        assert_eq!(stats.audio_wait.count(), 1);
    }
}
