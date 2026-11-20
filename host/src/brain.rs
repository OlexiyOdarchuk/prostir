//! The PC's states between headsets, its clocks, and who may open a session: the
//! rules of the control loop (`main.rs`) without its threads, files or desktop, so
//! they are tested with made-up times.
//!
//! ```text
//!   Idle       --a session opens-->                            Live
//!   Live       --the last session ends, monitors kept-->        Lingering { until }
//!   Live       --the last session ends, taken off or broken-->  Idle
//!   Lingering  --a session opens-->                            Live (the same monitors)
//!   Lingering  --until passed-->                               Idle
//! ```
//!
//! Idle has no monitors and no deadline: the control loop sleeps until an event comes.
//! Live looks at the desktop on a clock (see [`Clocks`]) until step 10 makes that
//! events too.

use std::time::{Duration, Instant};

/// How often a live PC does its housekeeping: the streams' reports, the PC's own
/// displays, whether a program records from the microphone, sessions taken off or
/// broken, commands the headset did not answer.
pub const HOUSEKEEPING: Duration = Duration::from_millis(500);
/// How often a live PC looks which output has the mouse (to dim the others in the
/// headset) and which plays sound.
pub const LOOK_AT_FOCUS: Duration = Duration::from_millis(150);

/// The clocks of a live PC: when it next does its housekeeping and looks at the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clocks {
    housekeeping: Instant,
    focus: Instant,
}

impl Clocks {
    /// Both due at once.
    fn new(now: Instant) -> Self {
        Self {
            housekeeping: now,
            focus: now,
        }
    }

    /// Whether the housekeeping is due at `now`; if so, the next one is set.
    pub fn housekeeping(&mut self, now: Instant) -> bool {
        Self::due(&mut self.housekeeping, now, HOUSEKEEPING)
    }

    /// Whether looking at the focus is due at `now`; if so, the next look is set.
    pub fn focus(&mut self, now: Instant) -> bool {
        Self::due(&mut self.focus, now, LOOK_AT_FOCUS)
    }

    fn due(at: &mut Instant, now: Instant, every: Duration) -> bool {
        if now < *at {
            return false;
        }
        *at = now + every;
        true
    }

    fn next(&self) -> Instant {
        self.housekeeping.min(self.focus)
    }
}

/// Where the PC is between headsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No session and no monitors.
    Idle,
    /// At least one session.
    Live(Clocks),
    /// No session; the monitors wait for the headset to come back until `until`
    /// (`None`: a time too far to reckon, so for good).
    Lingering { until: Option<Instant> },
}

/// How the last session ended, for what comes after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastEnded {
    /// The monitors are kept for a while (`--linger`).
    Keep,
    /// The headset was taken off, or a monitor or encoder broke: the monitors go now.
    Drop,
}

impl Phase {
    /// A session opened at `now` (another one, or the same headset again, keeps the
    /// clocks).
    pub fn session_opened(&mut self, now: Instant) {
        if !matches!(self, Phase::Live(_)) {
            *self = Phase::Live(Clocks::new(now));
        }
    }

    /// The last session ended at `now`; the monitors are kept for `linger`, if at all.
    pub fn last_session_ended(&mut self, how: LastEnded, now: Instant, linger: Duration) {
        *self = match how {
            LastEnded::Keep if !linger.is_zero() => Phase::Lingering {
                until: now.checked_add(linger),
            },
            _ => Phase::Idle,
        };
    }

    /// When the control loop has to look again without an event, if ever.
    pub fn due(&self) -> Option<Instant> {
        match self {
            Phase::Idle => None,
            Phase::Live(clocks) => Some(clocks.next()),
            Phase::Lingering { until } => *until,
        }
    }

    /// At `now`: whether the monitors' time ran out (the phase is then `Idle`).
    pub fn expire(&mut self, now: Instant) -> bool {
        match *self {
            Phase::Lingering { until: Some(until) } if now >= until => {
                *self = Phase::Idle;
                true
            }
            _ => false,
        }
    }
}

/// How many headsets may have a session at once.
///
/// Accepting, admitting and keeping sessions (by the headset's key, each with its own
/// number, reader and writer) already work for more than one. Raising this is not
/// enough, though: these still assume a single headset, and are the next sub-step
/// (step 11 of 0.2.0 needs them):
/// - `displays::Router` has one outbox: `attach` replaces the headset it sends to,
///   `detach` stops sending to anyone, and `publish` takes one headset's list;
/// - `Session::start` fits the codec of every stream to this headset
///   (`Displays::fit_codec`), stops what is too big for it (`stop_too_big`), and
///   starts every stream over (`headset_connected`, which also clears "taken off");
/// - `Session::finish` pauses every encoder (`set_streaming(false)`) and detaches the
///   router for all, whichever headset left;
/// - worn or not, and so "taken off" (`Displays::set_worn`, `taken_off_for`), the PC's
///   end of the microphone (`set_mic`) and its pause are one for the desktop;
/// - the tray's `Control.via`, `no_limits`, `mic_denied` and battery show one headset,
///   and a session's end clears them for all;
/// - commands from the tray and the test hook go to one session (the main one).
pub const MAX_HEADSETS: usize = 1;

/// What becomes of a headset that asked to open a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// A session of its own, next to the others.
    Open,
    /// It has a session already, which it no longer reads (it lost the connection and
    /// came back before the PC noticed): the new session replaces it.
    Replace,
    /// Too many headsets at once.
    Refuse,
}

/// Whether headset `key` may open a session while `live` have one.
pub fn admit<'a, K: PartialEq + 'a>(live: impl IntoIterator<Item = &'a K>, key: &K) -> Admit {
    let mut count = 0;
    for k in live {
        if k == key {
            return Admit::Replace;
        }
        count += 1;
    }
    if count < MAX_HEADSETS {
        Admit::Open
    } else {
        Admit::Refuse
    }
}

/// The earliest of the times, if any: when the control loop has to look again.
pub fn earliest(times: impl IntoIterator<Item = Option<Instant>>) -> Option<Instant> {
    times.into_iter().flatten().min()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINGER: Duration = Duration::from_secs(30);
    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_session_lingers_and_then_the_pc_idles() {
        let t0 = Instant::now();
        let mut phase = Phase::Idle;
        assert_eq!(phase.due(), None, "idle sleeps without a deadline");
        phase.session_opened(t0);
        assert!(matches!(phase, Phase::Live(_)));
        phase.last_session_ended(LastEnded::Keep, t0, LINGER);
        assert_eq!(phase.due(), Some(t0 + LINGER));
        assert!(!phase.expire(t0 + LINGER - MS));
        assert!(matches!(phase, Phase::Lingering { .. }));
        assert!(phase.expire(t0 + LINGER));
        assert_eq!(phase, Phase::Idle);
        assert_eq!(phase.due(), None);
        assert!(!phase.expire(t0 + LINGER * 2), "only once");
    }

    /// `--linger` as long as a number holds keeps the monitors for good instead of
    /// overflowing the clock.
    #[test]
    fn a_linger_too_long_to_reckon_never_ends() {
        let t0 = Instant::now();
        let mut phase = Phase::Live(Clocks::new(t0));
        phase.last_session_ended(LastEnded::Keep, t0, Duration::from_secs(u64::MAX));
        assert_eq!(phase, Phase::Lingering { until: None });
        assert_eq!(phase.due(), None, "no deadline");
        assert!(!phase.expire(t0 + Duration::from_secs(3600 * 24 * 365)));
        phase.session_opened(t0);
        assert!(matches!(phase, Phase::Live(_)));
    }

    #[test]
    fn a_headset_back_in_time_keeps_its_monitors() {
        let t0 = Instant::now();
        let mut phase = Phase::Idle;
        phase.session_opened(t0);
        phase.last_session_ended(LastEnded::Keep, t0, LINGER);
        phase.session_opened(t0 + LINGER - MS);
        assert!(matches!(phase, Phase::Live(_)));
        assert!(
            !phase.expire(t0 + LINGER * 2),
            "a live session never expires"
        );
    }

    #[test]
    fn taken_off_broken_or_no_linger_idles_at_once() {
        let t0 = Instant::now();
        let mut phase = Phase::Live(Clocks::new(t0));
        phase.last_session_ended(LastEnded::Drop, t0, LINGER);
        assert_eq!(phase, Phase::Idle);
        let mut phase = Phase::Live(Clocks::new(t0));
        phase.last_session_ended(LastEnded::Keep, t0, Duration::ZERO);
        assert_eq!(phase, Phase::Idle);
    }

    /// Live, the loop wakes for the focus every 150 ms and for the housekeeping every
    /// 500 ms; each is due once per period however often the loop runs.
    #[test]
    fn live_clocks_wake_the_loop_on_time() {
        let t0 = Instant::now();
        let mut phase = Phase::Idle;
        phase.session_opened(t0);
        assert_eq!(phase.due(), Some(t0), "both due at the start");
        let Phase::Live(clocks) = &mut phase else {
            unreachable!()
        };
        assert!(clocks.housekeeping(t0) && clocks.focus(t0));
        assert!(!clocks.housekeeping(t0) && !clocks.focus(t0), "once");
        assert_eq!(phase.due(), Some(t0 + LOOK_AT_FOCUS));
        let Phase::Live(clocks) = &mut phase else {
            unreachable!()
        };
        // Events in between change nothing.
        assert!(!clocks.focus(t0 + LOOK_AT_FOCUS - MS));
        assert!(clocks.focus(t0 + LOOK_AT_FOCUS));
        assert!(!clocks.housekeeping(t0 + LOOK_AT_FOCUS));
        assert_eq!(phase.due(), Some(t0 + LOOK_AT_FOCUS * 2));
        let Phase::Live(clocks) = &mut phase else {
            unreachable!()
        };
        for i in 2..=3 {
            assert!(clocks.focus(t0 + LOOK_AT_FOCUS * i));
        }
        assert_eq!(phase.due(), Some(t0 + HOUSEKEEPING));
        // Another session (the same headset back, say) keeps the clocks.
        phase.session_opened(t0 + HOUSEKEEPING - MS);
        assert_eq!(phase.due(), Some(t0 + HOUSEKEEPING));
    }

    #[test]
    fn the_same_headset_replaces_its_session_another_is_refused() {
        let none: [u8; 0] = [];
        assert_eq!(admit(&none, &1), Admit::Open);
        assert_eq!(admit(&[1], &1), Admit::Replace);
        assert_eq!(admit(&[1], &2), Admit::Refuse);
    }

    #[test]
    fn the_earliest_deadline_wins() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_millis(500);
        let sooner = t0 + Duration::from_millis(150);
        assert_eq!(earliest([None, None]), None);
        assert_eq!(earliest([Some(later), None, Some(sooner)]), Some(sooner));
    }
}
