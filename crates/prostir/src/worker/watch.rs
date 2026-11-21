//! Whether the desktop's worker is stuck: it marks the start and end of each step
//! (one call to the desktop or an encoder), and the control loop looks at the step
//! under way when it has handed work over and heard nothing back. A step that takes
//! [`STUCK`] makes the desktop "not responding" (the tray, the log, the local API,
//! the headsets); one that takes [`GIVE_UP`] ends the program, so that whoever
//! started it starts it again (a thread cannot be stopped from outside). A worker that
//! comes back goes on from the newest wanted state, and the PC is fine again.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A step this long and the desktop counts as not responding.
pub const STUCK: Duration = Duration::from_secs(15);
/// A step this long and the program ends, to be started again.
pub const GIVE_UP: Duration = Duration::from_secs(120);

/// The step the worker is on, written by the worker, read by the control loop.
#[derive(Default)]
pub struct Steps(Mutex<Option<(Instant, String)>>);

impl Steps {
    /// The worker starts on `what`.
    pub fn begin(&self, what: impl Into<String>) {
        *self.0.lock().unwrap() = Some((Instant::now(), what.into()));
    }

    /// The step is over.
    pub fn end(&self) {
        *self.0.lock().unwrap() = None;
    }

    /// The step under way: since when, and what.
    pub fn now(&self) -> Option<(Instant, String)> {
        self.0.lock().unwrap().clone()
    }
}

/// What changed in how the worker is doing, as the control loop sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    None,
    /// A step has taken [`STUCK`] or more: what it is.
    Stuck(String),
    /// It was stuck and answers again.
    Back,
    /// A step has taken [`GIVE_UP`]: the program ends.
    GiveUp(String),
}

/// The control loop's view of the worker: whether it waits for it, and whether it
/// said it is stuck.
#[derive(Debug)]
pub struct Watchdog {
    /// Work was handed over and the worker has not said it is done with all of it.
    waiting: bool,
    stuck: bool,
    /// [`STUCK`] and [`GIVE_UP`], or what a build for the end-to-end tests is told.
    stuck_after: Duration,
    give_up_after: Duration,
}

impl Default for Watchdog {
    fn default() -> Self {
        let (stuck_after, give_up_after) = limits();
        Self {
            waiting: false,
            stuck: false,
            stuck_after,
            give_up_after,
        }
    }
}

/// [`STUCK`] and [`GIVE_UP`]; in a build for the end-to-end tests, what
/// `PROSTIR_TEST_DESK_LIMITS` says (`3,6`: seconds; a third number is for
/// `answer_within`), so giving up is tested without waiting 2 min (never a setting
/// of a release).
fn limits() -> (Duration, Duration) {
    #[cfg(feature = "fake-desktop")]
    if let Some((stuck, give_up)) = std::env::var("PROSTIR_TEST_DESK_LIMITS")
        .ok()
        .and_then(|v| {
            let mut n = v.split(',').map(|n| n.trim().parse().ok());
            Some((n.next()??, n.next()??))
        })
    {
        return (Duration::from_secs(stuck), Duration::from_secs(give_up));
    }
    (STUCK, GIVE_UP)
}

impl Watchdog {
    /// How long a step takes before the desktop counts as not responding, and before
    /// the program ends.
    pub fn limits(&self) -> (Duration, Duration) {
        (self.stuck_after, self.give_up_after)
    }

    /// Work was handed over.
    pub fn sent(&mut self) {
        self.waiting = true;
    }

    /// The worker has nothing left to do.
    pub fn settled(&mut self) {
        self.waiting = false;
    }

    /// Whether the desktop counts as not responding now.
    pub fn stuck(&self) -> bool {
        self.stuck
    }

    /// Looks at the step under way (`step`: since when, and what) at `now`.
    pub fn look(&mut self, step: Option<(Instant, String)>, now: Instant) -> Change {
        let long = step
            .filter(|_| self.waiting)
            .map(|(since, what)| (now.saturating_duration_since(since), what));
        match long {
            Some((took, what)) if took >= self.give_up_after => Change::GiveUp(what),
            Some((took, what)) if took >= self.stuck_after => {
                if std::mem::replace(&mut self.stuck, true) {
                    Change::None
                } else {
                    Change::Stuck(what)
                }
            }
            _ if self.stuck => {
                self.stuck = false;
                Change::Back
            }
            _ => Change::None,
        }
    }

    /// When the control loop has to look again (none: it waits for nothing from the
    /// worker, so an idle PC is not woken). `step` is the step under way, at `now`.
    pub fn due(&self, step: Option<Instant>, now: Instant) -> Option<Instant> {
        if !self.waiting {
            return None;
        }
        Some(match step {
            Some(since) if self.stuck => since + self.give_up_after,
            Some(since) => since + self.stuck_after,
            // Between two steps: soon done, or a step starts; looked at again then.
            None => now + self.stuck_after,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(since: Instant) -> Option<(Instant, String)> {
        Some((since, "placing the outputs".into()))
    }

    /// Nothing handed over: never stuck, and nothing to look at (the PC sleeps).
    #[test]
    fn an_idle_worker_is_never_looked_at() {
        let t0 = Instant::now();
        let mut dog = Watchdog::default();
        assert_eq!(dog.due(None, t0), None);
        // A step of its own long ago (a layout retried between sessions) is no
        // waiting of the control loop.
        assert_eq!(dog.look(step(t0), t0 + GIVE_UP * 2), Change::None);
        assert!(!dog.stuck());
    }

    /// A step that takes 15 s is said once; the worker back says so; one of 2 min
    /// gives up.
    #[test]
    fn a_long_step_is_stuck_then_given_up() {
        let t0 = Instant::now();
        let mut dog = Watchdog::default();
        dog.sent();
        assert_eq!(dog.due(None, t0), Some(t0 + STUCK));
        assert_eq!(dog.due(Some(t0), t0), Some(t0 + STUCK));
        assert_eq!(dog.look(step(t0), t0 + STUCK / 2), Change::None);
        assert_eq!(
            dog.look(step(t0), t0 + STUCK),
            Change::Stuck("placing the outputs".into())
        );
        assert!(dog.stuck());
        assert_eq!(dog.look(step(t0), t0 + STUCK * 2), Change::None, "once");
        assert_eq!(dog.due(Some(t0), t0 + STUCK), Some(t0 + GIVE_UP));
        assert_eq!(
            dog.look(step(t0), t0 + GIVE_UP),
            Change::GiveUp("placing the outputs".into())
        );
    }

    /// Back before 2 min: the next step is fine, and the PC is fine again.
    #[test]
    fn a_worker_back_is_fine_again() {
        let t0 = Instant::now();
        let mut dog = Watchdog::default();
        dog.sent();
        assert!(matches!(dog.look(step(t0), t0 + STUCK), Change::Stuck(_)));
        let t1 = t0 + STUCK * 2;
        assert_eq!(dog.look(step(t1), t1), Change::Back);
        assert!(!dog.stuck());
        dog.settled();
        assert_eq!(dog.due(None, t1), None);
        // Settled while stuck (the step ended with its last event): back.
        let mut dog = Watchdog::default();
        dog.sent();
        assert!(matches!(dog.look(step(t0), t0 + STUCK), Change::Stuck(_)));
        dog.settled();
        assert_eq!(dog.look(None, t1), Change::Back);
    }

    /// Many short steps are no long one.
    #[test]
    fn short_steps_add_up_to_nothing() {
        let t0 = Instant::now();
        let mut dog = Watchdog::default();
        dog.sent();
        for i in 0..100 {
            let at = t0 + Duration::from_secs(i);
            assert_eq!(
                dog.look(step(at), at + Duration::from_millis(900)),
                Change::None
            );
        }
    }
}
