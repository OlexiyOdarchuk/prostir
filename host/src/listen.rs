//! The listening socket, and the setup of each connection on a thread of its own: a
//! headset pairing (with a dialog that waits for the person at the PC) or trickling
//! its bytes holds only its own thread, never the PC. A connection set up and trusted
//! goes to the control loop as [`Event::Accepted`].

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{debug, warn};

use crate::events::{Accepted, Event, EventSink};
use crate::pairing;
use crate::platform::tray;

/// Connections set up at once, at most: more are closed at once, so a peer opening
/// connection after connection cannot make the PC start threads without end.
const MAX_SETUPS: usize = 16;

/// One connection being set up; its place is free again when it is dropped.
struct Setup(Arc<AtomicUsize>);

impl Setup {
    /// A place, if one is free.
    fn take(setups: &Arc<AtomicUsize>) -> Option<Self> {
        let place = Self(setups.clone());
        // Over the limit, the place is given back as it is dropped.
        (setups.fetch_add(1, Ordering::AcqRel) < MAX_SETUPS).then_some(place)
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Accepts connections on `listener` for as long as the program runs.
pub fn spawn(
    listener: TcpListener,
    security: Arc<pairing::Security>,
    tray: Option<tray::Handle>,
    events: EventSink,
) -> Result<()> {
    std::thread::Builder::new()
        .name("listen".into())
        .spawn(move || {
            let setups = Arc::new(AtomicUsize::new(0));
            // When the log last said connections were closed for want of a place.
            let mut said_full: Option<Instant> = None;
            loop {
                let (stream, from) = match listener.accept() {
                    Ok(accepted) => accepted,
                    // Pending network errors (ENETDOWN, EHOSTUNREACH…) or running out
                    // of file descriptors: not a reason to stop listening.
                    Err(e) => {
                        warn!("accepting a connection failed: {e}");
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                };
                let Some(setup) = Setup::take(&setups) else {
                    if said_full.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                        said_full = Some(Instant::now());
                        warn!(
                            "{MAX_SETUPS} connections are being set up: closing the one from \
                             {from} (and others for a minute without saying so)"
                        );
                    }
                    continue;
                };
                let (security, tray, events) = (security.clone(), tray.clone(), events.clone());
                let spawned = std::thread::Builder::new()
                    .name("setup".into())
                    .spawn(move || {
                        let _setup = setup;
                        match pairing::accept(stream, &security, tray.as_ref()) {
                            Ok((conn, open)) => {
                                let accepted = Accepted { conn, open, from };
                                events.send(Event::Accepted(Box::new(accepted)));
                            }
                            Err(e) if e.is::<pairing::TurnedAwayAgain>() => {
                                debug!("rejected connection from {from}: {e:#}");
                            }
                            Err(e) => warn!("rejected connection from {from}: {e:#}"),
                        }
                    });
                if let Err(e) = spawned {
                    warn!("connection from {from} dropped: {e}");
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At most `MAX_SETUPS` places, each free again once its connection is done.
    #[test]
    fn connections_set_up_at_once_are_few() {
        let setups = Arc::new(AtomicUsize::new(0));
        let taken: Vec<Setup> = (0..MAX_SETUPS)
            .map(|_| Setup::take(&setups).expect("a free place"))
            .collect();
        assert!(Setup::take(&setups).is_none(), "one too many");
        drop(taken);
        assert_eq!(setups.load(Ordering::Acquire), 0);
        assert!(Setup::take(&setups).is_some());
    }
}
