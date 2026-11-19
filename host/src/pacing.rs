//! How fast the connection to the headset may send: a little under what the network
//! is measured to deliver.
//!
//! Unpaced, TCP pushes a whole keyframe into the Wi-Fi access point's queue at once,
//! and sound waits behind it there, where nothing on the PC can overtake it. Paced at
//! about 90 % of what the link carries, the queue stays short, and a link faster than
//! the video loses nothing.
//!
//! The measure is TCP's delivery rate, taken while the network held the PC back (the
//! writer waited on the socket most of the time: a keyframe going out, say); a rate
//! taken while the PC had little to send says how much it sent, not what the link
//! carries. TCP's own "app-limited" mark does not tell this here: `TCP_NOTSENT_LOWAT`
//! keeps little unsent data in the kernel on purpose, so TCP marks nearly every
//! sample. Each such stretch (a burst) gives one measure, the upper quarter of what
//! it delivered, once it is over (or after half a second of it): its start, TCP still speeding up, does
//! not count alone, and a burst too short to fill the link does not count at all.
//!
//! Paced, a burst can show only the pace itself. Then the estimate grows a little, to
//! find out whether the link carries more; a burst clearly under the pace brings it
//! down to what the link delivered.
//!
//! Whatever the estimate says, the pace never goes under what the streams need: half
//! as much again as what was queued for the headset over the last 2 s, and at least
//! 20 Mbit/s. A link slower than the video is for the bitrate to adapt to (0.2.0 step
//! 10), not for pacing to starve the video.
//!
//! This is a rule of thumb until the link is estimated properly (step 10): the
//! measures come in through [`Pacer::sample`], and the pace from the estimate and the
//! floor is [`pace_for`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use prostir_proto::link::Via;

/// What is paced at: a share of the estimate, in tenths.
const SHARE_TENTHS: u64 = 9;
/// A rate at least this share (in hundredths) of the pace only shows the pace.
const PACE_LIMITED: u64 = 98;
/// After a burst that showed only the pace, the estimate grows by this share (in
/// hundredths).
const PROBE: u64 = 5;
/// A new pace that differs less than this share (in hundredths) is not worth setting.
const CHANGE: u64 = 3;
/// A burst moving less than this says little about the link.
const MIN_BURST: usize = 256 * 1024;
/// A burst counts after this many samples (a writer looking every 20 ms: half a
/// second), and another one starts: a link that is always full is still measured.
const MAX_BURST_SAMPLES: u32 = 25;
/// The floor: this many tenths of what the streams need.
const FLOOR_TENTHS: u64 = 15;
/// Never paced slower (20 Mbit/s).
const MIN_PACE: u64 = 20_000_000 / 8;
/// What the streams need is what was queued for the headset over about this long.
const DEMAND_WINDOW: Duration = Duration::from_secs(2);

/// The pace for a link that carries `estimate` bytes a second, when the streams need
/// `floor`.
pub fn pace_for(estimate: u64, floor: u64) -> u64 {
    (estimate / 10 * SHARE_TENTHS).max(floor)
}

/// What the streams need with room to spare, when `demand` bytes a second are queued
/// for the headset.
pub fn floor_for(demand: u64) -> u64 {
    (demand / 10 * FLOOR_TENTHS).max(MIN_PACE)
}

/// What was queued for the headset lately, in slices of a quarter of the window.
#[derive(Default)]
struct Demand {
    slices: VecDeque<(Instant, u64)>,
}

impl Demand {
    fn add(&mut self, now: Instant, bytes: usize) {
        while self
            .slices
            .front()
            .is_some_and(|&(at, _)| now.saturating_duration_since(at) > DEMAND_WINDOW)
        {
            self.slices.pop_front();
        }
        match self.slices.back_mut() {
            Some((at, b)) if now.saturating_duration_since(*at) < DEMAND_WINDOW / 4 => {
                *b += bytes as u64
            }
            _ => self.slices.push_back((now, bytes as u64)),
        }
    }

    /// Bytes a second, over the time the slices cover (at least a quarter of the
    /// window, so the first bytes do not read as a flood).
    fn rate(&self, now: Instant) -> u64 {
        let Some(&(first, _)) = self.slices.front() else {
            return 0;
        };
        let covered = now.saturating_duration_since(first).max(DEMAND_WINDOW / 4);
        let bytes: u64 = self.slices.iter().map(|&(_, b)| b).sum();
        (bytes as f64 / covered.as_secs_f64()) as u64
    }
}

/// A stretch of sending that the network held back.
#[derive(Default)]
struct Burst {
    /// The rates it delivered, bytes a second.
    rates: [u64; MAX_BURST_SAMPLES as usize],
    samples: u32,
    /// What it sent.
    bytes: usize,
    /// It delivered the pace: the link may carry more.
    paced: bool,
}

impl Burst {
    /// What it delivered: the upper quarter of its rates. Its first rates still
    /// show the quiet before it, and a single rate can read well over what the link
    /// carries, when acknowledgements come in clumps.
    fn delivered(&mut self) -> u64 {
        let rates = &mut self.rates[..self.samples as usize];
        rates.sort_unstable();
        rates[rates.len() * 3 / 4]
    }
}

/// Turns delivery rates into a pace for one connection.
pub struct Pacer {
    /// Over USB nothing queues the way Wi-Fi does: no pacing.
    usb: bool,
    /// The burst going on.
    burst: Option<Burst>,
    /// What the link is thought to carry, bytes a second; `None` until measured.
    estimate: Option<u64>,
    demand: Demand,
    /// What the streams need with room to spare: the pace never goes under it.
    floor: u64,
    /// The pace set on the connection (0: none yet).
    pace: u64,
}

impl Pacer {
    pub fn new(via: Via) -> Self {
        Self {
            usb: via == Via::Usb,
            burst: None,
            estimate: None,
            demand: Demand::default(),
            floor: MIN_PACE,
            pace: 0,
        }
    }

    /// What the streams need with room to spare, bytes a second.
    pub fn floor(&self) -> u64 {
        self.floor
    }

    /// What the link is thought to carry, bytes a second.
    pub fn estimate(&self) -> Option<u64> {
        self.estimate
    }

    /// Takes a delivery rate at `now`; `held_back`: the network held the PC back
    /// since the last one, while it sent `sent` bytes; `queued` bytes were queued for
    /// the headset meanwhile. The new pace when it should change.
    pub fn sample(
        &mut self,
        now: Instant,
        bytes_per_sec: u64,
        held_back: bool,
        sent: usize,
        queued: usize,
    ) -> Option<u64> {
        self.demand.add(now, queued);
        self.floor = floor_for(self.demand.rate(now));
        if self.usb || bytes_per_sec == 0 {
            return None;
        }
        self.measure(bytes_per_sec, held_back, sent);
        let pace = pace_for(self.estimate?, self.floor);
        let changed = self.pace == 0 || pace.abs_diff(self.pace) > self.pace / 100 * CHANGE;
        changed.then(|| {
            self.pace = pace;
            pace
        })
    }

    /// The estimate from a burst, once it is over.
    fn measure(&mut self, bytes_per_sec: u64, held_back: bool, sent: usize) {
        if held_back {
            let b = self.burst.get_or_insert_with(Burst::default);
            b.rates[b.samples as usize] = bytes_per_sec;
            b.samples += 1;
            b.bytes += sent;
            b.paced |= self.pace > 0 && bytes_per_sec >= self.pace / 100 * PACE_LIMITED;
            if b.samples < MAX_BURST_SAMPLES {
                return;
            }
        }
        // A burst ended (or went on long enough): what it shows.
        let Some(mut burst) = self.burst.take().filter(|b| b.bytes >= MIN_BURST) else {
            return;
        };
        self.estimate = Some(match self.estimate {
            // It showed only the pace: perhaps the link carries more.
            Some(e) if burst.paced => e + e / 100 * PROBE,
            // What the link delivered.
            _ => burst.delivered(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBIT: u64 = 1_000_000 / 8;
    /// What a packet carries.
    const PACKET: usize = 16 * 1024;
    /// How often the writer looks.
    const LOOK: Duration = Duration::from_millis(20);

    /// A pacer looked at every 20 ms, with nothing queued unless a test says.
    struct Clocked {
        p: Pacer,
        now: Instant,
    }

    impl Clocked {
        fn new(via: Via) -> Self {
            Self {
                p: Pacer::new(via),
                now: Instant::now(),
            }
        }

        fn sample(&mut self, bytes_per_sec: u64, held_back: bool, sent: usize) -> Option<u64> {
            self.queued(bytes_per_sec, held_back, sent, 0)
        }

        fn queued(
            &mut self,
            bytes_per_sec: u64,
            held_back: bool,
            sent: usize,
            queued: usize,
        ) -> Option<u64> {
            self.now += LOOK;
            self.p
                .sample(self.now, bytes_per_sec, held_back, sent, queued)
        }

        fn estimate(&self) -> Option<u64> {
            self.p.estimate()
        }
    }

    /// A burst delivering `rates` (Mbit/s), a few packets a sample, and what the end
    /// of it gives.
    fn burst(p: &mut Clocked, rates: &[u64]) -> Option<u64> {
        let mut paces = None;
        for &r in rates {
            paces = paces.or(p.sample(r * MBIT, true, 8 * PACKET));
        }
        paces.or(p.sample(MBIT, false, 0))
    }

    #[test]
    fn the_pace_is_nine_tenths_of_the_link_never_under_the_floor() {
        assert_eq!(pace_for(100 * MBIT, MIN_PACE), 90 * MBIT);
        assert_eq!(pace_for(10 * MBIT, 30 * MBIT), 30 * MBIT, "the floor");
        assert_eq!(floor_for(0), 20 * MBIT, "never under 20 Mbit/s");
        assert_eq!(floor_for(60 * MBIT), 90 * MBIT);
    }

    /// The owner's Wi-Fi after a reconnect, 2026-11-19: bursts delivered 143, 85,
    /// then 17 Mbit/s, and the pace fell to 20 while the monitors needed about 55.
    /// It stays at half as much again as what they queue.
    #[test]
    fn a_collapse_does_not_starve_the_video() {
        let mut p = Clocked::new(Via::Network);
        let need = 55 * MBIT * LOOK.as_millis() as u64 / 1000;
        let mut paces = Vec::new();
        for rate in [143, 85, 17, 17, 28] {
            for _ in 0..30 {
                paces.extend(p.queued(rate * MBIT, true, 8 * PACKET, need as usize));
            }
            paces.extend(p.queued(MBIT, false, 0, need as usize));
        }
        assert!(p.estimate() < Some(30 * MBIT), "the link looked that slow");
        let last = &paces[1..];
        assert!(
            last.iter().all(|&pace| pace >= 80 * MBIT),
            "{:?}",
            paces.iter().map(|p| p / MBIT).collect::<Vec<_>>()
        );
    }

    /// Nothing is paced before a whole burst was measured (TCP speeds up during the
    /// first one), nor from a burst too short to fill the link, nor from rates taken
    /// while the PC had little to send, nor ever over USB.
    #[test]
    fn a_pace_only_from_a_whole_burst() {
        let mut p = Clocked::new(Via::Network);
        assert_eq!(p.sample(2 * MBIT, false, PACKET), None, "quiet: no measure");
        assert_eq!(p.sample(2 * MBIT, true, PACKET), None);
        assert_eq!(p.sample(MBIT, false, 0), None, "too short to say");
        assert_eq!(p.sample(5 * MBIT, true, 10 * PACKET), None);
        assert_eq!(p.sample(60 * MBIT, true, 10 * PACKET), None);
        assert_eq!(p.estimate(), None);
        assert_eq!(
            p.sample(MBIT, false, 0),
            Some(pace_for(60 * MBIT, MIN_PACE))
        );
        assert_eq!(p.estimate(), Some(60 * MBIT));
        // Quiet again: nothing changes.
        assert_eq!(p.sample(MBIT, false, PACKET), None);
        let mut usb = Clocked::new(Via::Usb);
        assert_eq!(burst(&mut usb, &[300, 300]), None);
    }

    /// A link that stays full is measured every half a second.
    #[test]
    fn a_long_burst_is_measured_on_the_way() {
        let mut p = Clocked::new(Via::Network);
        for _ in 1..MAX_BURST_SAMPLES {
            assert_eq!(p.sample(80 * MBIT, true, 4 * PACKET), None);
        }
        assert_eq!(
            p.sample(80 * MBIT, true, 4 * PACKET),
            Some(pace_for(80 * MBIT, MIN_PACE))
        );
    }

    /// Paced, bursts show the pace: the estimate grows a little after each, until a
    /// burst shows less than the pace; then it is what the link delivered. The pace
    /// stays near the link, never far over it.
    #[test]
    fn the_estimate_finds_the_link_and_follows_it() {
        let mut p = Clocked::new(Via::Network);
        // A first burst still speeding up: the link (400) looks slower.
        assert_eq!(burst(&mut p, &[50, 100]), Some(90 * MBIT));
        let link = 400 * MBIT;
        let mut paces = Vec::new();
        for _ in 0..60 {
            let delivered = p.p.pace.min(link) / MBIT;
            burst(&mut p, &[delivered, delivered]);
            paces.push(p.p.pace / MBIT);
        }
        let last = &paces[40..];
        assert!(last.iter().all(|&m| (350..=450).contains(&m)), "{last:?}");
        // The link slows down to 50: the next burst brings the pace down.
        assert_eq!(burst(&mut p, &[50, 50]), Some(45 * MBIT));
    }
}
