//! Two linked ends of a transport inside one process, for tests: what one sends, the
//! other receives, in order. Each direction holds at most a given number of records,
//! so a side that stops reading makes the other one wait, as a stalled network does.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{Closer, Link, PeerInfo, RecordReceiver, RecordSender, Via};

/// A send waits at most this long for room, like a TCP write timeout.
const WRITE_LIMIT: Duration = Duration::from_secs(5);

/// Two linked ends; each direction holds up to `capacity` records not yet received.
pub fn pair(capacity: usize) -> (Link, Link) {
    let ab = Arc::new(Pipe::new(capacity));
    let ba = Arc::new(Pipe::new(capacity));
    (end(ab.clone(), ba.clone()), end(ba, ab))
}

fn end(out: Arc<Pipe>, inbound: Arc<Pipe>) -> Link {
    let closer = {
        let (out, inbound) = (out.clone(), inbound.clone());
        // As a socket shut down both ways: the peer still reads what was sent before.
        Closer::new(move || {
            out.update(|s| s.writer_closed = true);
            inbound.update(|s| s.reader_closed = true);
        })
    };
    Link {
        sender: Box::new(Sender(out)),
        receiver: Box::new(Receiver {
            pipe: inbound,
            current: Vec::new(),
            idle: None,
        }),
        closer,
        peer: PeerInfo {
            key: [0; 32],
            binding: Vec::new(),
            via: Via::Network,
        },
    }
}

struct Pipe {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    records: VecDeque<Vec<u8>>,
    capacity: usize,
    /// No more records come: the reader gets the end once it took the rest.
    writer_closed: bool,
    /// The reader is gone: it gets the end at once, and sends fail.
    reader_closed: bool,
}

impl Pipe {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                records: VecDeque::new(),
                capacity: capacity.max(1),
                writer_closed: false,
                reader_closed: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn update(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
        self.changed.notify_all();
    }
}

struct Sender(Arc<Pipe>);

impl RecordSender for Sender {
    fn send(&mut self, parts: &[&[u8]]) -> io::Result<()> {
        let record = parts.concat();
        if record.len() > super::MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "record too large",
            ));
        }
        let until = Instant::now() + WRITE_LIMIT;
        let mut s = self.0.state.lock().unwrap();
        loop {
            if s.writer_closed || s.reader_closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if s.records.len() < s.capacity {
                s.records.push_back(record);
                self.0.changed.notify_all();
                return Ok(());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            s = self.0.changed.wait_timeout(s, left).unwrap().0;
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        self.0.update(|s| s.writer_closed = true);
        Ok(())
    }

    fn set_pace(&mut self, _bytes_per_sec: u64) {}
}

struct Receiver {
    pipe: Arc<Pipe>,
    current: Vec<u8>,
    idle: Option<Duration>,
}

impl RecordReceiver for Receiver {
    fn recv(&mut self) -> io::Result<&[u8]> {
        let until = self.idle.map(|idle| Instant::now() + idle);
        let mut s = self.pipe.state.lock().unwrap();
        loop {
            if s.reader_closed {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if let Some(record) = s.records.pop_front() {
                self.pipe.changed.notify_all();
                drop(s);
                self.current = record;
                return Ok(&self.current);
            }
            if s.writer_closed {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            s = match until {
                None => self.pipe.changed.wait(s).unwrap(),
                Some(until) => {
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    self.pipe.changed.wait_timeout(s, left).unwrap().0
                }
            };
        }
    }

    fn set_idle_limit(&mut self, idle: Option<Duration>) {
        self.idle = idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_arrive_in_order_and_a_full_pipe_waits() {
        let (mut a, mut b) = pair(2);
        a.sender.send(&[b"one"]).unwrap();
        a.sender.send(&[b"tw", b"o"]).unwrap();
        // Full: the third send waits until the other side reads.
        let sender = std::thread::spawn(move || {
            a.sender.send(&[b"three"]).unwrap();
            a
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!sender.is_finished());
        assert_eq!(b.receiver.recv().unwrap(), b"one");
        let a = sender.join().unwrap();
        assert_eq!(b.receiver.recv().unwrap(), b"two");
        assert_eq!(b.receiver.recv().unwrap(), b"three");
        b.receiver.set_idle_limit(Some(Duration::from_millis(20)));
        assert_eq!(
            b.receiver.recv().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        // Closed by the sending side: the end, after what was sent.
        drop(a.sender);
        a.closer.close();
        assert_eq!(
            b.receiver.recv().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    /// Closing wakes a send blocked on a full pipe and a receive blocked on an empty one.
    #[test]
    fn closing_wakes_blocked_calls() {
        let (mut a, mut b) = pair(1);
        a.sender.send(&[b"x"]).unwrap();
        let closer = a.closer.clone();
        let blocked_send = std::thread::spawn(move || a.sender.send(&[b"y"]));
        let blocked_recv = std::thread::spawn(move || a.receiver.recv().map(<[u8]>::to_vec));
        std::thread::sleep(Duration::from_millis(50));
        closer.close();
        assert!(blocked_send.join().unwrap().is_err());
        assert!(blocked_recv.join().unwrap().is_err());
        assert_eq!(b.receiver.recv().unwrap(), b"x");
        assert!(b.sender.send(&[b"z"]).is_err(), "nobody reads any more");
    }
}
