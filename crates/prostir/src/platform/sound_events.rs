//! Following the sound system without polling: one long-lived `pactl subscribe`, whose
//! every line says what changed (`Event 'change' on sink #57`). The watcher thread
//! sleeps in the kernel on its output; a line about Prostir's own devices makes it
//! read that device again and send what is new to the control loop as a
//! [`SoundEvent`]. Lines about anything else (other programs' streams, the clients
//! `pactl` itself connects as) are read and dropped, and while the sound is idle there
//! are none, so nothing wakes.
//!
//! - Lines that come together (a slider being dragged) are read together and looked
//!   at once.
//! - When `pactl subscribe` ends (PipeWire restarted, the program killed), it starts
//!   again after a pause that doubles up to [`MOST_PAUSE`], and everything is read
//!   again then: what changed meanwhile is said, what did not is not.
//! - Each value is said when it differs from the last one said, so one change that
//!   comes as two lines (the volume, then the mute) is said once.
//!
//! The same loop follows the fake desktop's sound system (`platform/fake`), which
//! writes the same lines into a pipe, so the end-to-end tests run this code.

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use log::{info, warn};

use super::sound::SoundEvent;
use crate::events::{Event, EventSink};

/// The first pause before `pactl subscribe` starts again.
const FIRST_PAUSE: Duration = Duration::from_secs(1);
/// The longest pause before it starts again.
pub const MOST_PAUSE: Duration = Duration::from_secs(30);
/// A subscription that lasted this long ended by chance: the next pause is short again.
const LASTED: Duration = Duration::from_secs(30);

/// What happened, in a line of `pactl subscribe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    New,
    Change,
    Remove,
}

/// What it happened to (only what the watcher looks at; the rest is `Other`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facility {
    Sink,
    Source,
    /// A program's recording stream.
    SourceOutput,
    Other,
}

/// A line of `pactl subscribe`: `Event 'change' on sink #57`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    pub what: What,
    pub on: Facility,
    pub index: u32,
}

/// The line `pactl subscribe` wrote (with `LC_ALL=C`), if it is one.
pub fn parse(line: &str) -> Option<Line> {
    let rest = line.trim().strip_prefix("Event '")?;
    let (what, rest) = rest.split_once('\'')?;
    let what = match what {
        "new" => What::New,
        "change" => What::Change,
        "remove" => What::Remove,
        _ => return None,
    };
    let (facility, index) = rest.strip_prefix(" on ")?.split_once(" #")?;
    let on = match facility {
        "sink" => Facility::Sink,
        "source" => Facility::Source,
        "source-output" => Facility::SourceOutput,
        _ => Facility::Other,
    };
    Some(Line {
        what,
        on,
        index: index.trim().parse().ok()?,
    })
}

/// The numbers the sound system gives Prostir's devices, while they exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Indices {
    /// The headset's output device.
    pub sink: Option<u32>,
    /// The headset microphone, as programs record from it.
    pub mic: Option<u32>,
}

/// What to read again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dirty {
    pub indices: bool,
    pub volume: bool,
    pub recording: bool,
}

impl Dirty {
    const ALL: Self = Self {
        indices: true,
        volume: true,
        recording: true,
    };

    fn or(self, other: Self) -> Self {
        Self {
            indices: self.indices || other.indices,
            volume: self.volume || other.volume,
            recording: self.recording || other.recording,
        }
    }
}

/// The headset's device as read: its level in percent, muted or not, and how many
/// times the PC had set it by then (see `SinkVolume::outdated`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub level: u8,
    pub muted: bool,
    pub writes: u64,
}

/// The sound system as the watcher sees it: `pactl` on Linux, memory in the fake
/// desktop.
pub trait SoundSystem: Send + 'static {
    /// Starts following it: what it says, a line per change, until the subscription
    /// is ended.
    fn subscribe(&mut self) -> std::io::Result<Subscription>;
    /// Prostir's devices now; `None` if the sound system did not answer.
    fn indices(&mut self) -> Option<Indices>;
    /// The headset's device now; `None` if it did not answer.
    fn volume(&mut self) -> Option<Reading>;
    /// Whether a program records from the headset microphone now; `None` if it did not
    /// answer.
    fn recording(&mut self) -> Option<bool>;
}

/// Ends a subscription from any thread: its lines then end. Ending it twice is
/// harmless.
pub trait End: Send + Sync {
    fn end(&self);
}

/// A running subscription.
pub struct Subscription {
    /// What the sound system says, a line per change; at its end the subscription
    /// is over.
    pub lines: std::fs::File,
    pub end: Arc<dyn End>,
}

/// What the watcher knows of the sound system, and the last values it said.
#[derive(Debug, Default)]
pub struct Follow {
    at: Indices,
    /// The last reading said, with the writes done by then: a reading after another
    /// write is said even at the same level (the one before may have been old news).
    volume: Option<Reading>,
    /// The sink was made anew and its first reading is not said yet.
    new_sink: bool,
    recording: bool,
}

impl Follow {
    /// What a line asks to read again. Before a device is there, any line about its
    /// kind may be it.
    pub fn line(&self, line: Option<Line>) -> Dirty {
        let none = Dirty::default();
        let Some(line) = line else {
            return none;
        };
        let ours = |index: Option<u32>, then: Dirty| match index {
            None => Dirty {
                indices: true,
                ..none
            },
            Some(i) if i == line.index && line.what == What::Remove => Dirty {
                indices: true,
                ..none
            },
            Some(i) if i == line.index => then,
            Some(_) => none,
        };
        match line.on {
            Facility::Sink => ours(
                self.at.sink,
                Dirty {
                    volume: true,
                    ..none
                },
            ),
            Facility::Source => ours(
                self.at.mic,
                Dirty {
                    recording: true,
                    ..none
                },
            ),
            // A program began or stopped recording from some source: the microphone
            // is looked at, if it is there.
            Facility::SourceOutput if self.at.mic.is_some() => Dirty {
                recording: true,
                ..none
            },
            Facility::SourceOutput | Facility::Other => none,
        }
    }

    /// Reads again what `dirty` names and says what is new, as of `at` (when its first
    /// line came).
    pub fn look(
        &mut self,
        mut dirty: Dirty,
        at: Instant,
        system: &mut dyn SoundSystem,
    ) -> Vec<SoundEvent> {
        if dirty.indices
            && let Some(now) = system.indices()
        {
            // A device made anew is said as it is, whatever the one before said, and
            // as a new one: its level is the sound system's, not the person's.
            if now.sink != self.at.sink {
                dirty.volume = true;
                self.volume = None;
                self.new_sink = now.sink.is_some();
            }
            // A microphone made anew starts as nobody recording, as the PC's end of it
            // does; one already recorded from is then said.
            if now.mic != self.at.mic {
                dirty.recording = true;
                if now.mic.is_some() {
                    self.recording = false;
                }
            }
            self.at = now;
        }
        let mut said = Vec::new();
        if dirty.volume {
            match self.at.sink {
                // No device: nothing to say until one is made.
                None => {
                    self.volume = None;
                    self.new_sink = false;
                }
                Some(_) => {
                    if let Some(r) = system.volume()
                        && self.volume != Some(r)
                    {
                        self.volume = Some(r);
                        said.push(SoundEvent::Volume {
                            level: r.level,
                            muted: r.muted,
                            at,
                            writes: r.writes,
                            new_device: std::mem::take(&mut self.new_sink),
                        });
                    }
                }
            }
        }
        if dirty.recording {
            let on = match self.at.mic {
                None => Some(false),
                Some(_) => system.recording(),
            };
            if let Some(on) = on
                && on != self.recording
            {
                self.recording = on;
                said.push(SoundEvent::Recording(on));
            }
        }
        said
    }
}

/// The pause before the next subscription: doubling up to [`MOST_PAUSE`].
#[derive(Debug)]
pub struct Pause {
    first: Duration,
    next: Duration,
}

impl Pause {
    pub fn new(first: Duration) -> Self {
        Self { first, next: first }
    }

    /// The pause now, and a longer one next time.
    pub fn take(&mut self) -> Duration {
        let now = self.next;
        self.next = (self.next * 2).min(MOST_PAUSE.max(self.first));
        now
    }

    pub fn reset(&mut self) {
        self.next = self.first;
    }
}

/// What the watcher's thread and its owner share.
struct Shared {
    stopped: AtomicBool,
    /// The subscription running now, to end it when the watcher goes.
    current: Mutex<Option<Arc<dyn End>>>,
}

/// Follows the sound system on a thread of its own ("sound-events") while it lives;
/// dropped, the subscription ends and the thread with it.
pub struct Watcher {
    shared: Arc<Shared>,
    /// Dropped with the watcher: wakes the thread from a pause.
    _wake: mpsc::Sender<()>,
}

impl Watcher {
    /// Starts following `system`; what it says goes to `events`.
    pub fn start(system: impl SoundSystem, events: EventSink) -> Option<Self> {
        Self::start_with(system, events, FIRST_PAUSE)
    }

    fn start_with(
        mut system: impl SoundSystem,
        events: EventSink,
        first_pause: Duration,
    ) -> Option<Self> {
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            current: Mutex::new(None),
        });
        let (wake, woken) = mpsc::channel();
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("sound-events".into())
                .spawn(move || follow(&mut system, &events, &shared, &woken, first_pause))
        };
        match thread {
            Ok(_) => Some(Self {
                shared,
                _wake: wake,
            }),
            Err(e) => {
                warn!("sound: cannot follow the sound system: {e}");
                None
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let current = self.shared.current.lock().unwrap();
        self.shared.stopped.store(true, Ordering::SeqCst);
        if let Some(end) = current.as_ref() {
            end.end();
        }
    }
}

/// The watcher's thread: subscribes, reads every line until the subscription ends,
/// then starts again after a pause, until the watcher goes or nobody listens.
fn follow(
    system: &mut dyn SoundSystem,
    events: &EventSink,
    shared: &Shared,
    woken: &mpsc::Receiver<()>,
    first_pause: Duration,
) {
    let mut known = Follow::default();
    let mut pause = Pause::new(first_pause);
    let mut again = false;
    loop {
        match system.subscribe() {
            Ok(sub) => {
                {
                    let mut current = shared.current.lock().unwrap();
                    if shared.stopped.load(Ordering::SeqCst) {
                        sub.end.end();
                        return;
                    }
                    *current = Some(sub.end.clone());
                }
                info!(
                    "sound: following the sound system{}",
                    if again { " again" } else { "" }
                );
                let started = Instant::now();
                // What changed while nobody listened.
                let listened = say(&known.look(Dirty::ALL, started, system), events)
                    && read(sub.lines, &mut known, system, events);
                shared.current.lock().unwrap().take();
                sub.end.end();
                if !listened || shared.stopped.load(Ordering::SeqCst) {
                    return;
                }
                if started.elapsed() >= LASTED {
                    pause.reset();
                }
            }
            Err(e) => warn!("sound: cannot follow the sound system: {e}"),
        }
        let wait = pause.take();
        warn!(
            "sound: the sound system's events stopped; following it again in {} s",
            wait.as_secs()
        );
        if !matches!(
            woken.recv_timeout(wait),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) || shared.stopped.load(Ordering::SeqCst)
        {
            return;
        }
        again = true;
    }
}

/// Sends what the watcher said; `false` once nobody listens.
fn say(said: &[SoundEvent], events: &EventSink) -> bool {
    said.iter().all(|e| events.send(Event::Sound(*e)))
}

/// Reads lines until the subscription ends (`true`) or nobody listens (`false`). The
/// lines already there when one comes are taken with it and looked at once. The first
/// line shows the subscription is live (`pactl` subscribes a moment after it starts):
/// everything is read again then, so nothing that changed in that moment is missed.
fn read(
    lines: std::fs::File,
    known: &mut Follow,
    system: &mut dyn SoundSystem,
    events: &EventSink,
) -> bool {
    use std::os::fd::AsRawFd;
    let fd = lines.as_raw_fd();
    let mut reader = BufReader::new(lines);
    let mut line = String::new();
    let next = |reader: &mut BufReader<std::fs::File>, line: &mut String| {
        line.clear();
        matches!(reader.read_line(line), Ok(n) if n > 0)
    };
    let mut first = true;
    loop {
        if !next(&mut reader, &mut line) {
            return true;
        }
        let at = Instant::now();
        let mut dirty = known.line(parse(&line));
        if first {
            dirty = Dirty::ALL;
            first = false;
        }
        let mut ended = false;
        while waiting(&reader, fd) {
            if !next(&mut reader, &mut line) {
                ended = true;
                break;
            }
            dirty = dirty.or(known.line(parse(&line)));
        }
        if dirty != Dirty::default() && !say(&known.look(dirty, at, system), events) {
            return false;
        }
        if ended {
            return true;
        }
    }
}

/// Whether more is there to read right now, without waiting.
fn waiting(reader: &BufReader<std::fs::File>, fd: std::os::fd::RawFd) -> bool {
    if reader.buffer().contains(&b'\n') {
        return true;
    }
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, no waiting.
    unsafe { libc::poll(&raw mut pfd, 1, 0) > 0 }
}

/// A pipe: its read end and its write end (closed on exec).
#[cfg(any(test, feature = "fake-desktop"))]
pub fn pipe() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    // SAFETY: pipe2 writes two descriptors, owned right below.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: new descriptors nothing else owns.
    Ok(unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use prostir_proto::volume::{Act, PcVolume};

    use super::*;

    #[test]
    fn lines_of_pactl_subscribe() {
        let line = |s| parse(s);
        assert_eq!(
            line("Event 'change' on sink #57\n"),
            Some(Line {
                what: What::Change,
                on: Facility::Sink,
                index: 57
            })
        );
        assert_eq!(
            line("Event 'new' on source-output #183706"),
            Some(Line {
                what: What::New,
                on: Facility::SourceOutput,
                index: 183706
            })
        );
        assert_eq!(
            line("Event 'remove' on source #3").map(|l| (l.what, l.on)),
            Some((What::Remove, Facility::Source))
        );
        // Other objects are lines too, about nothing the watcher follows.
        for other in [
            "Event 'new' on client #183692",
            "Event 'change' on sink-input #12",
            "Event 'change' on server #4294967295",
            "Event 'change' on card #45",
        ] {
            assert_eq!(line(other).map(|l| l.on), Some(Facility::Other), "{other}");
        }
        for not in [
            "",
            "Event 'change' on sink",
            "Event 'moved' on sink #1",
            "Подія 'change' on sink #1",
            "Event 'change' on sink #x",
        ] {
            assert_eq!(line(not), None, "{not:?}");
        }
    }

    fn change(on: Facility, index: u32) -> Option<Line> {
        Some(Line {
            what: What::Change,
            on,
            index,
        })
    }

    #[test]
    fn what_a_line_asks_to_read_again() {
        let none = Dirty::default();
        let indices = Dirty {
            indices: true,
            ..none
        };
        let volume = Dirty {
            volume: true,
            ..none
        };
        let recording = Dirty {
            recording: true,
            ..none
        };
        let mut known = Follow::default();
        // No devices known: any sink or source may be one being made.
        assert_eq!(known.line(change(Facility::Sink, 9)), indices);
        assert_eq!(known.line(change(Facility::Source, 9)), indices);
        assert_eq!(known.line(change(Facility::SourceOutput, 9)), none);
        known.at = Indices {
            sink: Some(5),
            mic: Some(7),
        };
        assert_eq!(known.line(change(Facility::Sink, 5)), volume);
        assert_eq!(known.line(change(Facility::Sink, 6)), none);
        assert_eq!(known.line(change(Facility::Source, 7)), recording);
        // The sink's monitor has the sink's number and is not the microphone.
        assert_eq!(known.line(change(Facility::Source, 5)), none);
        assert_eq!(known.line(change(Facility::SourceOutput, 40)), recording);
        assert_eq!(
            known.line(Some(Line {
                what: What::Remove,
                on: Facility::Sink,
                index: 5
            })),
            indices
        );
        assert_eq!(
            known.line(Some(Line {
                what: What::New,
                on: Facility::Sink,
                index: 8
            })),
            none
        );
        assert_eq!(known.line(change(Facility::Other, 5)), none);
        assert_eq!(known.line(None), none);
    }

    /// A sound system in memory: its devices, and how often each was read.
    #[derive(Default)]
    struct Memory {
        indices: Indices,
        volume: (u8, bool),
        writes: u64,
        recording: bool,
        silent: bool,
        reads: usize,
        lines: Option<std::fs::File>,
        subscriptions: usize,
    }

    #[derive(Clone, Default)]
    struct System(Arc<Mutex<Memory>>);

    struct CloseLines(Arc<Mutex<Memory>>);

    impl End for CloseLines {
        fn end(&self) {
            self.0.lock().unwrap().lines = None;
        }
    }

    impl System {
        fn with(f: impl FnOnce(&mut Memory)) -> Self {
            let s = Self::default();
            f(&mut s.0.lock().unwrap());
            s
        }

        fn line(&self, line: &str) {
            if let Some(w) = self.0.lock().unwrap().lines.as_mut() {
                writeln!(w, "{line}").unwrap();
            }
        }
    }

    impl SoundSystem for System {
        fn subscribe(&mut self) -> std::io::Result<Subscription> {
            let (lines, w) = pipe()?;
            let mut m = self.0.lock().unwrap();
            m.lines = Some(w);
            m.subscriptions += 1;
            Ok(Subscription {
                lines,
                end: Arc::new(CloseLines(self.0.clone())),
            })
        }

        fn indices(&mut self) -> Option<Indices> {
            let mut m = self.0.lock().unwrap();
            m.reads += 1;
            (!m.silent).then_some(m.indices)
        }

        fn volume(&mut self) -> Option<Reading> {
            let mut m = self.0.lock().unwrap();
            m.reads += 1;
            (!m.silent).then_some(Reading {
                level: m.volume.0,
                muted: m.volume.1,
                writes: m.writes,
            })
        }

        fn recording(&mut self) -> Option<bool> {
            let mut m = self.0.lock().unwrap();
            m.reads += 1;
            (!m.silent).then_some(m.recording)
        }
    }

    fn volume(level: u8, muted: bool) -> impl Fn(&SoundEvent) -> bool {
        move |e| matches!(e, SoundEvent::Volume { level: l, muted: m, .. } if *l == level && *m == muted)
    }

    #[test]
    fn each_value_is_said_once_when_it_changes() {
        let mut system = System::with(|m| {
            m.indices = Indices {
                sink: Some(5),
                mic: Some(7),
            };
            m.volume = (40, false);
        });
        let now = Instant::now();
        let mut known = Follow::default();
        let said = known.look(Dirty::ALL, now, &mut system);
        assert_eq!(said.len(), 1, "the volume, nobody records: {said:?}");
        assert!(volume(40, false)(&said[0]));
        // The same again: nothing.
        assert!(known.look(Dirty::ALL, now, &mut system).is_empty());
        // A program records.
        system.0.lock().unwrap().recording = true;
        let dirty = known.line(change(Facility::Source, 7));
        assert_eq!(
            known.look(dirty, now, &mut system),
            [SoundEvent::Recording(true)]
        );
        // The sound system does not answer: nothing is said, nothing forgotten.
        system.0.lock().unwrap().silent = true;
        assert!(known.look(Dirty::ALL, now, &mut system).is_empty());
        assert!(known.recording);
        system.0.lock().unwrap().silent = false;
        // The microphone goes: nobody records from it any more.
        system.0.lock().unwrap().indices.mic = None;
        let dirty = known.line(Some(Line {
            what: What::Remove,
            on: Facility::Source,
            index: 7,
        }));
        assert_eq!(
            known.look(dirty, now, &mut system),
            [SoundEvent::Recording(false)]
        );
        // The sink made anew at the same level: said again, as a new device.
        system.0.lock().unwrap().indices.sink = Some(9);
        let said = known.look(
            Dirty {
                indices: true,
                ..Dirty::default()
            },
            now,
            &mut system,
        );
        assert!(said.iter().any(volume(40, false)), "{said:?}");
    }

    fn sink_change() -> Dirty {
        Dirty {
            volume: true,
            ..Dirty::default()
        }
    }

    /// A reading taken after another write is said even at the level said before: the
    /// earlier one may have been old news the session dropped, and the device's level
    /// would then never reach it.
    #[test]
    fn a_reading_after_a_write_is_said_at_the_same_level() {
        let mut system = System::with(|m| {
            m.indices.sink = Some(5);
            m.volume = (40, false);
        });
        let now = Instant::now();
        let mut known = Follow::default();
        assert_eq!(known.look(Dirty::ALL, now, &mut system).len(), 1);
        assert!(known.look(sink_change(), now, &mut system).is_empty());
        system.0.lock().unwrap().writes += 1;
        let said = known.look(sink_change(), now, &mut system);
        assert!(
            matches!(
                said[..],
                [SoundEvent::Volume {
                    level: 40,
                    writes: 1,
                    ..
                }]
            ),
            "{said:?}"
        );
        assert!(known.look(sink_change(), now, &mut system).is_empty());
    }

    /// The microphone made anew (its number changes) while a program records from it:
    /// the PC's new end of it starts as nobody recording, so the recording is said.
    #[test]
    fn a_new_microphone_already_recorded_from_is_said() {
        let mut system = System::with(|m| {
            m.indices.mic = Some(7);
            m.recording = true;
        });
        let now = Instant::now();
        let mut known = Follow::default();
        assert_eq!(
            known.look(Dirty::ALL, now, &mut system),
            [SoundEvent::Recording(true)]
        );
        system.0.lock().unwrap().indices.mic = Some(9);
        let dirty = known.line(Some(Line {
            what: What::Remove,
            on: Facility::Source,
            index: 7,
        }));
        assert_eq!(
            known.look(dirty, now, &mut system),
            [SoundEvent::Recording(true)]
        );
        // Gone for good: nobody records from it any more.
        system.0.lock().unwrap().indices.mic = None;
        let dirty = known.line(Some(Line {
            what: What::Remove,
            on: Facility::Source,
            index: 9,
        }));
        assert_eq!(
            known.look(dirty, now, &mut system),
            [SoundEvent::Recording(false)]
        );
    }

    /// The first reading of a sink, and of one made anew, says it is a new device;
    /// the next ones do not. Without a sink nothing is said.
    #[test]
    fn a_new_sink_is_said_as_one() {
        let mut system = System::with(|m| m.volume = (100, false));
        let now = Instant::now();
        let mut known = Follow::default();
        assert!(known.look(Dirty::ALL, now, &mut system).is_empty());
        let new = |said: &[SoundEvent]| match said {
            [SoundEvent::Volume { new_device, .. }] => Some(*new_device),
            _ => None,
        };
        system.0.lock().unwrap().indices.sink = Some(5);
        let said = known.look(Dirty::ALL, now, &mut system);
        assert_eq!(new(&said), Some(true), "{said:?}");
        system.0.lock().unwrap().volume = (30, false);
        let said = known.look(sink_change(), now, &mut system);
        assert_eq!(new(&said), Some(false), "{said:?}");
        // Taken off and put on: the sink goes, and comes with another number.
        system.0.lock().unwrap().indices.sink = None;
        assert!(known.look(Dirty::ALL, now, &mut system).is_empty());
        {
            let mut m = system.0.lock().unwrap();
            m.indices.sink = Some(8);
            m.volume = (100, false);
        }
        let said = known.look(Dirty::ALL, now, &mut system);
        assert_eq!(new(&said), Some(true), "{said:?}");
    }

    /// The volume rule (`PcVolume`) with what the watcher says: the slider asks the
    /// headset once; the PC's own write of the answer (the volume, then the mute: two
    /// lines) is its echo and asks nothing; a level read before the PC's write is old.
    #[test]
    fn the_volume_rule_with_the_device_events() {
        let mut system = System::with(|m| {
            m.indices.sink = Some(5);
            m.volume = (50, false);
        });
        let mut known = Follow::default();
        let mut pc = PcVolume::default();
        let start = Instant::now();
        let ms = |n| start + Duration::from_millis(n);
        // What the session does with an event (see `Session::device_volume`), with
        // the PC's writes so far (a new device would take the headset's level, which
        // this headset has not said).
        fn on(said: Vec<SoundEvent>, pc: &mut PcVolume, writes_now: u64, acts: &mut Vec<Act>) {
            for e in said {
                if let SoundEvent::Volume {
                    level,
                    muted,
                    at,
                    writes,
                    new_device: false,
                } = e
                    && writes >= writes_now
                {
                    acts.extend(pc.device(level, muted, at));
                }
            }
        }
        let mut acts = Vec::new();
        // The first look: a new device, which asks nothing.
        on(
            known.look(Dirty::ALL, ms(0), &mut system),
            &mut pc,
            0,
            &mut acts,
        );
        // The slider moves to 30.
        system.0.lock().unwrap().volume = (30, false);
        let dirty = known.line(change(Facility::Sink, 5));
        on(
            known.look(dirty, ms(100), &mut system),
            &mut pc,
            0,
            &mut acts,
        );
        assert!(
            matches!(acts[..], [Act::Ask(a)] if a.level == 30),
            "{acts:?}"
        );
        // The headset answers with its step, muted; the PC writes it once it rests.
        let answer = prostir_proto::state::Volume {
            level: 33,
            muted: true,
            seq: 0,
            ask: 1,
        };
        assert_eq!(pc.from_headset(&answer, ms(110)), None);
        let Some(Act::Write { level, muted }) = pc.tick(ms(200)) else {
            panic!("not written");
        };
        // The device read just before the PC's write, and the event handled after it:
        // old news, which asks nothing.
        system.0.lock().unwrap().volume = (31, false);
        let early = known.look(known.line(change(Facility::Sink, 5)), ms(199), &mut system);
        assert_eq!(early.len(), 1);
        // The write: the volume, then the mute, two lines read together.
        {
            let mut m = system.0.lock().unwrap();
            m.volume = (level, muted);
            m.writes += 1;
        }
        let before = acts.len();
        on(early, &mut pc, 1, &mut acts);
        assert_eq!(acts.len(), before, "a reading from before the write is old");
        let dirty = known
            .line(change(Facility::Sink, 5))
            .or(known.line(change(Facility::Sink, 5)));
        let said = known.look(dirty, ms(202), &mut system);
        assert_eq!(said.len(), 1, "one change, said once: {said:?}");
        on(said, &mut pc, 1, &mut acts);
        assert_eq!(acts.len(), before, "the PC's own write asked nothing");
        assert_eq!(pc.due(), None);
    }

    #[test]
    fn the_pause_doubles_up_to_the_most() {
        let mut pause = Pause::new(Duration::from_secs(1));
        let secs: Vec<u64> = (0..7).map(|_| pause.take().as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30]);
        pause.reset();
        assert_eq!(pause.take(), Duration::from_secs(1));
    }

    fn sound(events: &mpsc::Receiver<Event>) -> SoundEvent {
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Sound(e)) => e,
            Ok(_) => panic!("not a sound event"),
            Err(e) => panic!("no sound event: {e}"),
        }
    }

    /// The thread: lines wake it, what changed is said; the subscription ending starts
    /// a new one after the pause, which says what changed meanwhile; the watcher
    /// dropped ends it.
    #[test]
    fn the_watcher_follows_and_starts_again() {
        let system = System::with(|m| {
            m.indices = Indices {
                sink: Some(5),
                mic: Some(7),
            };
            m.volume = (40, false);
        });
        let (sink, events) = crate::events::channel();
        let watcher = Watcher::start_with(system.clone(), sink, Duration::from_millis(50)).unwrap();
        assert!(volume(40, false)(&sound(&events)));
        // Noise wakes it but says nothing; the volume after it does.
        system.line("Event 'new' on client #1");
        system.0.lock().unwrap().volume = (41, false);
        system.line("Event 'change' on sink #5");
        assert!(volume(41, false)(&sound(&events)));
        // The subscription ends (pactl died); a change meanwhile is said after the pause.
        system.0.lock().unwrap().lines = None;
        system.0.lock().unwrap().recording = true;
        assert_eq!(sound(&events), SoundEvent::Recording(true));
        assert_eq!(system.0.lock().unwrap().subscriptions, 2);
        // Gone with the watcher.
        drop(watcher);
        let until = Instant::now() + Duration::from_secs(5);
        while system.0.lock().unwrap().lines.is_some() {
            assert!(Instant::now() < until, "the subscription did not end");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            events.recv_timeout(Duration::from_millis(300)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
