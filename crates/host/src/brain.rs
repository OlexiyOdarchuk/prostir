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

use prostir_proto::state::{Access, perms};

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

/// How many headsets may have a session at once: the one rule that keeps it to a
/// single headset for now (step 11 of 0.2.0 raises it).
///
/// Everything else already works for more than one: each session has its own outbox
/// in the `Router` (a stream fans out to every headset, in a codec all of them decode,
/// and fails only for a headset too small for it), its own worn or not (the encoders
/// pause only once no headset is worn), its own rights; what only one headset can
/// have goes to one session at a time (see [`roles`]).
pub const MAX_HEADSETS: usize = 1;

/// How many headsets may have a session at once in this run: [`MAX_HEADSETS`], or in
/// a build for the end-to-end tests what `PROSTIR_TEST_MAX_HEADSETS` says (never a
/// setting of a release).
pub fn max_headsets() -> usize {
    #[cfg(feature = "fake-desktop")]
    if let Some(n) = std::env::var("PROSTIR_TEST_MAX_HEADSETS")
        .ok()
        .and_then(|n| n.parse().ok())
    {
        return n;
    }
    MAX_HEADSETS
}

/// Which sessions have the jobs only one headset can have at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roles<K> {
    /// The main session: its arrangement is the desktop's, its volume the PC's, its
    /// "No limits" the PC's limits. One whose `settings.txt` line makes it main;
    /// of several, the one that was main already, else the first connected. The
    /// others are guests while it lasts.
    pub main: Option<K>,
    /// The tray shows it and commands go to it: the main session, else the first.
    pub lead: Option<K>,
    /// Its microphone is the PC's end of the headset microphone: the main session's
    /// if it has the right, else the first connected with the right; while it is in
    /// use (a program records, or the headset streams it), it stays where it is.
    pub mic: Option<K>,
}

impl<K> Roles<K> {
    /// No session has any.
    pub const NONE: Self = Self {
        main: None,
        lead: None,
        mic: None,
    };
}

/// The [`Roles`] of `sessions` (the headset's key and its line's rights, in the order
/// the headsets first connected), where `was` are the roles before and `mic_busy`
/// whether the microphone is in use.
pub fn roles<K: Copy + PartialEq>(
    sessions: &[(K, Access)],
    was: Roles<K>,
    mic_busy: bool,
) -> Roles<K> {
    let mains = || sessions.iter().filter(|(_, a)| a.is_main());
    let main = mains()
        .find(|(k, _)| Some(*k) == was.main)
        .or_else(|| mains().next())
        .map(|(k, _)| *k);
    let lead = main.or_else(|| sessions.first().map(|(k, _)| *k));
    let may_mic = |k: &K| {
        sessions
            .iter()
            .any(|(key, a)| key == k && a.may(perms::MIC))
    };
    let kept = was.mic.filter(|k| mic_busy && may_mic(k));
    let mic = kept.or_else(|| main.filter(may_mic)).or_else(|| {
        sessions
            .iter()
            .find(|(_, a)| a.may(perms::MIC))
            .map(|(k, _)| *k)
    });
    Roles { main, lead, mic }
}

/// The order headsets first connected in, by key: one connecting again (its session
/// replaced) keeps its place, one that left loses it.
#[derive(Debug, Default)]
pub struct Arrivals<K>(Vec<K>);

impl<K: Copy + PartialEq> Arrivals<K> {
    /// Headset `key` has a session (again).
    pub fn connected(&mut self, key: K) {
        if !self.0.contains(&key) {
            self.0.push(key);
        }
    }

    /// Headset `key` has no session any more.
    pub fn left(&mut self, key: K) {
        self.0.retain(|k| *k != key);
    }

    /// The keys in the order they first connected.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.0.iter()
    }
}

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

/// Whether headset `key` may open a session while `live` have one, `max` at most.
pub fn admit<'a, K: PartialEq + 'a>(
    live: impl IntoIterator<Item = &'a K>,
    key: &K,
    max: usize,
) -> Admit {
    let mut count = 0;
    for k in live {
        if k == key {
            return Admit::Replace;
        }
        count += 1;
    }
    if count < max {
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
        assert_eq!(admit(&none, &1, MAX_HEADSETS), Admit::Open);
        assert_eq!(admit(&[1], &1, MAX_HEADSETS), Admit::Replace);
        assert_eq!(admit(&[1], &2, MAX_HEADSETS), Admit::Refuse);
        // With room for two, the second one opens; a third is refused.
        assert_eq!(admit(&[1], &2, 2), Admit::Open);
        assert_eq!(admit(&[1, 2], &2, 2), Admit::Replace);
        assert_eq!(admit(&[1, 2], &3, 2), Admit::Refuse);
    }

    const GUEST: Access = Access {
        role: prostir_proto::state::role::GUEST,
        perms: perms::INPUT,
    };
    const GUEST_MIC: Access = Access {
        role: prostir_proto::state::role::GUEST,
        perms: perms::MIC,
    };
    const MAIN_NO_MIC: Access = Access {
        role: prostir_proto::state::role::MAIN,
        perms: perms::CHANGES,
    };

    fn was(main: Option<u8>, mic: Option<u8>) -> Roles<u8> {
        Roles {
            main,
            lead: main,
            mic,
        }
    }

    /// One main session at a time: the first main one connected, kept while it lasts
    /// (a main headset that connected later does not take it over).
    #[test]
    fn one_session_is_the_main_one() {
        let none: [(u8, Access); 0] = [];
        assert_eq!(roles(&none, Roles::NONE, false), Roles::NONE);
        let all = [(1, GUEST), (2, Access::MAIN), (3, Access::MAIN)];
        let r = roles(&all, Roles::NONE, false);
        assert_eq!((r.main, r.lead, r.mic), (Some(2), Some(2), Some(2)));
        let r = roles(&all, was(Some(3), Some(3)), false);
        assert_eq!(r.main, Some(3), "the main one stays the main one");
        // It left: the next main one.
        let r = roles(
            &[(1, GUEST), (2, Access::MAIN)],
            was(Some(3), Some(3)),
            false,
        );
        assert_eq!(r.main, Some(2));
    }

    /// Without a main session the first is shown in the tray and takes the commands;
    /// the microphone is the main session's if it may, else the first's that may.
    #[test]
    fn the_tray_and_the_microphone_go_to_one_session() {
        let r = roles(&[(1, GUEST), (2, GUEST_MIC)], Roles::NONE, false);
        assert_eq!((r.main, r.lead, r.mic), (None, Some(1), Some(2)));
        let r = roles(&[(1, GUEST_MIC), (2, MAIN_NO_MIC)], Roles::NONE, false);
        assert_eq!((r.main, r.lead, r.mic), (Some(2), Some(2), Some(1)));
        let r = roles(&[(1, GUEST), (2, MAIN_NO_MIC)], Roles::NONE, false);
        assert_eq!(r.mic, None, "nobody may");
    }

    /// The microphone in use stays with its headset when a main one that may comes;
    /// once it is not in use, it goes to the main one.
    #[test]
    fn the_microphone_in_use_stays_where_it_is() {
        let both = [(1, GUEST_MIC), (2, Access::MAIN)];
        let r = roles(&both, was(None, Some(1)), true);
        assert_eq!((r.main, r.mic), (Some(2), Some(1)));
        let r = roles(&both, was(Some(2), Some(1)), false);
        assert_eq!(r.mic, Some(2));
        // Gone, or without the right: it moves all the same.
        let r = roles(&[(2, Access::MAIN)], was(Some(2), Some(1)), true);
        assert_eq!(r.mic, Some(2));
    }

    /// A headset connecting again keeps its place among the others; one that left
    /// comes back last.
    #[test]
    fn a_headset_back_keeps_its_place() {
        let mut arrivals = Arrivals::default();
        for key in [1, 2, 3] {
            arrivals.connected(key);
        }
        arrivals.connected(1);
        assert_eq!(arrivals.keys().copied().collect::<Vec<_>>(), [1, 2, 3]);
        arrivals.left(1);
        arrivals.connected(1);
        assert_eq!(arrivals.keys().copied().collect::<Vec<_>>(), [2, 3, 1]);
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
