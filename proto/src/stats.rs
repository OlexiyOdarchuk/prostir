//! Small, cheap statistics for the periodic log lines on both sides: how long things
//! wait (a histogram with percentiles), pauses between events, and when to report.
//!
//! Nothing here allocates after creation or reads the clock by itself: callers pass
//! `now`, so the types are tested without waiting.

use std::time::{Duration, Instant};

/// Upper bounds of the histogram buckets, in microseconds. Fine below a few
/// milliseconds (queue waits are often sub-millisecond), coarse above 100 ms.
const BOUNDS_US: [u64; 20] = [
    100, 250, 500, 1_000, 2_000, 3_000, 5_000, 7_500, 10_000, 15_000, 20_000, 30_000, 50_000,
    75_000, 100_000, 150_000, 250_000, 500_000, 1_000_000, 2_000_000,
];

/// Durations sorted into fixed buckets. Percentiles are the upper bound of the bucket
/// they fall in, never more than the largest value seen (which is kept exactly).
#[derive(Debug, Clone, Default)]
pub struct Histogram {
    /// One count per bound, plus one for everything above the last bound.
    buckets: [u32; BOUNDS_US.len() + 1],
    count: u32,
    sum_us: u64,
    max_us: u64,
}

impl Histogram {
    pub fn add(&mut self, d: Duration) {
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        let i = BOUNDS_US.partition_point(|&b| b < us);
        self.buckets[i] = self.buckets[i].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    pub fn max(&self) -> Duration {
        Duration::from_micros(self.max_us)
    }

    pub fn mean(&self) -> Duration {
        Duration::from_micros(self.sum_us / u64::from(self.count.max(1)))
    }

    /// The value below which a share `p` (0..=1) of the samples lie; zero when empty.
    pub fn percentile(&self, p: f64) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        // The rank of the sample asked for, 1-based.
        let rank = ((p.clamp(0.0, 1.0) * f64::from(self.count)).ceil() as u32).max(1);
        let mut seen = 0;
        for (i, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                let bound = BOUNDS_US.get(i).copied().unwrap_or(u64::MAX);
                return Duration::from_micros(bound.min(self.max_us));
            }
        }
        self.max()
    }

    /// `p50 0.3 p95 2.0 max 15.1 ms`, for logs; `none` without samples.
    pub fn summary_ms(&self) -> String {
        if self.count == 0 {
            return "none".into();
        }
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        format!(
            "p50 {:.1} p95 {:.1} max {:.1} ms",
            ms(self.percentile(0.5)),
            ms(self.percentile(0.95)),
            ms(self.max())
        )
    }
}

/// The pauses counted by [`Gaps`]: longer than each of these.
pub const GAP_LIMITS: [Duration; 4] = [
    Duration::from_millis(30),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(250),
];

/// Pauses between events (messages arriving, say): how many were longer than each of
/// [`GAP_LIMITS`], and the longest.
#[derive(Debug, Clone, Default)]
pub struct Gaps {
    last: Option<Instant>,
    over: [u32; GAP_LIMITS.len()],
    longest: Duration,
    events: u32,
}

impl Gaps {
    pub fn event(&mut self, now: Instant) {
        self.events = self.events.saturating_add(1);
        let Some(last) = self.last.replace(now) else {
            return;
        };
        let gap = now.saturating_duration_since(last);
        self.longest = self.longest.max(gap);
        for (limit, n) in GAP_LIMITS.iter().zip(&mut self.over) {
            if gap > *limit {
                *n += 1;
            }
        }
    }

    /// How many pauses were longer than each of [`GAP_LIMITS`].
    pub fn over(&self) -> [u32; GAP_LIMITS.len()] {
        self.over
    }

    pub fn longest(&self) -> Duration {
        self.longest
    }

    pub fn events(&self) -> u32 {
        self.events
    }

    /// Starts counting anew, measuring the next pause from the last event.
    pub fn restart(&mut self) {
        *self = Self {
            last: self.last,
            ..Self::default()
        };
    }

    /// `over 30/50/100/250 ms: 3/1/0/0 (18/6/0/0 per min), longest 62 ms` for a
    /// count taken over `window`.
    pub fn summary(&self, window: Duration) -> String {
        let join =
            |v: [u32; GAP_LIMITS.len()]| v.iter().map(u32::to_string).collect::<Vec<_>>().join("/");
        let per_min = self.over.map(|n| per_minute(n, window).round() as u32);
        format!(
            "over 30/50/100/250 ms: {} ({} per min), longest {} ms",
            join(self.over),
            join(per_min),
            self.longest.as_millis()
        )
    }
}

/// `n` events in `window`, as a rate per minute.
pub fn per_minute(n: u32, window: Duration) -> f64 {
    f64::from(n) * 60.0 / window.as_secs_f64().max(1e-3)
}

/// Says when a periodic report is due.
#[derive(Debug, Clone)]
pub struct Period {
    every: Duration,
    since: Option<Instant>,
}

impl Period {
    pub const fn new(every: Duration) -> Self {
        Self { every, since: None }
    }

    /// The time covered when a report is due (and a new period starts), else `None`.
    /// The first call starts the first period.
    pub fn due(&mut self, now: Instant) -> Option<Duration> {
        let since = *self.since.get_or_insert(now);
        let covered = now.saturating_duration_since(since);
        if covered < self.every {
            return None;
        }
        self.since = Some(now);
        Some(covered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn percentiles_come_from_the_buckets() {
        let mut h = Histogram::default();
        assert_eq!(h.percentile(0.5), Duration::ZERO);
        assert_eq!(h.summary_ms(), "none");
        for _ in 0..90 {
            h.add(Duration::from_micros(400));
        }
        for _ in 0..9 {
            h.add(ms(12));
        }
        h.add(ms(40));
        assert_eq!(h.count(), 100);
        // 400 µs is in the bucket up to 500 µs.
        assert_eq!(h.percentile(0.5), Duration::from_micros(500));
        assert_eq!(h.percentile(0.95), ms(15));
        assert_eq!(h.percentile(1.0), ms(40));
        assert_eq!(h.max(), ms(40));
        assert_eq!(h.summary_ms(), "p50 0.5 p95 15.0 max 40.0 ms");
    }

    #[test]
    fn a_percentile_is_never_above_the_max() {
        let mut h = Histogram::default();
        h.add(ms(11));
        assert_eq!(h.percentile(0.5), ms(11));
        // Beyond the last bucket.
        h.add(Duration::from_secs(9));
        assert_eq!(h.percentile(1.0), Duration::from_secs(9));
        assert_eq!(h.mean(), Duration::from_micros(4_505_500));
    }

    #[test]
    fn gaps_are_counted_per_limit() {
        let t0 = Instant::now();
        let mut g = Gaps::default();
        for at in [0, 10, 50, 60, 160, 460, 470] {
            g.event(t0 + ms(at));
        }
        // Pauses: 10, 40, 10, 100, 300, 10.
        assert_eq!(g.over(), [3, 2, 1, 1]);
        assert_eq!(g.longest(), ms(300));
        assert_eq!(g.events(), 7);
        assert_eq!(
            g.summary(Duration::from_secs(30)),
            "over 30/50/100/250 ms: 3/2/1/1 (6/4/2/2 per min), longest 300 ms"
        );
        // The next pause is measured from the last event even after a restart.
        g.restart();
        g.event(t0 + ms(530));
        assert_eq!(g.over(), [1, 1, 0, 0]);
    }

    #[test]
    fn a_period_reports_what_it_covered() {
        let t0 = Instant::now();
        let mut p = Period::new(Duration::from_secs(10));
        assert_eq!(p.due(t0), None);
        assert_eq!(p.due(t0 + Duration::from_secs(9)), None);
        assert_eq!(
            p.due(t0 + Duration::from_secs(11)),
            Some(Duration::from_secs(11))
        );
        assert_eq!(p.due(t0 + Duration::from_secs(12)), None);
        assert_eq!(per_minute(3, Duration::from_secs(30)), 6.0);
    }
}
