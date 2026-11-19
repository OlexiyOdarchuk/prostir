//! Encrypted, authenticated connections between the PC and the headset.
//!
//! * Every device has a long-term Curve25519 key ([`Identity`]).
//! * Each connection starts with a Noise `XX` handshake (mutual, encrypted); after it
//!   the byte stream is encrypted with AES-256-GCM ([`SecureReader`], [`SecureWriter`]).
//! * Devices trust each other's keys after pairing once ([`TrustStore`]): one side shows a
//!   6-digit code, the user types it on the other side, and SPAKE2 turns it into a shared
//!   key without exposing it to offline guessing. A confirmation tag bound to this
//!   connection's handshake hash proves both sides used the same code over the same
//!   channel, which defeats a man in the middle.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use snow::StatelessTransportState;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

/// AES-GCM rather than ChaChaPoly: both CPUs have hardware AES (AES-NI, ARMv8 crypto),
/// while ChaCha20 and Poly1305 run in plain software on the headset.
const NOISE: &str = "Noise_XX_25519_AESGCM_BLAKE2s";
const MAX_NOISE: usize = 65535;
const TAG: usize = 16;
/// Largest plaintext per encrypted chunk.
const CHUNK: usize = MAX_NOISE - TAG;

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// A device's long-term key pair.
pub struct Identity {
    private: [u8; 32],
    pub public: [u8; 32],
}

impl Identity {
    /// Loads the key pair from `path` (`private public`, hex), creating it — readable
    /// by the owner only — on first use.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Some((private, public)) = text.trim().split_once(' ')
            && let (Some(private), Some(public)) = (from_hex(private), from_hex(public))
        {
            return Ok(Self { private, public });
        }
        let keypair = snow::Builder::new(NOISE.parse().map_err(err)?)
            .generate_keypair()
            .map_err(err)?;
        let private: [u8; 32] = keypair.private.as_slice().try_into().map_err(err)?;
        let public: [u8; 32] = keypair.public.as_slice().try_into().map_err(err)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_private(path, &format!("{} {}\n", to_hex(&private), to_hex(&public)))?;
        Ok(Self { private, public })
    }
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    write_atomic(path, text.as_bytes())
}

/// Replaces a file in one step: written next to it, synced, then renamed over it. A
/// crash or a full disk mid-write leaves the old file, never a cut-off one. The file
/// is readable by its owner only (keys, trusted devices, settings).
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Short human-readable form of a key, for logs and lists.
pub fn fingerprint(key: &[u8; 32]) -> String {
    to_hex(&key[..6])
}

/// Peer keys this device has paired with, one `hexkey label` per line.
pub struct TrustStore {
    path: PathBuf,
    entries: Vec<([u8; 32], String)>,
}

impl TrustStore {
    pub fn load(path: &Path) -> Self {
        let entries = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (key, label) = l.split_once(' ').unwrap_or((l, ""));
                Some((from_hex(key.trim())?, label.trim().to_owned()))
            })
            .collect();
        Self {
            path: path.to_owned(),
            entries,
        }
    }

    pub fn contains(&self, key: &[u8; 32]) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    pub fn label(&self, key: &[u8; 32]) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, l)| l.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn add(&mut self, key: [u8; 32], label: &str) -> io::Result<()> {
        self.entries.retain(|(k, _)| *k != key);
        let label = label.replace(['\n', '\r'], " ");
        self.entries.push((key, label));
        self.save()
    }

    /// Every trusted key with its label, oldest first.
    pub fn entries(&self) -> &[([u8; 32], String)] {
        &self.entries
    }

    pub fn remove(&mut self, key: &[u8; 32]) -> io::Result<()> {
        self.entries.retain(|(k, _)| k != key);
        self.save()
    }

    pub fn clear(&mut self) -> io::Result<()> {
        self.entries.clear();
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text: String = self
            .entries
            .iter()
            .map(|(k, l)| format!("{} {l}\n", to_hex(k)))
            .collect();
        write_atomic(&self.path, text.as_bytes())
    }
}

/// What the handshake established about the other side.
pub struct Peer {
    /// The peer's long-term key.
    pub key: [u8; 32],
    /// Unique to this connection; pairing confirmations are bound to it.
    pub handshake_hash: Vec<u8>,
}

/// Runs the Noise XX handshake. `initiator` is the headset (the side that connects).
pub fn handshake(
    stream: &TcpStream,
    identity: &Identity,
    initiator: bool,
) -> io::Result<(SecureReader, SecureWriter, Peer)> {
    let builder = snow::Builder::new(NOISE.parse().map_err(err)?)
        .local_private_key(&identity.private)
        .map_err(err)?;
    let mut hs = if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(err)?;
    let mut s = stream;
    let mut buf = vec![0u8; MAX_NOISE];
    let mut payload = vec![0u8; MAX_NOISE];
    // XX: -> e ; <- e, ee, s, es ; -> s, se
    for step in 0..3 {
        let writing = (step % 2 == 0) == initiator;
        if writing {
            let n = hs.write_message(&[], &mut buf).map_err(err)?;
            write_frame(&mut s, &buf[..n])?;
        } else {
            let msg = read_frame(&mut s)?;
            hs.read_message(&msg, &mut payload).map_err(err)?;
        }
    }
    let key: [u8; 32] = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| err("peer sent no static key"))?;
    let handshake_hash = hs.get_handshake_hash().to_vec();
    let transport = Arc::new(hs.into_stateless_transport_mode().map_err(err)?);
    let reader = SecureReader {
        inner: stream.try_clone()?,
        transport: transport.clone(),
        nonce: 0,
        plain: Vec::new(),
        pos: 0,
    };
    let writer = SecureWriter {
        inner: stream.try_clone()?,
        transport,
        nonce: 0,
        buf: vec![0u8; MAX_NOISE],
    };
    Ok((
        reader,
        writer,
        Peer {
            key,
            handshake_hash,
        },
    ))
}

fn write_frame(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    let len = u16::try_from(data.len()).map_err(err)?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(data)
}

fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len)?;
    let mut data = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut data)?;
    Ok(data)
}

/// Decrypting half of a connection (usable from its own thread).
pub struct SecureReader {
    inner: TcpStream,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
    plain: Vec<u8>,
    pos: usize,
}

impl Read for SecureReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // An empty chunk carries nothing: read on, so `Ok(0)` only ever means the
        // stream ended.
        while self.pos >= self.plain.len() {
            let mut len = [0u8; 2];
            // A clean end of stream between chunks is a normal close.
            match self.inner.read(&mut len[..1])? {
                0 => return Ok(0),
                _ => self.inner.read_exact(&mut len[1..])?,
            }
            let mut cipher = vec![0u8; u16::from_be_bytes(len) as usize];
            self.inner.read_exact(&mut cipher)?;
            self.plain.resize(MAX_NOISE, 0);
            let n = self
                .transport
                .read_message(self.nonce, &cipher, &mut self.plain)
                .map_err(|_| err("decryption failed (tampered or out-of-order data)"))?;
            self.nonce += 1;
            self.plain.truncate(n);
            self.pos = 0;
        }
        let n = out.len().min(self.plain.len() - self.pos);
        out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Encrypting half of a connection (usable from its own thread).
pub struct SecureWriter {
    inner: TcpStream,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
    buf: Vec<u8>,
}

impl Write for SecureWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let chunk = &data[..data.len().min(CHUNK)];
        let n = self
            .transport
            .write_message(self.nonce, chunk, &mut self.buf)
            .map_err(err)?;
        self.nonce += 1;
        // One write per chunk: header and ciphertext leave together.
        let mut frame = Vec::with_capacity(n + 2);
        frame.extend_from_slice(&(n as u16).to_be_bytes());
        frame.extend_from_slice(&self.buf[..n]);
        self.inner.write_all(&frame)?;
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Bytes of a SPAKE2 message (Ed25519 group, symmetric: a marker byte and a point).
pub const PAIR_MSG_LEN: usize = 33;
/// Bytes of a pairing confirmation tag (HMAC-SHA256).
pub const CONFIRM_LEN: usize = 32;

/// A random 6-digit pairing code.
pub fn pairing_code() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("system randomness");
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}

/// One side of a SPAKE2 exchange keyed by the pairing code.
pub struct Pairing(Spake2<Ed25519Group>);

impl Pairing {
    /// Starts pairing with `code`; send the returned message to the other side.
    pub fn start(code: &str) -> (Self, [u8; PAIR_MSG_LEN]) {
        let digits: String = code.chars().filter(char::is_ascii_digit).collect();
        let (state, msg) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(digits.as_bytes()),
            &SpakeId::new(b"prostir pairing v1"),
        );
        let msg = msg
            .try_into()
            .expect("an Ed25519 SPAKE2 message is 33 bytes");
        (Self(state), msg)
    }

    /// Key shared with the other side if both used the same code (otherwise the keys
    /// differ and the confirmation tags will not match).
    pub fn finish(self, peer_msg: &[u8]) -> io::Result<Vec<u8>> {
        self.0
            .finish(peer_msg)
            .map_err(|_| err("bad pairing message"))
    }
}

/// Tag proving knowledge of the pairing key, bound to this connection and to `role`.
pub fn confirmation(key: &[u8], role: &str, peer: &Peer, own_key: &[u8; 32]) -> [u8; CONFIRM_LEN] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    mac.update(b"prostir confirm v1");
    mac.update(role.as_bytes());
    mac.update(&peer.handshake_hash);
    // Both long-term keys, in a fixed (sorted) order so both sides compute the same.
    let (a, b) = if own_key <= &peer.key {
        (own_key, &peer.key)
    } else {
        (&peer.key, own_key)
    };
    mac.update(a);
    mac.update(b);
    mac.finalize().into_bytes().into()
}

/// Constant-time tag comparison.
pub fn tags_match(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn identity(tag: &str) -> Identity {
        let dir = std::env::temp_dir().join(format!("prostir-test-{}-{tag}", std::process::id()));
        let id = Identity::load_or_create(&dir.join("key")).unwrap();
        // Loading again gives the same key.
        assert_eq!(
            Identity::load_or_create(&dir.join("key")).unwrap().public,
            id.public
        );
        std::fs::remove_dir_all(dir).unwrap();
        id
    }

    fn connect() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (c, s)
    }

    #[test]
    fn encrypted_stream_and_pairing() {
        let (headset, pc) = (identity("h"), identity("p"));
        let pc_public = pc.public;
        let (cs, ss) = connect();
        let server = std::thread::spawn(move || {
            let (mut r, mut w, peer) = handshake(&ss, &pc, false).unwrap();
            let mut big = vec![0u8; 200_000];
            r.read_exact(&mut big).unwrap();
            assert!(big.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
            w.write_all(b"pong").unwrap();
            // Pairing, PC side.
            let (p, msg) = Pairing::start("123 456");
            write_frame(&mut w, &msg).unwrap();
            let theirs = read_frame(&mut r).unwrap();
            let key = p.finish(&theirs).unwrap();
            let tag = read_frame(&mut r).unwrap();
            assert!(tags_match(
                &tag,
                &confirmation(&key, "headset", &peer, &pc.public)
            ));
            write_frame(&mut w, &confirmation(&key, "pc", &peer, &pc.public)).unwrap();
            peer.key
        });
        let (mut r, mut w, peer) = handshake(&cs, &headset, true).unwrap();
        let big: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        w.write_all(&big).unwrap();
        let mut pong = [0u8; 4];
        r.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"pong");
        let (p, msg) = Pairing::start("123456");
        let theirs = read_frame(&mut r).unwrap();
        write_frame(&mut w, &msg).unwrap();
        let key = p.finish(&theirs).unwrap();
        write_frame(
            &mut w,
            &confirmation(&key, "headset", &peer, &headset.public),
        )
        .unwrap();
        let tag = read_frame(&mut r).unwrap();
        assert!(tags_match(
            &tag,
            &confirmation(&key, "pc", &peer, &headset.public)
        ));
        assert_eq!(server.join().unwrap(), headset.public);
        assert_eq!(peer.key, pc_public);
    }

    #[test]
    fn wrong_code_does_not_confirm() {
        let (a, b) = (identity("a"), identity("b"));
        let b_public = b.public;
        let (cs, ss) = connect();
        let t = std::thread::spawn(move || handshake(&ss, &b, false).map(|x| x.2).unwrap());
        let peer_a = handshake(&cs, &a, true).unwrap().2;
        let peer_b = t.join().unwrap();
        let (pa, ma) = Pairing::start("111111");
        let (pb, mb) = Pairing::start("222222");
        let ka = pa.finish(&mb).unwrap();
        let kb = pb.finish(&ma).unwrap();
        assert!(!tags_match(
            &confirmation(&ka, "headset", &peer_a, &a.public),
            &confirmation(&kb, "headset", &peer_b, &b_public)
        ));
    }

    #[test]
    fn trust_store_roundtrip() {
        let path = std::env::temp_dir().join(format!("prostir-trust-{}", std::process::id()));
        let mut t = TrustStore::load(&path);
        assert!(t.is_empty());
        t.add([7; 32], "Quest 3").unwrap();
        let t2 = TrustStore::load(&path);
        assert!(t2.contains(&[7; 32]) && !t2.contains(&[8; 32]));
        assert_eq!(t2.label(&[7; 32]), Some("Quest 3"));
        std::fs::remove_file(path).unwrap();
        let code = pairing_code();
        assert_eq!(code.len(), 6);
    }
}
