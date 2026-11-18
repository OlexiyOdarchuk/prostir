//! What goes to the headset, most important first: control messages, then sound, then
//! video. A video frame that waited too long is dropped instead of being sent late (and
//! the frames after it, up to the next keyframe, since the decoder cannot use them), so
//! a slow moment on Wi-Fi costs a few frames rather than seconds of added latency.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use prostir_proto::HostMsg;

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

#[derive(Default)]
struct State {
    control: VecDeque<HostMsg>,
    audio: VecDeque<HostMsg>,
    video: VecDeque<Frame>,
    video_bytes: usize,
    /// Streams that lost a frame: their next frames are dropped until a keyframe.
    broken: BTreeSet<u8>,
    /// Streams that lost a frame since the router last looked: they need a keyframe.
    dropped: BTreeSet<u8>,
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
            let mut freed = 0;
            s.video.retain(|f| {
                let keep = f.id != id;
                if !keep {
                    freed += f.bytes;
                }
                keep
            });
            s.video_bytes -= freed;
            s.broken.remove(&id);
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
        }
        s.audio.push_back(msg);
        self.ready.notify_one();
    }

    /// Queues an encoded frame. `false`: not queued (the stream must wait for a
    /// keyframe).
    pub fn video(&self, id: u8, keyframe: bool, bytes: usize, msg: HostMsg) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.closed || (s.video_bytes + bytes > MAX_VIDEO_BYTES && !keyframe) {
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
            if let Some(msg) = s.audio.pop_front() {
                return Some(msg);
            }
            while let Some(f) = s.video.pop_front() {
                s.video_bytes -= f.bytes;
                if f.keyframe {
                    s.broken.remove(&f.id);
                    return Some(f.msg);
                }
                if s.broken.contains(&f.id) {
                    continue;
                }
                if f.queued.elapsed() > MAX_AGE {
                    s.broken.insert(f.id);
                    s.dropped.insert(f.id);
                    continue;
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
        o.control(HostMsg::Focus { id: 0 });
        assert_eq!(o.next(), Some(HostMsg::Focus { id: 0 }));
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
    }
}
