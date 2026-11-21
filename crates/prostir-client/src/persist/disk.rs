//! The one place the headset's own files are read and written: once at the start,
//! and from then on by the `persist` thread.
//!
//! The thread is handed a file's text through a slot of its own: the newest text of a
//! file replaces one still waiting (the newest wins), and a file being written while a
//! new text comes is written again after. The texts move between three buffers for
//! each file, the main thread's draft, the slot and the thread's own, which are
//! swapped and never made anew. The main thread holds the lock only to swap a buffer,
//! the thread only to take one; the write itself (with its `fsync`) is done without
//! it.

#![expect(
    clippy::disallowed_methods,
    reason = "the files' writer: the shell's thread that reads and writes the headset's files"
)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::warn;

use super::File;
use crate::layout::{self, Desk, Placement};
use crate::profiles::{self, Profile};
use crate::settings::Settings;

/// What the files held at the start.
pub struct Loaded {
    pub settings: Settings,
    pub desk: Desk,
    pub profiles: [Profile; profiles::SLOTS],
    /// `profiles.txt` is there: a profile was saved before.
    pub profiles_saved: bool,
    pub keyboard: Option<Placement>,
}

impl Loaded {
    /// Reads the files in `dir`; one that is not there, or cannot be read, is as if
    /// empty.
    pub fn read(dir: &Path) -> Self {
        let text = |file: File| std::fs::read_to_string(dir.join(file.name())).ok();
        Self {
            settings: text(File::Settings)
                .map(|t| Settings::parse(&t))
                .unwrap_or_default(),
            desk: text(File::Desk)
                .map(|t| Desk::parse(&t))
                .unwrap_or_default(),
            profiles: profiles::parse(&text(File::Profiles).unwrap_or_default()),
            profiles_saved: dir.join(File::Profiles.name()).exists(),
            keyboard: text(File::Keyboard).and_then(|t| layout::parse_keyboard(&t)),
        }
    }
}

/// What the thread says back.
#[derive(Debug, PartialEq, Eq)]
pub enum PersistEvent {
    /// `file` could not be written; the next save of it tries again.
    Failed { file: File, error: String },
}

const FILES: usize = File::ALL.len();

/// A file's text on its way to the thread.
#[derive(Default)]
struct Slot {
    text: String,
    /// `text` waits to be written.
    queued: bool,
}

#[derive(Default)]
struct State {
    slots: [Slot; FILES],
    /// The thread is writing a file.
    busy: bool,
    /// Asked to end: the thread writes what waits, then ends.
    stop: bool,
    /// The thread has ended.
    finished: bool,
    /// Writes done, for the tests.
    #[cfg(test)]
    writes: u32,
    /// The thread takes nothing while held, for the tests.
    #[cfg(test)]
    held: bool,
}

impl State {
    /// The first file waiting, unless the thread is held.
    fn next(&self) -> Option<usize> {
        #[cfg(test)]
        if self.held {
            return None;
        }
        self.slots.iter().position(|s| s.queued)
    }

    /// Something is still to be written or being written.
    fn pending(&self) -> bool {
        self.busy || self.slots.iter().any(|s| s.queued)
    }
}

/// The lock and the one condition both sides wait on: work for the thread, a write
/// done or the thread ended for the main thread.
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_for<'a>(&self, state: MutexGuard<'a, State>, left: Duration) -> MutexGuard<'a, State> {
        self.changed
            .wait_timeout(state, left)
            .map_or_else(|e| e.into_inner().0, |(state, _)| state)
    }
}

/// The `persist` thread of one start of the app, and the main thread's side of it.
/// Dropped, it writes what waits and ends within [`Saver::STOPS_WITHIN`].
pub struct Saver {
    shared: Arc<Shared>,
    /// Where each file is written.
    paths: [PathBuf; FILES],
    /// The main thread's buffer for each file's text, swapped into its slot.
    drafts: [String; FILES],
    events: Receiver<PersistEvent>,
    /// Kept for writing in place when the thread could not start.
    to_app: Sender<PersistEvent>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    /// How long the main thread waits for what waits to be written when the app is
    /// paused, the headset taken off, or the app closes.
    pub const STOPS_WITHIN: Duration = Duration::from_millis(300);

    /// Starts the thread, which writes the files in `dir`.
    pub fn start(dir: &Path) -> Self {
        let paths = File::ALL.map(|f| dir.join(f.name()));
        let shared = Arc::new(Shared::default());
        let (to_app, events) = mpsc::channel();
        let thread = {
            let (shared, paths, to_app) = (shared.clone(), paths.clone(), to_app.clone());
            std::thread::Builder::new()
                .name("persist".into())
                .spawn(move || run(&shared, &paths, &to_app))
                .inspect_err(|e| warn!("no persist thread, files are written in place: {e}"))
                .ok()
        };
        Self {
            shared,
            paths,
            drafts: Default::default(),
            events,
            to_app,
            thread,
        }
    }

    /// Hands `file` to the thread, its text written by `write` into a buffer kept from
    /// the last time. A text of it still waiting is replaced.
    pub fn save(&mut self, file: File, write: impl FnOnce(&mut String)) {
        let i = file.index();
        let Some(draft) = self.drafts.get_mut(i) else {
            return;
        };
        draft.clear();
        write(draft);
        if self.thread.is_none() {
            if let Some(path) = self.paths.get(i) {
                write_file(file, path, draft, &self.to_app);
            }
            return;
        }
        let mut state = self.shared.lock();
        if let Some(slot) = state.slots.get_mut(i) {
            std::mem::swap(&mut slot.text, draft);
            slot.queued = true;
        }
        drop(state);
        self.shared.changed.notify_all();
    }

    /// Waits until everything handed over is written, for `within` at most; true when
    /// it is.
    pub fn flush(&self, within: Duration) -> bool {
        let until = Instant::now() + within;
        let mut state = self.shared.lock();
        while state.pending() && !state.finished {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.shared.wait_for(state, left);
        }
        !state.pending()
    }

    /// What the thread said since the last call.
    pub fn try_recv(&self) -> Option<PersistEvent> {
        self.events.try_recv().ok()
    }

    /// Asks the thread to write what waits and end, and waits for it for `within` at
    /// most. A thread that did not end by then is left (the log says so): a new start
    /// in the same process does not wait for it, and its temporary files have names
    /// of their own (see `write_atomic`).
    pub fn stop(&mut self, within: Duration) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let until = Instant::now() + within;
        let mut state = self.shared.lock();
        state.stop = true;
        self.shared.changed.notify_all();
        while !state.finished {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self.shared.wait_for(state, left);
        }
        let finished = state.finished;
        drop(state);
        if !finished {
            warn!(
                "the persist thread did not stop within {} ms",
                within.as_millis()
            );
        } else if thread.join().is_err() {
            warn!("the persist thread panicked");
        }
    }

    /// Holds the thread (it takes nothing) or lets it go on.
    #[cfg(test)]
    fn hold(&self, held: bool) {
        self.shared.lock().held = held;
        self.shared.changed.notify_all();
    }

    #[cfg(test)]
    fn writes(&self) -> u32 {
        self.shared.lock().writes
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        self.stop(Self::STOPS_WITHIN);
    }
}

/// The thread: takes the first file waiting, writes it, and again, until asked to end
/// with nothing waiting.
fn run(shared: &Shared, paths: &[PathBuf; FILES], to_app: &Sender<PersistEvent>) {
    let mut work = String::new();
    let mut state = shared.lock();
    loop {
        if let Some(i) = state.next() {
            if let Some(slot) = state.slots.get_mut(i) {
                std::mem::swap(&mut slot.text, &mut work);
                slot.queued = false;
            }
            state.busy = true;
            drop(state);
            if let (Some(file), Some(path)) = (File::ALL.get(i), paths.get(i)) {
                write_file(*file, path, &work, to_app);
            }
            state = shared.lock();
            state.busy = false;
            #[cfg(test)]
            {
                state.writes += 1;
            }
            shared.changed.notify_all();
        } else if state.stop && !state.pending() {
            state.finished = true;
            drop(state);
            shared.changed.notify_all();
            return;
        } else {
            state = shared.wait(state);
        }
    }
}

/// Writes `text` as `file` at `path` (whole or not at all), and says so when it
/// cannot.
fn write_file(file: File, path: &Path, text: &str, to_app: &Sender<PersistEvent>) {
    if let Err(e) = prostir_proto::secure::write_atomic(path, text.as_bytes()) {
        warn!("cannot save {}: {e}", path.display());
        let _ = to_app.send(PersistEvent::Failed {
            file,
            error: e.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own for each test, empty.
    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("prostir-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(dir: &Path, file: File) -> String {
        std::fs::read_to_string(dir.join(file.name())).unwrap_or_default()
    }

    /// Saves of a file while it waits replace each other: the newest is written, once.
    #[test]
    fn the_newest_text_wins() {
        let dir = folder("newest");
        let mut saver = Saver::start(&dir);
        saver.hold(true);
        for n in 0..100 {
            saver.save(File::Desk, |out| {
                use std::fmt::Write as _;
                let _ = writeln!(out, "{n}");
            });
        }
        assert!(!saver.flush(Duration::from_millis(20)), "held");
        assert_eq!(read(&dir, File::Desk), "");
        saver.hold(false);
        assert!(saver.flush(Duration::from_secs(10)));
        assert_eq!(read(&dir, File::Desk), "99\n");
        assert_eq!(saver.writes(), 1);
        assert_eq!(saver.try_recv(), None);
        drop(saver);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every file handed over is written by the flush, each with its own text; the
    /// buffers keep their memory, and a shorter text after a longer one is whole.
    #[test]
    fn a_flush_writes_every_file() {
        let dir = folder("flush");
        let mut saver = Saver::start(&dir);
        for file in File::ALL {
            saver.save(file, |out| out.push_str("a long first text\n"));
        }
        assert!(saver.flush(Duration::from_secs(10)));
        for file in File::ALL {
            saver.save(file, |out| out.push_str(file.name()));
        }
        assert!(saver.flush(Duration::from_secs(10)));
        for file in File::ALL {
            assert_eq!(read(&dir, file), file.name());
        }
        drop(saver);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that cannot be written is told, every time; the next save tries again
    /// and, once it can, writes it.
    #[test]
    fn a_failed_write_is_told_and_tried_again() {
        let parent = folder("failed");
        // Not there yet: nothing can be written in it.
        let dir = parent.join("later");
        let mut saver = Saver::start(&dir);
        for _ in 0..2 {
            saver.save(File::Profiles, |out| out.push_str("[0]\n"));
            assert!(saver.flush(Duration::from_secs(10)));
            match saver.try_recv() {
                Some(PersistEvent::Failed { file, error }) => {
                    assert_eq!(file, File::Profiles);
                    assert!(!error.is_empty());
                }
                None => panic!("the failure is told"),
            }
        }
        std::fs::create_dir_all(&dir).unwrap();
        saver.save(File::Profiles, |out| out.push_str("[1]\n"));
        assert!(saver.flush(Duration::from_secs(10)));
        assert_eq!(saver.try_recv(), None);
        assert_eq!(read(&dir, File::Profiles), "[1]\n");
        drop(saver);
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Stopped, the thread writes what waits and ends; the main thread waits for it.
    #[test]
    fn stopping_writes_what_waits() {
        let dir = folder("stop");
        let mut saver = Saver::start(&dir);
        saver.hold(true);
        saver.save(File::Settings, |out| out.push_str("mic off\n"));
        saver.hold(false);
        saver.stop(Duration::from_secs(10));
        assert!(saver.thread.is_none());
        assert_eq!(read(&dir, File::Settings), "mic off\n");
        // Stopped twice, or dropped after: nothing more to wait for.
        saver.stop(Duration::from_secs(10));
        drop(saver);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A thread held past the time given is left, and said so; the app goes on.
    #[test]
    fn a_thread_that_does_not_stop_in_time_is_left() {
        let dir = folder("left");
        let mut saver = Saver::start(&dir);
        saver.hold(true);
        saver.save(File::Keyboard, |out| out.push_str("0 0 0 0 0 0 0 1 0.5\n"));
        let shared = saver.shared.clone();
        let started = Instant::now();
        saver.stop(Duration::from_millis(30));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!shared.lock().finished);
        // Let go, it writes and ends on its own.
        shared.lock().held = false;
        shared.changed.notify_all();
        let until = Instant::now() + Duration::from_secs(10);
        while !shared.lock().finished && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(shared.lock().finished);
        assert_eq!(read(&dir, File::Keyboard), "0 0 0 0 0 0 0 1 0.5\n");
        drop(saver);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The files of an earlier start come back; those not there are as if empty.
    #[test]
    fn the_start_reads_what_was_saved() {
        let dir = folder("read");
        let empty = Loaded::read(&dir);
        assert_eq!(empty.settings, Settings::default());
        assert!(empty.desk.arrangement().placements.is_empty());
        assert!(!empty.profiles_saved && empty.keyboard.is_none());
        let mut saver = Saver::start(&dir);
        let settings = Settings {
            mic: false,
            ..Settings::default()
        };
        saver.save(File::Settings, |out| settings.write_text(out));
        let profiles: [Profile; profiles::SLOTS] = std::array::from_fn(Profile::default_for);
        saver.save(File::Profiles, |out| profiles::write_text(out, &profiles));
        let place = Placement {
            position: glam::Vec3::new(0.0, 1.0, -0.5),
            rotation: glam::Quat::IDENTITY,
            width: 0.5,
        };
        saver.save(File::Keyboard, |out| layout::write_keyboard(out, &place));
        let mut desk = Desk::default();
        desk.keep(
            [(3, place)],
            &std::collections::HashMap::new(),
            ((glam::Vec3::ZERO, 0.0), layout::RADIUS),
            false,
        );
        saver.save(File::Desk, |out| desk.write_text(out));
        drop(saver);
        let back = Loaded::read(&dir);
        assert_eq!(back.settings, settings);
        assert!(back.profiles_saved);
        assert_eq!(back.profiles, profiles);
        assert_eq!(back.keyboard, Some(place));
        assert_eq!(back.desk.arrangement().placements[&3], place);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
