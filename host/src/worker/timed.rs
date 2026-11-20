//! A call that may hang (a driver starting or stopping an encoder), made on a thread
//! of its own and waited for at most so long. One that runs out of time goes on
//! there; what it returns late is dropped there, so nothing it made stays.

use std::sync::mpsc;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

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
