//! Monitors just added that the headset is to place: their place on the desktop is a
//! stand-in until the desktop was laid out with the headset's place for them, so the
//! local API shows them nowhere meanwhile (a map never shows one jump).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::screen::ScreenId;

/// How long a monitor just added waits for the headset to place it before its stand-in
/// place counts: the headset shows it first and sends an arrangement once it held
/// for 0.8 s.
pub const PLACE_WAIT: Duration = Duration::from_secs(4);

/// Where one monitor is on its way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Not in the headset's arrangement yet.
    Waiting,
    /// In it, not handed to the desktop's worker yet.
    Arranged,
    /// Handed over as the worker's `n`th thing: a layout of that or later puts it
    /// where it stays.
    Handed(u64),
}

#[derive(Debug, Clone, Copy)]
struct Placing {
    /// Its stand-in place counts from then on.
    until: Instant,
    stage: Stage,
}

/// The monitors being placed.
#[derive(Debug, Default)]
pub struct Placings(BTreeMap<ScreenId, Placing>);

impl Placings {
    /// Monitor `id` was added at `now`.
    pub fn add(&mut self, id: ScreenId, now: Instant) {
        self.0.insert(
            id,
            Placing {
                until: now + PLACE_WAIT,
                stage: Stage::Waiting,
            },
        );
    }

    pub fn remove(&mut self, id: ScreenId) {
        self.0.remove(&id);
    }

    /// No headset places them any more: their places count.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The headset's arrangement now places monitor `id` if `placed` says so.
    pub fn arranged(&mut self, placed: impl Fn(ScreenId) -> bool) {
        for (id, p) in &mut self.0 {
            if p.stage == Stage::Waiting && placed(*id) {
                p.stage = Stage::Arranged;
            }
        }
    }

    /// The wanted state with those arrangements went to the worker, whose count of
    /// things handed over is `posted` now.
    pub fn handed(&mut self, posted: u64) {
        for p in self.0.values_mut() {
            if p.stage == Stage::Arranged {
                p.stage = Stage::Handed(posted);
            }
        }
    }

    /// The desktop was laid out from what the worker took up to `upto`: the monitors
    /// handed over by then are where they stay. Those past their time go too.
    pub fn laid_out(&mut self, upto: u64, now: Instant) {
        self.0
            .retain(|_, p| now < p.until && !matches!(p.stage, Stage::Handed(n) if upto >= n));
    }

    /// Whether the desktop has monitor `id` where it stays.
    pub fn known(&self, id: ScreenId, now: Instant) -> bool {
        self.0.get(&id).is_none_or(|p| now >= p.until)
    }

    /// When the next stand-in place starts to count, if any is still to (one whose time
    /// passed is due no more: the control loop would wake at once, again and again).
    pub fn due(&self, now: Instant) -> Option<Instant> {
        self.0.values().map(|p| p.until).filter(|&u| u > now).min()
    }

    /// Forgets the monitors whose stand-in place counts by `now`.
    pub fn prune(&mut self, now: Instant) {
        self.0.retain(|_, p| now < p.until);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> ScreenId {
        ScreenId::saved(7)
    }

    #[test]
    fn known_once_laid_out_with_the_headsets_place() {
        let t0 = Instant::now();
        let mut p = Placings::default();
        p.add(id(), t0);
        assert!(!p.known(id(), t0));
        assert!(p.known(ScreenId::saved(8), t0), "another one");
        // Laid out before the headset placed it: still a stand-in.
        p.laid_out(10, t0);
        assert!(!p.known(id(), t0));
        // The arrangement came; a layout started before it was handed over ends.
        p.arranged(|other| other == id());
        p.laid_out(10, t0);
        assert!(!p.known(id(), t0));
        p.handed(12);
        p.laid_out(11, t0);
        assert!(!p.known(id(), t0), "laid out from what came before");
        p.laid_out(12, t0);
        assert!(p.known(id(), t0));
    }

    #[test]
    fn a_monitor_never_placed_counts_after_the_wait() {
        let t0 = Instant::now();
        let mut p = Placings::default();
        p.add(id(), t0);
        assert_eq!(p.due(t0), Some(t0 + PLACE_WAIT));
        assert!(!p.known(id(), t0 + PLACE_WAIT - Duration::from_millis(1)));
        assert!(p.known(id(), t0 + PLACE_WAIT));
        // Its time passed without a layout: nothing is due any more, before or after
        // it is forgotten.
        assert_eq!(p.due(t0 + PLACE_WAIT), None);
        assert_eq!(p.due(t0 + PLACE_WAIT * 2), None);
        p.prune(t0 + PLACE_WAIT);
        assert_eq!(p.due(t0), None);
    }

    #[test]
    fn without_a_headset_every_place_counts() {
        let t0 = Instant::now();
        let mut p = Placings::default();
        p.add(id(), t0);
        p.clear();
        assert!(p.known(id(), t0));
        assert_eq!(p.due(t0), None);
    }
}
