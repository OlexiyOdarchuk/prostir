//! The transport over one TCP connection (Wi-Fi, or USB through `adb reverse`),
//! encrypted with Noise:
//!
//! ```text
//! Noise_XX_25519_AESGCM_BLAKE2s
//! ```
//!
//! * `XX`: mutual authentication; both long-term keys travel encrypted. The headset
//!   (the side that connects) is the initiator. No prologue, no pre-shared key, empty
//!   handshake payloads.
//! * Every handshake message and every record goes as a 2-byte big-endian length, then
//!   that many bytes: a Noise message, or a record's ciphertext with its 16-byte tag.
//!   Nonces are counters, one per direction, from 0; there is no rekeying.
//!
//! The socket belongs to the transport: the sender, the receiver and the [`Closer`]
//! each hold a handle to it. Each half keeps its buffers for the whole connection, so
//! sending or receiving a record allocates nothing.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use snow::StatelessTransportState;

use super::{Closer, Link, MAX_RECORD, PeerInfo, RecordReceiver, RecordSender, Via};
use crate::secure::Identity;

/// AES-GCM rather than ChaChaPoly: both CPUs have hardware AES (AES-NI, ARMv8 crypto),
/// while ChaCha20 and Poly1305 run in plain software on the headset.
pub(crate) const NOISE: &str = "Noise_XX_25519_AESGCM_BLAKE2s";
/// The largest Noise message.
const MAX_NOISE: usize = 65535;
/// A write the network does not take within this ends the connection.
pub const WRITE_LIMIT: Duration = Duration::from_secs(5);

/// Socket settings that need system calls this crate does not make (it has no unsafe
/// code): keepalive, `TCP_USER_TIMEOUT`, `TCP_NOTSENT_LOWAT`, pacing. Each app's
/// platform provides them; [`Plain`] makes none.
pub trait SocketOptions: Send + Sync {
    /// Once, on the connected socket, before the handshake.
    fn prepare(&self, stream: &TcpStream);

    /// Sends at most `bytes_per_sec` (0: no limit), spreading packets out evenly.
    fn set_pace(&self, stream: &TcpStream, bytes_per_sec: u64);
}

/// No settings beyond what the transport makes itself (`TCP_NODELAY`, timeouts).
pub struct Plain;

impl SocketOptions for Plain {
    fn prepare(&self, _stream: &TcpStream) {}

    fn set_pace(&self, _stream: &TcpStream, _bytes_per_sec: u64) {}
}

/// Opens Noise-encrypted links over TCP.
pub struct NoiseTcp;

impl NoiseTcp {
    /// The headset's side: `stream` is connected to the PC. The handshake waits as
    /// long as the stream's read timeout allows.
    pub fn connect(
        stream: TcpStream,
        identity: &Identity,
        options: Arc<dyn SocketOptions>,
    ) -> io::Result<Link> {
        open(stream, identity, options, true)
    }

    /// The PC's side: `stream` is a connection it accepted.
    pub fn accept(
        stream: TcpStream,
        identity: &Identity,
        options: Arc<dyn SocketOptions>,
    ) -> io::Result<Link> {
        open(stream, identity, options, false)
    }
}

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// A socket timeout reads as `WouldBlock` on Unix: say what it is.
fn timeouts(e: io::Error) -> io::Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => io::Error::new(
            io::ErrorKind::TimedOut,
            "nothing came from the other side in time",
        ),
        _ => e,
    }
}

fn open(
    stream: TcpStream,
    identity: &Identity,
    options: Arc<dyn SocketOptions>,
    initiator: bool,
) -> io::Result<Link> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(WRITE_LIMIT))?;
    options.prepare(&stream);
    let via = match stream.peer_addr()?.ip().is_loopback() {
        true => Via::Usb,
        false => Via::Network,
    };
    let builder = snow::Builder::new(NOISE.parse().map_err(err)?)
        .local_private_key(&identity.private)
        .map_err(err)?;
    let mut hs = if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(err)?;
    let mut buf = vec![0u8; MAX_NOISE];
    let mut payload = vec![0u8; MAX_NOISE];
    let mut s = &stream;
    // XX: -> e ; <- e, ee, s, es ; -> s, se
    for step in 0..3 {
        let writing = (step % 2 == 0) == initiator;
        if writing {
            let n = hs.write_message(&[], &mut buf).map_err(err)?;
            let len = u16::try_from(n).map_err(err)?;
            let mut frame = len.to_be_bytes().to_vec();
            frame.extend_from_slice(&buf[..n]);
            s.write_all(&frame).map_err(timeouts)?;
        } else {
            let mut len = [0u8; 2];
            s.read_exact(&mut len).map_err(timeouts)?;
            let len = usize::from(u16::from_be_bytes(len));
            s.read_exact(&mut buf[..len]).map_err(timeouts)?;
            hs.read_message(&buf[..len], &mut payload).map_err(err)?;
        }
    }
    let key: [u8; 32] = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| err("peer sent no static key"))?;
    let binding = hs.get_handshake_hash().to_vec();
    let transport = Arc::new(hs.into_stateless_transport_mode().map_err(err)?);
    let closer = {
        let stream = stream.try_clone()?;
        Closer::new(move || {
            let _ = stream.shutdown(Shutdown::Both);
        })
    };
    Ok(Link {
        sender: Box::new(TcpSender {
            stream: stream.try_clone()?,
            transport: transport.clone(),
            nonce: 0,
            plain: vec![0; MAX_RECORD].into_boxed_slice(),
            record: vec![0; 2 + MAX_NOISE].into_boxed_slice(),
            options,
            pace: 0,
        }),
        receiver: Box::new(TcpReceiver {
            stream,
            transport,
            nonce: 0,
            record: vec![0; 2 + MAX_NOISE].into_boxed_slice(),
            got: 0,
            plain: vec![0; MAX_NOISE].into_boxed_slice(),
        }),
        closer,
        peer: PeerInfo { key, binding, via },
    })
}

struct TcpSender {
    stream: TcpStream,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
    /// The record before encryption.
    plain: Box<[u8]>,
    /// Its length and ciphertext, as they go on the wire.
    record: Box<[u8]>,
    options: Arc<dyn SocketOptions>,
    pace: u64,
}

impl RecordSender for TcpSender {
    fn send(&mut self, parts: &[&[u8]]) -> io::Result<()> {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if len > MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "record too large",
            ));
        }
        let mut at = 0;
        for p in parts {
            self.plain[at..at + p.len()].copy_from_slice(p);
            at += p.len();
        }
        let n = self
            .transport
            .write_message(self.nonce, &self.plain[..len], &mut self.record[2..])
            .map_err(err)?;
        self.nonce += 1;
        self.record[..2].copy_from_slice(&(n as u16).to_be_bytes());
        // One write: length and ciphertext leave together.
        (&self.stream)
            .write_all(&self.record[..2 + n])
            .map_err(timeouts)
    }

    fn finish(&mut self) -> io::Result<()> {
        self.stream.shutdown(Shutdown::Write)
    }

    fn set_pace(&mut self, bytes_per_sec: u64) {
        if bytes_per_sec != self.pace {
            self.pace = bytes_per_sec;
            self.options.set_pace(&self.stream, bytes_per_sec);
        }
    }
}

struct TcpReceiver {
    stream: TcpStream,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
    /// The record coming in: its length, then its ciphertext.
    record: Box<[u8]>,
    /// How much of it came so far: a read that times out goes on where it stopped.
    got: usize,
    plain: Box<[u8]>,
}

impl TcpReceiver {
    /// Reads until `self.record[..want]` is there.
    fn fill(&mut self, want: usize) -> io::Result<()> {
        while self.got < want {
            match (&self.stream).read(&mut self.record[self.got..want]) {
                Ok(0) if self.got == 0 => {
                    // A clean end between records is a normal close.
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the other side closed the connection",
                    ));
                }
                Ok(0) => return Err(err("the connection closed in the middle of a record")),
                Ok(n) => self.got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(timeouts(e)),
            }
        }
        Ok(())
    }
}

impl RecordReceiver for TcpReceiver {
    fn recv(&mut self) -> io::Result<&[u8]> {
        self.fill(2)?;
        let len = 2 + usize::from(u16::from_be_bytes([self.record[0], self.record[1]]));
        self.fill(len)?;
        self.got = 0;
        let n = self
            .transport
            .read_message(self.nonce, &self.record[2..len], &mut self.plain)
            .map_err(|_| err("decryption failed (tampered or out-of-order data)"))?;
        self.nonce += 1;
        Ok(&self.plain[..n])
    }

    fn set_idle_limit(&mut self, idle: Option<Duration>) {
        // Zero means "for ever" to the socket, and is no limit anyone means.
        let idle = idle.map(|d| d.max(Duration::from_millis(1)));
        let _ = self.stream.set_read_timeout(idle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn identity(tag: &str) -> Identity {
        let dir = std::env::temp_dir().join(format!("prostir-tcp-{}-{tag}", std::process::id()));
        let id = Identity::load_or_create(&dir.join("key")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        id
    }

    /// A headset's link and a PC's, connected over loopback.
    fn linked(tag: &str) -> (Link, Link, [u8; 32], [u8; 32]) {
        let (headset, pc) = (identity(&format!("{tag}-h")), identity(&format!("{tag}-p")));
        let keys = (headset.public, pc.public);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            NoiseTcp::connect(TcpStream::connect(addr).unwrap(), &headset, Arc::new(Plain)).unwrap()
        });
        let (s, _) = l.accept().unwrap();
        let pc_link = NoiseTcp::accept(s, &pc, Arc::new(Plain)).unwrap();
        (h.join().unwrap(), pc_link, keys.0, keys.1)
    }

    #[test]
    fn records_go_both_ways_encrypted() {
        let (mut h, mut p, hkey, pkey) = linked("both");
        assert_eq!(h.peer.key, pkey);
        assert_eq!(p.peer.key, hkey);
        assert_eq!(h.peer.binding, p.peer.binding, "the same connection");
        assert_eq!((h.peer.via, p.peer.via), (Via::Usb, Via::Usb));
        let big: Vec<u8> = (0..MAX_RECORD).map(|i| (i % 251) as u8).collect();
        h.sender.send(&[&big[..10], &big[10..]]).unwrap();
        h.sender.send(&[]).unwrap();
        assert_eq!(p.receiver.recv().unwrap(), big);
        assert_eq!(p.receiver.recv().unwrap(), b"");
        p.sender.send(&[b"pong"]).unwrap();
        assert_eq!(h.receiver.recv().unwrap(), b"pong");
        // Done sending: the other side reads the end, and can still answer.
        p.sender.finish().unwrap();
        assert_eq!(
            h.receiver.recv().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        h.sender.send(&[b"bye"]).unwrap();
        assert_eq!(p.receiver.recv().unwrap(), b"bye");
        // One byte more than a record holds is refused, not cut.
        let err = h.sender.send(&[&big, b"x"]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // Silence, then a clean close.
        p.receiver.set_idle_limit(Some(Duration::from_millis(20)));
        assert_eq!(
            p.receiver.recv().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        h.closer.close();
        assert_eq!(
            p.receiver.recv().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    /// A record that stops coming half way is read on after the timeout.
    #[test]
    fn a_receive_that_timed_out_goes_on() {
        let (headset, pc) = (identity("resume-h"), identity("resume-p"));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = TcpListener::bind("127.0.0.1:0").unwrap();
        let (pc_addr, relay_addr) = (l.local_addr().unwrap(), relay.local_addr().unwrap());
        // Between the two: passes everything on, but holds the record after the
        // handshake (its first bytes past the three handshake messages) half way.
        std::thread::spawn(move || {
            let (mut from, _) = relay.accept().unwrap();
            let mut to = TcpStream::connect(pc_addr).unwrap();
            let back = (from.try_clone().unwrap(), to.try_clone().unwrap());
            std::thread::spawn(move || {
                let (mut from, mut to) = (back.1, back.0);
                let _ = io::copy(&mut from, &mut to);
            });
            let mut buf = [0u8; 4096];
            let mut handshakes = 0;
            while handshakes < 2 {
                let mut len = [0u8; 2];
                from.read_exact(&mut len).unwrap();
                let n = usize::from(u16::from_be_bytes(len));
                from.read_exact(&mut buf[..n]).unwrap();
                to.write_all(&len).unwrap();
                to.write_all(&buf[..n]).unwrap();
                handshakes += 1;
            }
            let n = from.read(&mut buf).unwrap();
            to.write_all(&buf[..3]).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            to.write_all(&buf[3..n]).unwrap();
            let _ = io::copy(&mut from, &mut to);
        });
        let h = std::thread::spawn(move || {
            let mut link = NoiseTcp::connect(
                TcpStream::connect(relay_addr).unwrap(),
                &headset,
                Arc::new(Plain),
            )
            .unwrap();
            link.sender.send(&[b"held half way"]).unwrap();
            link
        });
        let (s, _) = l.accept().unwrap();
        let mut p = NoiseTcp::accept(s, &pc, Arc::new(Plain)).unwrap();
        let _h = h.join().unwrap();
        p.receiver.set_idle_limit(Some(Duration::from_millis(30)));
        assert_eq!(
            p.receiver.recv().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        p.receiver.set_idle_limit(Some(Duration::from_secs(5)));
        assert_eq!(p.receiver.recv().unwrap(), b"held half way");
    }

    /// The closer wakes a receive that waits for ever.
    #[test]
    fn closing_wakes_a_blocked_receive() {
        let (mut h, _p, _, _) = linked("wake");
        let closer = h.closer.clone();
        let blocked = std::thread::spawn(move || h.receiver.recv().map(<[u8]>::to_vec));
        std::thread::sleep(Duration::from_millis(50));
        closer.close();
        assert!(blocked.join().unwrap().is_err());
    }
}
