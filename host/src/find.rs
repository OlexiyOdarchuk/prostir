//! Answering the headsets that ask for this PC (see `prostir_proto::discovery`), and
//! the PC's beacon until the headset asks instead (step 7 of 0.2.0).
//!
//! The PC answers every query while it has no paired headset or while its pairing
//! window is open, and otherwise only a query with its own tag. The same rules hold in
//! the full program (the `find` thread here), in the light phase it starts with under
//! systemd and in the sockets' watcher (`crate::wake`): [`Responder`] is all of them.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{debug, info, warn};
use prostir_proto::discovery::{Answer, FindSecret, MAX_QUERY, Query, flags};

use crate::events::{Event, EventSink};
use crate::pairing::Security;
use crate::platform;

/// Answers sent in a second, at most, whoever asks.
const ANSWERS_PER_SECOND: usize = 20;

/// Who may get an answer now, and the find secret the PC's own headsets make their
/// tags with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Open {
    /// The PC trusts at least one headset.
    pub paired: bool,
    /// The pairing window is open.
    pub pairing: bool,
    pub secret: FindSecret,
}

impl Open {
    /// Whether the PC answers queries without its tag.
    pub fn answers_all(self) -> bool {
        !self.paired || self.pairing
    }
}

/// A headset that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asker {
    pub from: SocketAddr,
    /// Its query carries this PC's tag: one of its own headsets (it has the PC's find
    /// secret).
    pub ours: bool,
    /// The name it gave (only while it looks for a PC to pair with).
    pub name: String,
    /// It got an answer.
    pub answered: bool,
}

/// What the PC answers with: its port and name, and a limit on how many answers go
/// out a second.
pub struct Responder {
    port: u16,
    name: String,
    /// When the last answers went, at most [`ANSWERS_PER_SECOND`] of them.
    sent: VecDeque<Instant>,
}

impl Responder {
    pub fn new(port: u16, name: String) -> Self {
        Self {
            port,
            name,
            sent: VecDeque::with_capacity(ANSWERS_PER_SECOND),
        }
    }

    /// Looks at `datagram` from `from` at `now`: `None` when it is no query (a beacon,
    /// too short or too long, broken); else who asked, and the answer to send back to
    /// `from` if it gets one.
    pub fn consider(
        &mut self,
        datagram: &[u8],
        from: SocketAddr,
        open: Open,
        now: Instant,
    ) -> Option<(Asker, Option<Vec<u8>>)> {
        let query = Query::decode(datagram)?;
        let ours = query.looks_for(&open.secret);
        let mut answer = None;
        if (ours || open.answers_all()) && self.may_send(now) {
            let mut bits = 0;
            if open.pairing {
                bits |= flags::PAIRING;
            }
            if open.answers_all() {
                bits |= flags::ANSWERS_ALL;
            }
            let bytes = Answer::to(&query, &open.secret, self.port, bits, &self.name).encode();
            debug_assert!(bytes.len() < datagram.len());
            answer = Some(bytes);
        }
        let asker = Asker {
            from,
            ours,
            name: query.name,
            answered: answer.is_some(),
        };
        Some((asker, answer))
    }

    /// Whether another answer may go at `now` (it is then counted).
    fn may_send(&mut self, now: Instant) -> bool {
        while self
            .sent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= Duration::from_secs(1))
        {
            self.sent.pop_front();
        }
        if self.sent.len() >= ANSWERS_PER_SECOND {
            return false;
        }
        self.sent.push_back(now);
        true
    }
}

/// Reads one datagram from `socket` (waiting for it unless the socket does not block)
/// and answers it: who asked, if it was a query.
pub fn answer_one(
    socket: &UdpSocket,
    responder: &mut Responder,
    open: impl FnOnce() -> Open,
) -> std::io::Result<Option<Asker>> {
    let mut buf = [0u8; MAX_QUERY + 1];
    let (n, from) = socket.recv_from(&mut buf)?;
    let Some((asker, answer)) = responder.consider(&buf[..n], from, open(), Instant::now()) else {
        return Ok(None);
    };
    if let Some(answer) = answer
        && let Err(e) = socket.send_to(&answer, from)
    {
        debug!("cannot answer {from}: {e}");
    }
    debug!(
        "discovery: {} headset at {from} asked{}",
        if asker.ours { "a paired" } else { "another" },
        if asker.answered { ", answered" } else { "" }
    );
    Ok(Some(asker))
}

/// Whether the pairing window is open, as the `find` thread reads it (the control loop
/// owns the window).
#[derive(Clone, Default)]
pub struct PairingOpen(Arc<AtomicBool>);

impl PairingOpen {
    pub fn set(&self, open: bool) {
        self.0.store(open, Ordering::Release);
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Answers the queries on `socket` on a thread of its own, asleep in the kernel until
/// one comes, for as long as the program runs. A headset that is not one of this PC's
/// goes to the control loop (`Event::Asked`), at most once a second for each address.
pub fn spawn(
    socket: UdpSocket,
    mut responder: Responder,
    security: Arc<Security>,
    pairing: PairingOpen,
    events: EventSink,
) -> Result<()> {
    info!(
        "answering headsets that ask for this PC: discovery on UDP port {}",
        socket.local_addr()?.port()
    );
    std::thread::Builder::new()
        .name("find".into())
        .spawn(move || {
            // The addresses told to the control loop lately, and when.
            let mut told: VecDeque<(SocketAddr, Instant)> = VecDeque::new();
            loop {
                let open = || Open {
                    paired: !security.trusted.lock().unwrap().is_empty(),
                    pairing: pairing.get(),
                    secret: security.find_secret(),
                };

                let asker = match answer_one(&socket, &mut responder, open) {
                    Ok(Some(asker)) => asker,
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        warn!("discovery stopped: {e}");
                        return;
                    }
                };
                if asker.ours {
                    continue;
                }
                let now = Instant::now();
                told.retain(|(_, at)| now.saturating_duration_since(*at) < Duration::from_secs(1));
                if told.iter().any(|(from, _)| *from == asker.from) || told.len() >= 16 {
                    continue;
                }
                told.push_back((asker.from, now));
                if !events.send(Event::Asked(asker)) {
                    return;
                }
            }
        })?;
    Ok(())
}

/// Sends the beacon every second on a thread of its own: headsets that do not ask yet
/// (before step 7 of 0.2.0) listen for it.
pub fn spawn_beacon(port: u16) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let beacon = prostir_proto::encode_beacon(port, &platform::hostname());
    // A build for the end-to-end tests sends it to the test only, never to the network.
    #[cfg(feature = "fake-desktop")]
    let to_test: Option<SocketAddr> = std::env::var("PROSTIR_TEST_BEACON_TO")
        .ok()
        .and_then(|a| a.parse().ok());
    #[cfg(not(feature = "fake-desktop"))]
    let to_test: Option<SocketAddr> = None;
    // It wakes the program once a second while it runs (not while it sleeps): until
    // the headset asks instead (step 7 of 0.2.0), the price of being found.
    std::thread::Builder::new()
        .name("beacon".into())
        .spawn(move || {
            loop {
                // 255.255.255.255 only leaves through the default route, so also send to
                // the broadcast address of every interface (Wi-Fi, Ethernet, hotspot).
                let targets: Vec<SocketAddr> = match to_test {
                    Some(to) => vec![to],
                    None => platform::broadcast_addresses()
                        .into_iter()
                        .chain([Ipv4Addr::BROADCAST])
                        .map(|ip| SocketAddr::from((ip, prostir_proto::DISCOVERY_PORT)))
                        .collect(),
                };
                for to in targets {
                    let _ = socket.send_to(&beacon, to);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prostir_proto::discovery::{MIN_QUERY, Query};

    const PC: [u8; 32] = [3; 32];
    const OTHER_PC: [u8; 32] = [4; 32];

    fn from() -> SocketAddr {
        "192.168.1.20:40000".parse().unwrap()
    }

    fn responder() -> Responder {
        Responder::new(47830, "desk".into())
    }

    const PAIRED: Open = Open {
        paired: true,
        pairing: false,
        secret: PC,
    };

    fn ask(r: &mut Responder, q: &Query, open: Open, now: Instant) -> (Asker, Option<Answer>) {
        let bytes = q.encode();
        let (asker, back) = r.consider(&bytes, from(), open, now).expect("a query");
        if let Some(b) = &back {
            assert!(
                b.len() < bytes.len(),
                "the answer is smaller than the query"
            );
        }
        (asker, back.map(|b| Answer::decode(&b).unwrap()))
    }

    /// Paired: only a query with this PC's tag gets an answer, and the answer says
    /// which PC it is.
    #[test]
    fn a_paired_pc_answers_its_own_headsets_only() {
        let mut r = responder();
        let now = Instant::now();
        let own = Query::new([&OTHER_PC, &PC], "");
        let (asker, answer) = ask(&mut r, &own, PAIRED, now);
        assert!(asker.ours && asker.answered);
        let answer = answer.unwrap();
        assert!(answer.from_pc(&PC));
        assert_eq!(
            (answer.port, answer.flags, answer.name.as_str()),
            (47830, 0, "desk")
        );
        let stranger = Query::new([&OTHER_PC], "Quest 3S");
        let (asker, answer) = ask(&mut r, &stranger, PAIRED, now);
        assert!(!asker.ours && !asker.answered);
        assert_eq!(asker.name, "Quest 3S");
        assert!(answer.is_none());
        let none = Query::new([], "");
        assert!(ask(&mut r, &none, PAIRED, now).1.is_none());
    }

    /// Without a paired headset, or while the pairing window is open, everyone gets
    /// an answer, which says so.
    #[test]
    fn unpaired_or_pairing_answers_everyone() {
        let mut r = responder();
        let now = Instant::now();
        let stranger = Query::new([&OTHER_PC], "");
        let unpaired = Open {
            paired: false,
            pairing: false,
            secret: PC,
        };
        let a = ask(&mut r, &stranger, unpaired, now).1.unwrap();
        assert_eq!(a.flags, flags::ANSWERS_ALL);
        let pairing = Open {
            paired: true,
            pairing: true,
            secret: PC,
        };
        let a = ask(&mut r, &stranger, pairing, now).1.unwrap();
        assert_eq!(a.flags, flags::ANSWERS_ALL | flags::PAIRING);
        // The window closed: silence again.
        assert!(ask(&mut r, &stranger, PAIRED, now).1.is_none());
    }

    /// A short query, a beacon or anything else is not read at all.
    #[test]
    fn what_is_no_query_is_not_answered() {
        let mut r = responder();
        let now = Instant::now();
        let open = Open {
            paired: false,
            pairing: true,
            secret: PC,
        };
        let bytes = Query::new([&PC], "").encode();
        assert!(
            r.consider(&bytes[..MIN_QUERY - 1], from(), open, now)
                .is_none()
        );
        let beacon = prostir_proto::encode_beacon(47830, "desk");
        assert!(r.consider(&beacon, from(), open, now).is_none());
        assert!(r.consider(&[0; 200], from(), open, now).is_none());
    }

    /// At most 20 answers a second in all, then again as the second moves on.
    #[test]
    fn answers_are_limited_to_twenty_a_second() {
        let mut r = responder();
        let start = Instant::now();
        let own = Query::new([&PC], "");
        let answered = |r: &mut Responder, at| ask(r, &own, PAIRED, at).1.is_some();
        for i in 0..20 {
            assert!(
                answered(&mut r, start + Duration::from_millis(i * 10)),
                "{i}"
            );
        }
        assert!(!answered(&mut r, start + Duration::from_millis(500)));
        assert!(!answered(&mut r, start + Duration::from_millis(999)));
        // The first one is a second old now: room for one.
        assert!(answered(&mut r, start + Duration::from_millis(1000)));
        assert!(!answered(&mut r, start + Duration::from_millis(1001)));
        assert!(answered(&mut r, start + Duration::from_millis(2500)));
    }
}
