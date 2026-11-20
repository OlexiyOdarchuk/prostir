//! The listening socket, and the setup of each connection on a thread of its own: a
//! headset pairing (with a dialog that waits for the person at the PC) or trickling
//! its bytes holds only its own thread, never the PC. A connection set up and trusted
//! goes to the control loop as [`Event::Accepted`].

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use log::{debug, warn};

use crate::events::{Accepted, Event, EventSink};
use crate::pairing;
use crate::platform::tray;

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
                let (security, tray, events) = (security.clone(), tray.clone(), events.clone());
                let spawned = std::thread::Builder::new()
                    .name("setup".into())
                    .spawn(
                        move || match pairing::accept(stream, &security, tray.as_ref()) {
                            Ok((conn, open)) => {
                                let accepted = Accepted { conn, open, from };
                                events.send(Event::Accepted(Box::new(accepted)));
                            }
                            Err(e) if e.is::<pairing::TurnedAwayAgain>() => {
                                debug!("rejected connection from {from}: {e:#}");
                            }
                            Err(e) => warn!("rejected connection from {from}: {e:#}"),
                        },
                    );
                if let Err(e) = spawned {
                    warn!("connection from {from} dropped: {e}");
                }
            }
        })?;
    Ok(())
}
