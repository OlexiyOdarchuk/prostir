//! A call that may hang (a driver starting or stopping an encoder), made on a thread
//! of its own and waited for at most so long. One that runs out of time goes on
//! there; what it returns late is dropped there, so nothing it made stays.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use log::warn;

/// The call did not end in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedOut {
    pub what: String,
    pub after: Duration,
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} did not end within {:?}", self.what, self.after)
    }
}

impl std::error::Error for TimedOut {}

/// Runs `f` on a thread named `name` and waits for it at most `limit`.
pub fn call<T: Send + 'static>(
    name: &str,
    what: &str,
    limit: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, TimedOut> {
    let timed_out = || TimedOut {
        what: what.to_owned(),
        after: limit,
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            // Late, nobody waits: what it made is dropped here.
            let _ = tx.send(f());
        });
    if let Err(e) = spawned {
        warn!("no thread for {what}: {e}");
        return Err(timed_out());
    }
    rx.recv_timeout(limit).map_err(|_| {
        warn!("{what} did not end within {limit:?}: going on without it");
        timed_out()
    })
}

/// Calls left behind (see [`call_counted`]), by kind: how many still run, and how
/// many times each kind was left behind at all.
pub struct Behind<K> {
    counts: Mutex<(BTreeMap<K, u32>, BTreeMap<K, u32>)>,
}

impl<K> Default for Behind<K> {
    fn default() -> Self {
        Self {
            counts: Mutex::new((BTreeMap::new(), BTreeMap::new())),
        }
    }
}

impl<K: Ord + Copy> Behind<K> {
    /// Those of each kind left behind that still run.
    pub fn running(&self) -> BTreeMap<K, u32> {
        self.counts.lock().unwrap().0.clone()
    }

    /// All left behind that still run.
    pub fn total(&self) -> u32 {
        self.counts.lock().unwrap().0.values().sum()
    }

    /// How many times a call of `kind` was left behind.
    pub fn hangs(&self, kind: K) -> u32 {
        self.counts
            .lock()
            .unwrap()
            .1
            .get(&kind)
            .copied()
            .unwrap_or(0)
    }
}

/// As [`call`], counting a call of `kind` left behind in `behind` until it ends.
pub fn call_counted<K: Ord + Copy + Send + 'static, T: Send + 'static>(
    behind: &Arc<Behind<K>>,
    kind: K,
    name: &str,
    what: &str,
    limit: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, TimedOut> {
    // 0: under way, 1: left behind (counted), 2: ended.
    let state = Arc::new(Mutex::new(0u8));
    let (ended, counts) = (state.clone(), behind.clone());
    let result = call(name, what, limit, move || {
        let out = f();
        let mut state = ended.lock().unwrap();
        if *state == 1
            && let Some(n) = counts.counts.lock().unwrap().0.get_mut(&kind)
        {
            *n = n.saturating_sub(1);
        }
        *state = 2;
        out
    });
    if result.is_err() {
        let mut state = state.lock().unwrap();
        if *state == 0 {
            *state = 1;
            let mut counts = behind.counts.lock().unwrap();
            *counts.0.entry(kind).or_insert(0) += 1;
            *counts.1.entry(kind).or_insert(0) += 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    /// A call left behind counts while it runs, and the times its kind hung stay.
    #[test]
    fn calls_left_behind_are_counted_while_they_run() {
        let behind: Arc<Behind<u8>> = Arc::default();
        let quick = call_counted(&behind, 1, "t", "quick", Duration::from_secs(5), || 7);
        assert_eq!(quick, Ok(7));
        assert_eq!((behind.total(), behind.hangs(1)), (0, 0));
        let slow = call_counted(&behind, 1, "t", "slow", Duration::from_millis(30), || {
            std::thread::sleep(Duration::from_millis(300));
        });
        assert!(slow.is_err());
        assert_eq!(behind.running().get(&1), Some(&1));
        assert_eq!(
            (behind.total(), behind.hangs(1), behind.hangs(2)),
            (1, 1, 0)
        );
        let until = Instant::now() + Duration::from_secs(5);
        while behind.total() > 0 {
            assert!(Instant::now() < until, "it ended");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(behind.hangs(1), 1, "remembered");
    }

    #[test]
    fn a_call_in_time_gives_its_result() {
        assert_eq!(call("t", "adding", Duration::from_secs(5), || 2 + 2), Ok(4));
    }

    /// One that hangs is left behind after its time: the caller goes on, and what it
    /// returns late is dropped where it ran.
    #[test]
    fn a_call_that_hangs_is_left_behind() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let late = dropped.clone();
        let started = Instant::now();
        let result = call("t", "stopping", Duration::from_millis(50), move || {
            std::thread::sleep(Duration::from_millis(300));
            Dropped(late)
        });
        let e = result.err().expect("too late");
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(e.to_string(), "stopping did not end within 50ms");
        let until = Instant::now() + Duration::from_secs(5);
        while !dropped.load(Ordering::Relaxed) {
            assert!(Instant::now() < until, "what came late is dropped");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
