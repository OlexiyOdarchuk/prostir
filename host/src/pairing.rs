//! Encrypted connections and pairing on the PC side.

use crate::i18n::t;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use prostir_proto::link::tcp::NoiseTcp;
use prostir_proto::link::{Closer, Link, PeerInfo, StreamReader, StreamWriter};
use prostir_proto::secure::{self, Identity, Pairing, TrustStore, confirmation, tags_match};
use prostir_proto::{ClientMsg, HostMsg, PROTOCOL_VERSION};

use crate::platform::dialogs::{ask_code, notify};
use crate::platform::{NoCode, tray};

/// Wrong codes allowed per connection before giving up.
const MAX_ATTEMPTS: u32 = 3;
/// Wrong codes allowed in a row (across connections) before pairing slows down.
const FREE_FAILURES: u32 = 3;
/// A code made on the PC stays the same for this long, however many attempts use it.
const CODE_LIFETIME: Duration = Duration::from_secs(300);
/// Encryption, hello and opening the session must be done within this time, however
/// slowly bytes trickle.
const SETUP_TIME: Duration = Duration::from_secs(10);
/// Time to read and type a code.
const PAIRING_TIME: Duration = Duration::from_secs(300);
/// Before the session, each message from the headset comes within this.
const READ_TIME: Duration = Duration::from_secs(5);

/// Wrong pairing codes across all connections. After a few, every attempt waits
/// longer (30 s, 1 min, 2 min… up to 15 min), so guessing the 6-digit code from the
/// network would take years instead of minutes.
struct Limiter {
    failures: u32,
    not_before: Option<Instant>,
    warned: bool,
}

static LIMITER: Mutex<Limiter> = Mutex::new(Limiter {
    failures: 0,
    not_before: None,
    warned: false,
});

impl Limiter {
    /// How long until the next attempt is allowed.
    fn wait(&self) -> Option<Duration> {
        self.not_before
            .and_then(|t| t.checked_duration_since(Instant::now()))
            .filter(|d| !d.is_zero())
    }

    fn failed(&mut self) {
        self.failures += 1;
        if self.failures > FREE_FAILURES {
            let step = (self.failures - FREE_FAILURES - 1).min(5);
            let wait = Duration::from_secs((30u64 << step).min(900));
            self.not_before = Some(Instant::now() + wait);
            if !self.warned {
                self.warned = true;
                notify(
                    t("Неправильні коди сполучення", "Wrong pairing codes"),
                    t(
                        "Хтось кілька разів ввів неправильний код. Нові спроби тепер чекають.",
                        "Someone typed wrong codes several times. New attempts now have to wait.",
                    ),
                );
            }
        }
    }

    fn succeeded(&mut self) {
        self.failures = 0;
        self.not_before = None;
        self.warned = false;
    }
}

struct ClearTrayCode<'a>(Option<&'a tray::Handle>);

impl Drop for ClearTrayCode<'_> {
    fn drop(&mut self) {
        tray::set_pairing(self.0, None);
    }
}

/// The code shown on the PC: one per pairing window, not one per attempt.
static PC_CODE: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Closes a connection that is not through some stage in time: per-read timeouts
/// alone let a peer that sends a byte every few seconds hold the PC forever.
struct Deadline {
    state: Arc<(Mutex<Instant>, std::sync::Condvar)>,
    done: Arc<AtomicBool>,
}

impl Deadline {
    fn new(closer: &Closer, after: Duration) -> Result<Self> {
        let state = Arc::new((
            Mutex::new(Instant::now() + after),
            std::sync::Condvar::new(),
        ));
        let done = Arc::new(AtomicBool::new(false));
        let watched = closer.clone();
        let (st, dn) = (state.clone(), done.clone());
        std::thread::Builder::new()
            .name("deadline".into())
            .spawn(move || {
                let (lock, cv) = &*st;
                let mut at = lock.lock().unwrap();
                while !dn.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    if now >= *at {
                        warn!("connection setup took too long: closing it");
                        watched.close();
                        return;
                    }
                    let left = *at - now;
                    at = cv.wait_timeout(at, left).unwrap().0;
                }
            })?;
        Ok(Self { state, done })
    }

    fn extend(&self, after: Duration) {
        *self.state.0.lock().unwrap() = Instant::now() + after;
        self.state.1.notify_all();
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        self.state.1.notify_all();
    }
}

pub fn config_dir() -> PathBuf {
    crate::platform::config_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("prostir")
}

pub struct Security {
    pub identity: Identity,
    pub trusted: TrustStore,
}

impl Security {
    pub fn load() -> Result<Self> {
        let dir = config_dir();
        let identity = Identity::load_or_create(&dir.join("identity"))
            .context("cannot create the PC's key")?;
        info!("PC key {}", secure::fingerprint(&identity.public));
        Ok(Self {
            identity,
            trusted: TrustStore::load(&dir.join("paired_headsets")),
        })
    }
}

/// Encrypts the connection, checks the versions, pairs the headset if either side asks,
/// and returns the link once the headset is trusted and opened the session.
pub fn accept(
    stream: TcpStream,
    security: &mut Security,
    tray: Option<&tray::Handle>,
) -> Result<Link> {
    // A vanished headset does not hold the PC (the transport also gives up a write
    // after a few seconds, and the deadline closes a setup that drags on).
    stream.set_read_timeout(Some(READ_TIME))?;
    let mut link = NoiseTcp::accept(
        stream,
        &security.identity,
        Arc::new(crate::platform::TcpOptions),
    )?;
    let deadline = Deadline::new(&link.closer, SETUP_TIME)?;
    // However this connection ends, the tray stops showing a code for it.
    let _clear_code = ClearTrayCode(tray);
    let peer = link.peer.clone();
    let mut reader = StreamReader::new(&mut *link.receiver);
    let mut writer = StreamWriter(&mut *link.sender);
    // Versions first, before pairing: this exchange is the same in every version, so
    // a headset of any version learns which side to update.
    match ClientMsg::read_first(&mut reader)? {
        Some(ClientMsg::Hello { version, min, app })
            if !prostir_proto::compatible(version, min) =>
        {
            let app = peer_version(&app);
            let first = incompatible(&mut writer, min > PROTOCOL_VERSION, &app)?;
            close_gently(writer, reader);
            return Err(turned_away(
                first,
                format!("headset speaks protocol v{version} (app {app}), PC v{PROTOCOL_VERSION}"),
            ));
        }
        Some(ClientMsg::Hello { .. }) => {}
        // Only headsets from before protocol 2 (0.1) start with anything else: they
        // could ask to pair before saying hello.
        _ => {
            let first = incompatible(&mut writer, false, "0.1.x")?;
            close_gently(writer, reader);
            return Err(turned_away(
                first,
                format!("headset from before protocol {PROTOCOL_VERSION}: no hello first"),
            ));
        }
    }
    HostMsg::Welcome {
        version: PROTOCOL_VERSION,
        language: crate::i18n::code().into(),
    }
    .write(&mut writer)?;
    writer.flush()?;
    let mut attempts = 0;
    loop {
        match ClientMsg::read(&mut reader)? {
            ClientMsg::Open if security.trusted.contains(&peer.key) => {
                HostMsg::Opened.write(&mut writer)?;
                writer.flush()?;
                reader.receiver().set_idle_limit(None);
                drop(deadline);
                return Ok(link);
            }
            ClientMsg::Open => {
                info!(
                    "unknown headset {}: pairing needed",
                    secure::fingerprint(&peer.key)
                );
                HostMsg::PairingRequired.write(&mut writer)?;
                writer.flush()?;
            }
            ClientMsg::PairBegin {
                headset_shows,
                name,
            } => {
                attempts += 1;
                if attempts > MAX_ATTEMPTS {
                    bail!("too many wrong pairing codes");
                }
                let wait = LIMITER.lock().unwrap().wait();
                if let Some(wait) = wait {
                    let minutes = wait.as_secs().div_ceil(60);
                    refuse(
                        &mut writer,
                        &format!(
                            "{} {minutes} {}",
                            t(
                                "Забагато неправильних кодів. Спробуйте за",
                                "Too many wrong codes. Try again in",
                            ),
                            t("хв.", "min."),
                        ),
                    )?;
                    bail!(
                        "pairing paused after wrong codes ({} s left)",
                        wait.as_secs()
                    );
                }
                // The user needs time to read and type the code.
                reader.receiver().set_idle_limit(Some(PAIRING_TIME));
                deadline.extend(PAIRING_TIME);
                // `None`: no attempt (the dialog was cancelled or there is none), which
                // is not a wrong code.
                let paired = match obtain_code(headset_shows, tray) {
                    Ok(code) => {
                        pair(&mut reader, &mut writer, &security.identity, &peer, &code).map(Some)
                    }
                    Err(NoCode::Cancelled) => refuse(
                        &mut writer,
                        t("На ПК код не введено", "No code was typed on the PC"),
                    )
                    .map(|_| None),
                    Err(NoCode::NoDialog) => refuse(
                        &mut writer,
                        t(
                            "На ПК немає вікна для коду. Виберіть «Навпаки: код на ноутбуці, ввести тут».",
                            "The PC has no dialog for the code. Choose \"The other way: code on the PC, type it here\".",
                        ),
                    )
                    .map(|_| None),
                };
                reader.receiver().set_idle_limit(Some(READ_TIME));
                deadline.extend(SETUP_TIME);
                // The headset sent its half of the exchange with its request: with no
                // attempt made, it is read and dropped so the next request can follow.
                if headset_shows && matches!(paired, Ok(None)) {
                    let _ = ClientMsg::read(&mut reader);
                }
                // Every attempt that did not succeed counts towards the slowdown: a
                // cancelled dialog or a broken exchange too, so a peer cannot keep
                // the PC showing dialogs and codes for free.
                let paired = match paired {
                    Ok(p) => p,
                    Err(e) => {
                        LIMITER.lock().unwrap().failed();
                        return Err(e);
                    }
                };
                match paired {
                    None => {
                        info!("pairing not attempted: no code on the PC");
                        LIMITER.lock().unwrap().failed();
                    }
                    Some(true) => {
                        LIMITER.lock().unwrap().succeeded();
                        *PC_CODE.lock().unwrap() = None;
                        tray::set_pairing(tray, None);
                        let label = if name.is_empty() {
                            "headset".into()
                        } else {
                            name
                        };
                        security.trusted.add(peer.key, &label)?;
                        info!("paired with {label} ({})", secure::fingerprint(&peer.key));
                        notify(
                            t("Шолом сполучено", "Headset paired"),
                            &format!(
                                "{label} {}",
                                t("тепер може підключатися.", "can connect now.")
                            ),
                        );
                    }
                    Some(false) => {
                        warn!("pairing failed: the codes did not match");
                        LIMITER.lock().unwrap().failed();
                    }
                }
            }
            other => bail!("unexpected message before the session: {}", brief(&other)),
        }
    }
}

/// The code for this attempt: typed on the PC (the headset shows it), or generated
/// here and shown in the tray and a notification (the headset asks for it).
fn obtain_code(headset_shows: bool, tray: Option<&tray::Handle>) -> Result<String, NoCode> {
    if headset_shows {
        return ask_code();
    }
    let mut current = PC_CODE.lock().unwrap();
    let (code, new) = match &*current {
        Some((code, made)) if made.elapsed() < CODE_LIFETIME => (code.clone(), false),
        _ => {
            let code = secure::pairing_code();
            *current = Some((code.clone(), Instant::now()));
            (code, true)
        }
    };
    drop(current);
    let shown = format!("{} {}", &code[..3], &code[3..]);
    // Never into the log: whoever can read the journal could pair with it.
    info!("pairing code shown in the tray");
    tray::set_pairing(tray, Some(shown.clone()));
    // Once per code: a peer asking again and again must not flood the desktop.
    if new {
        notify(
            t("Сполучення шолома", "Pairing a headset"),
            &format!(
                "{} {shown}",
                t("Введіть у шоломі код", "Type this code in the headset:")
            ),
        );
    }
    Ok(code)
}

/// A headset turned away for its version again: a 0.1 headset that is not paired yet
/// tries every few hundred milliseconds for minutes, and it was logged and shown once.
#[derive(Debug)]
pub struct TurnedAwayAgain(String);

impl std::fmt::Display for TurnedAwayAgain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TurnedAwayAgain {}

fn turned_away(first: bool, why: String) -> anyhow::Error {
    if first {
        anyhow::anyhow!(why)
    } else {
        TurnedAwayAgain(why).into()
    }
}

/// Lets the headset read what was sent before the connection closes. Closing with
/// unread data from the peer resets the connection, which can throw away what was
/// sent (over `adb reverse` it does): so the sending half closes first, and what the
/// headset still sends is read and dropped for a moment.
fn close_gently(writer: StreamWriter<'_>, mut reader: StreamReader<'_>) {
    let _ = writer.0.finish();
    let until = Instant::now() + Duration::from_millis(500);
    let mut buf = [0u8; 4096];
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        if left.is_zero() {
            break;
        }
        reader.receiver().set_idle_limit(Some(left));
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// The headset speaks another protocol: tell both the headset and the person at the PC
/// which side needs the update. `false` when this was logged and shown a moment ago.
fn incompatible(writer: &mut impl Write, pc_too_old: bool, headset_app: &str) -> Result<bool> {
    let ours = env!("CARGO_PKG_VERSION");
    let headset_app = if headset_app.is_empty() {
        t("старіша", "older")
    } else {
        headset_app
    };
    let (title, body) = if pc_too_old {
        (
            t("Оновіть Prostir на ПК", "Update Prostir on the PC"),
            format!(
                "{} {headset_app}, {} {ours}. prostir.ishawyha.dev",
                t("Застосунок у шоломі новіший:", "The headset app is newer:"),
                t("а на ПК", "the PC has")
            ),
        )
    } else {
        (
            t("Оновіть Prostir у шоломі", "Update Prostir in the headset"),
            format!(
                "{} {headset_app}, {} {ours}.",
                t("Застосунок у шоломі старіший:", "The headset app is older:"),
                t("а на ПК", "the PC has")
            ),
        )
    };
    // Headsets from before `Incompatible` existed still show this text.
    HostMsg::Error {
        text: format!("{title}. {body}"),
    }
    .write(writer)?;
    HostMsg::Incompatible {
        protocol: PROTOCOL_VERSION,
        app: ours.into(),
    }
    .write(writer)?;
    writer.flush()?;
    // Once per headset version, and at most once in ten minutes whatever versions a
    // peer claims: not on every reconnection attempt.
    static TOLD: std::sync::Mutex<(Vec<String>, Option<Instant>)> =
        std::sync::Mutex::new((Vec::new(), None));
    let mut told = TOLD.lock().unwrap();
    let quiet = told
        .1
        .is_some_and(|t| t.elapsed() < Duration::from_secs(600));
    let first = !quiet && !told.0.iter().any(|t| t == headset_app);
    if first {
        told.0.push(headset_app.to_owned());
        told.1 = Some(Instant::now());
        warn!("{title}: {body}");
        notify(title, &body);
    } else {
        debug!("{title}: {body}");
    }
    Ok(first)
}

/// The app version a peer that has not proved anything yet says it has, as it may go
/// into the log or a notification: version-like characters only, and short.
fn peer_version(app: &str) -> String {
    app.chars()
        .filter(|c| c.is_ascii_alphanumeric() || ".+-".contains(*c))
        .take(24)
        .collect()
}

fn refuse(writer: &mut impl Write, why: &str) -> Result<bool> {
    HostMsg::Error { text: why.into() }.write(writer)?;
    HostMsg::PairResult { tag: None }.write(writer)?;
    writer.flush()?;
    Ok(false)
}

/// A short description of `msg` for the log (a message from a peer that is not
/// trusted must not become a huge log line).
fn brief(msg: &ClientMsg) -> String {
    let full = format!("{msg:?}");
    let mut short: String = full.chars().take(80).collect();
    if full.chars().count() > 80 {
        short.push('…');
    }
    short
}

/// One pairing attempt with `code`; `Ok(false)` when the codes did not match.
fn pair(
    reader: &mut impl Read,
    writer: &mut impl Write,
    identity: &Identity,
    peer: &PeerInfo,
    code: &str,
) -> Result<bool> {
    let (pairing, msg) = Pairing::start(code);
    HostMsg::PairMsg { msg }.write(writer)?;
    writer.flush()?;
    let theirs = match ClientMsg::read(reader)? {
        ClientMsg::PairMsg { msg } => msg,
        other => bail!(
            "expected the headset's pairing message, got {}",
            brief(&other)
        ),
    };
    let key = pairing.finish(&theirs)?;
    let tag = match ClientMsg::read(reader)? {
        ClientMsg::PairConfirm { tag } => tag,
        other => bail!("expected a pairing confirmation, got {}", brief(&other)),
    };
    let own = &identity.public;
    let ok = tags_match(&tag, &confirmation(&key, "headset", peer, own));
    HostMsg::PairResult {
        tag: ok.then(|| confirmation(&key, "pc", peer, own)),
    }
    .write(writer)?;
    writer.flush()?;
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    use prostir_proto::link::tcp::Plain;

    /// The headset's side of a pairing attempt, as the Quest app does it.
    fn headset(stream: TcpStream, code: &str) -> bool {
        let dir =
            std::env::temp_dir().join(format!("prostir-pair-h-{}-{code}", std::process::id()));
        let id = Identity::load_or_create(&dir.join("key")).unwrap();
        let mut link = NoiseTcp::connect(stream, &id, Arc::new(Plain)).unwrap();
        let peer = link.peer.clone();
        let mut r = StreamReader::new(&mut *link.receiver);
        let mut w = StreamWriter(&mut *link.sender);
        let (p, msg) = Pairing::start(code);
        ClientMsg::PairMsg { msg }.write(&mut w).unwrap();
        let HostMsg::PairMsg { msg: theirs } = HostMsg::read(&mut r).unwrap() else {
            panic!("expected PairMsg")
        };
        let key = p.finish(&theirs).unwrap();
        ClientMsg::PairConfirm {
            tag: confirmation(&key, "headset", &peer, &id.public),
        }
        .write(&mut w)
        .unwrap();
        let HostMsg::PairResult { tag } = HostMsg::read(&mut r).unwrap() else {
            panic!("expected PairResult")
        };
        std::fs::remove_dir_all(dir).unwrap();
        tag.is_some_and(|tag| tags_match(&tag, &confirmation(&key, "pc", &peer, &id.public)))
    }

    fn run(pc_code: &'static str, headset_code: &'static str) -> (bool, bool) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h =
            std::thread::spawn(move || headset(TcpStream::connect(addr).unwrap(), headset_code));
        let (s, _) = l.accept().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "prostir-pair-p-{}-{pc_code}{headset_code}",
            std::process::id()
        ));
        let id = Identity::load_or_create(&dir.join("key")).unwrap();
        let mut link = NoiseTcp::accept(s, &id, Arc::new(Plain)).unwrap();
        let peer = link.peer.clone();
        let mut r = StreamReader::new(&mut *link.receiver);
        let mut w = StreamWriter(&mut *link.sender);
        let pc = pair(&mut r, &mut w, &id, &peer, pc_code).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        (pc, h.join().unwrap())
    }

    #[test]
    fn same_code_pairs_both_sides() {
        assert_eq!(run("123456", "123456"), (true, true));
    }

    #[test]
    fn different_code_pairs_neither() {
        assert_eq!(run("123456", "654321"), (false, false));
    }

    /// A PC in a test: its keys in a directory of its own.
    struct TestPc {
        dir: PathBuf,
        security: Security,
    }

    impl TestPc {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("prostir-{name}-p-{}", std::process::id()));
            let security = Security {
                identity: Identity::load_or_create(&dir.join("key")).unwrap(),
                trusted: TrustStore::load(&dir.join("trusted")),
            };
            Self { dir, security }
        }
    }

    impl Drop for TestPc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Runs `headset` against a PC that accepts one connection; what each side got.
    fn connect<T: Send + 'static>(
        pc: &mut TestPc,
        headset: impl FnOnce(&mut StreamReader, &mut StreamWriter, &PeerInfo, &Identity) -> T
        + Send
        + 'static,
    ) -> (Result<()>, T) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let name = pc
            .dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace("-p-", "-h-");
        let h = std::thread::spawn(move || {
            let stream = TcpStream::connect(addr).unwrap();
            let dir = std::env::temp_dir().join(name);
            let id = Identity::load_or_create(&dir.join("key")).unwrap();
            let mut link = NoiseTcp::connect(stream, &id, Arc::new(Plain)).unwrap();
            let peer = link.peer.clone();
            let mut r = StreamReader::new(&mut *link.receiver);
            let mut w = StreamWriter(&mut *link.sender);
            let got = headset(&mut r, &mut w, &peer, &id);
            std::fs::remove_dir_all(dir).unwrap();
            got
        });
        let (s, _) = l.accept().unwrap();
        // The headset reads until the PC hangs up.
        let accepted = accept(s, &mut pc.security, None).map(|link| link.closer.close());
        (accepted, h.join().unwrap())
    }

    /// The messages a headset of protocol 1 (0.1.x) reads, the way it reads them: tag,
    /// length, payload.
    fn read_as_v1(r: &mut impl std::io::Read) -> Vec<(u8, Vec<u8>)> {
        let mut msgs = Vec::new();
        let mut header = [0u8; 5];
        while r.read_exact(&mut header).is_ok() {
            let len = u32::from_le_bytes(header[1..].try_into().unwrap());
            let mut payload = vec![0u8; len as usize];
            r.read_exact(&mut payload).unwrap();
            msgs.push((header[0], payload));
        }
        msgs
    }

    /// What a PC of protocol 1 or 2 tells a headset of protocol 1 that it turns away:
    /// a text naming the side to update (tag 6), then the PC's protocol and app
    /// version (tag 17), in the bytes protocol 1 reads.
    fn assert_told_to_update(msgs: &[(u8, Vec<u8>)]) {
        let [(6, text), (17, incompatible)] = msgs else {
            panic!("expected an error and Incompatible, got {msgs:?}")
        };
        let text = String::from_utf8(text.clone()).unwrap();
        assert!(text.contains("Prostir"), "{text}");
        let mut expected = PROTOCOL_VERSION.to_le_bytes().to_vec();
        expected.extend_from_slice(env!("CARGO_PKG_VERSION").as_bytes());
        assert_eq!(incompatible, &expected);
    }

    /// A headset from another protocol version learns which side to update.
    #[test]
    fn other_protocol_is_told_to_update() {
        let mut pc = TestPc::new("ver");
        let (refused, (first, second)) = connect(&mut pc, |r, w, _, _| {
            ClientMsg::Hello {
                version: PROTOCOL_VERSION + 1,
                min: PROTOCOL_VERSION + 1,
                app: "9.9.9".into(),
            }
            .write(w)
            .unwrap();
            (HostMsg::read(r).unwrap(), HostMsg::read(r).unwrap())
        });
        assert!(refused.is_err(), "the session must not start");
        // Older headsets show this text; it names the side to update.
        assert!(matches!(first, HostMsg::Error { ref text } if text.contains("Prostir")));
        assert_eq!(
            second,
            HostMsg::Incompatible {
                protocol: PROTOCOL_VERSION,
                app: env!("CARGO_PKG_VERSION").into(),
            }
        );
    }

    /// A 0.1 headset says hello in protocol 1 and is told to update, in bytes it reads.
    #[test]
    fn a_protocol_1_hello_is_told_to_update() {
        let mut pc = TestPc::new("v1");
        let (refused, msgs) = connect(&mut pc, |r, w, _, _| {
            // Hello as 0.1.1 sends it: tag 101, length, version 1, min 1, its version.
            let mut hello = vec![101, 13, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0];
            hello.extend_from_slice(b"0.1.1");
            w.write_all(&hello).unwrap();
            read_as_v1(r)
        });
        assert!(refused.is_err(), "the session must not start");
        assert_told_to_update(&msgs);
    }

    /// A 0.1 headset that does not trust the PC yet asks to pair before saying hello:
    /// it is told to update instead of pairing in a protocol it does not speak.
    #[test]
    fn a_protocol_1_pairing_request_is_told_to_update() {
        let mut pc = TestPc::new("v1pair");
        let (refused, msgs) = connect(&mut pc, |r, w, _, _| {
            // PairBegin as 0.1.1 sends it: the headset shows the code, then its name;
            // right after it, its 33-byte SPAKE2 message, which the PC never reads.
            let mut begin = vec![109, 11, 0, 0, 0, 1];
            begin.extend_from_slice(b"Meta Quest");
            let (_, msg) = Pairing::start("123456");
            begin.extend_from_slice(&[110, 33, 0, 0, 0]);
            begin.extend_from_slice(&msg);
            w.write_all(&begin).unwrap();
            read_as_v1(r)
        });
        assert!(refused.is_err(), "no pairing and no session");
        assert_told_to_update(&msgs);
        assert!(pc.security.trusted.is_empty());
    }

    #[test]
    fn a_peer_version_is_short_and_plain() {
        assert_eq!(peer_version("0.2.0-beta+1"), "0.2.0-beta+1");
        assert_eq!(
            peer_version("1.0\n\u{1b}[31mfake log line"),
            "1.031mfakelogline"
        );
        assert_eq!(peer_version(&"9".repeat(60_000)).len(), 24);
    }

    /// Three wrong codes are free; after that every attempt waits longer, up to 15 min.
    #[test]
    fn wrong_codes_slow_pairing_down() {
        let mut l = Limiter {
            failures: 0,
            not_before: None,
            warned: true, // no notification from tests
        };
        for _ in 0..FREE_FAILURES {
            l.failed();
            assert!(l.wait().is_none());
        }
        l.failed();
        let first = l.wait().expect("a pause after the free ones");
        assert!(first > Duration::from_secs(25) && first <= Duration::from_secs(30));
        l.failed();
        assert!(l.wait().unwrap() > first);
        for _ in 0..20 {
            l.failed();
        }
        assert!(l.wait().unwrap() <= Duration::from_secs(900));
        l.succeeded();
        assert!(l.wait().is_none());
    }

    /// A headset the PC trusts still opens the session first: a session message before
    /// `Open` ends the connection, and nothing starts.
    #[test]
    fn nothing_before_open() {
        let mut pc = TestPc::new("early");
        // The headset's key, made before it connects, so the PC can trust it.
        let name = pc
            .dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace("-p-", "-h-");
        let key = Identity::load_or_create(&std::env::temp_dir().join(name).join("key"))
            .unwrap()
            .public;
        pc.security.trusted.add(key, "test headset").unwrap();
        let (refused, after) = connect(&mut pc, |r, w, _, _| {
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                min: prostir_proto::MIN_PROTOCOL,
                app: "test".into(),
            }
            .write(w)
            .unwrap();
            assert!(matches!(HostMsg::read(r).unwrap(), HostMsg::Welcome { .. }));
            ClientMsg::AddMonitor {
                width: 1920,
                height: 1080,
            }
            .write(w)
            .unwrap();
            HostMsg::read(r).map_err(|e| e.kind())
        });
        let err = refused.unwrap_err().to_string();
        assert!(err.contains("before the session"), "{err}");
        assert_eq!(
            after,
            Err(std::io::ErrorKind::UnexpectedEof),
            "closed, no Opened"
        );
    }

    /// "Code on the PC": the headset asks first, the PC makes and shows its code, the
    /// user types it in the headset, and the headset is trusted afterwards.
    #[test]
    fn code_shown_on_the_pc_pairs() {
        let mut pc = TestPc::new("pcc");
        let (accepted, (ok, opened)) = connect(&mut pc, |r, w, peer, id| {
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                min: prostir_proto::MIN_PROTOCOL,
                app: "test".into(),
            }
            .write(w)
            .unwrap();
            assert!(matches!(HostMsg::read(r).unwrap(), HostMsg::Welcome { .. }));
            ClientMsg::Open.write(w).unwrap();
            assert_eq!(HostMsg::read(r).unwrap(), HostMsg::PairingRequired);
            ClientMsg::PairBegin {
                headset_shows: false,
                name: "test headset".into(),
            }
            .write(w)
            .unwrap();
            let HostMsg::PairMsg { msg: theirs } = HostMsg::read(r).unwrap() else {
                panic!("expected PairMsg")
            };
            // What the user reads in the tray and types in the headset.
            let code = PC_CODE.lock().unwrap().clone().unwrap().0;
            let (p, msg) = Pairing::start(&code);
            ClientMsg::PairMsg { msg }.write(w).unwrap();
            let key = p.finish(&theirs).unwrap();
            ClientMsg::PairConfirm {
                tag: confirmation(&key, "headset", peer, &id.public),
            }
            .write(w)
            .unwrap();
            let HostMsg::PairResult { tag } = HostMsg::read(r).unwrap() else {
                panic!("expected PairResult")
            };
            ClientMsg::Open.write(w).unwrap();
            (tag.is_some(), HostMsg::read(r).unwrap())
        });
        assert!(ok, "the codes must match");
        assert_eq!(opened, HostMsg::Opened);
        assert!(accepted.is_ok());
        assert_eq!(pc.security.trusted.len(), 1);
    }
}
