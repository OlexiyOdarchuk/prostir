//! Encrypted connections and pairing on the PC side.

use crate::i18n::t;
use std::io;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use prostir_proto::discovery::{FindSecret, new_secret};
use prostir_proto::handshake::{self, First};
use prostir_proto::link::tcp::NoiseTcp;
use prostir_proto::link::{Closer, Link, PeerInfo};
use prostir_proto::packet::{self, Assembler, FRAGMENT, lane};
use prostir_proto::secure::{self, Identity, Pairing, TrustStore, confirmation, tags_match};
use prostir_proto::state::Part;
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

    /// An attempt starts: it counts as a wrong code until it succeeds, so one that
    /// never ends (a connection dropped half way) is not free.
    fn begin(&mut self) {
        self.failures += 1;
        self.pause();
    }

    /// The attempt did not succeed: the pause (if any) runs from now.
    fn failed(&mut self) {
        self.pause();
    }

    fn pause(&mut self) {
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

/// The connection whose code the tray shows now, if one does (by its number), and
/// the code.
static CODE_SHOWN_BY: Mutex<Option<(u64, String)>> = Mutex::new(None);
/// Told when the code shown comes or goes (the local API shows it too), and how each
/// pairing ends.
static ON_CODE: std::sync::OnceLock<crate::events::EventSink> = std::sync::OnceLock::new();

/// From now on, `events` hears when the PC begins or stops showing a pairing code, and
/// how each pairing ends.
pub fn tell_about_codes(events: crate::events::EventSink) {
    let _ = ON_CODE.set(events);
}

fn tell(event: crate::events::Event) {
    if let Some(events) = ON_CODE.get() {
        events.send(event);
    }
}

/// The settings window is on the bus: a code a headset shows is typed there, not in
/// a dialog.
static WINDOW_TYPES: AtomicBool = AtomicBool::new(false);

/// A headset waiting for its code to be typed in the settings window: its connection's
/// number, its name, and where the code goes.
static CODE_FOR: Mutex<Option<(u64, String, std::sync::mpsc::Sender<String>)>> = Mutex::new(None);

/// Whether the settings window is on the bus (codes are typed there). Once it leaves,
/// a headset waiting for its code there is told no code was typed.
pub fn window_types_codes(on: bool) {
    WINDOW_TYPES.store(on, Ordering::Release);
    if !on && CODE_FOR.lock().unwrap().take().is_some() {
        code_changed();
    }
}

/// The name of the headset waiting for its code to be typed in the settings window.
pub fn code_for() -> Option<String> {
    CODE_FOR
        .lock()
        .unwrap()
        .as_ref()
        .map(|(_, name, _)| name.clone())
}

/// The code a headset shows, typed in the settings window: `false` when no headset
/// waits for one.
pub fn type_code(code: String) -> bool {
    CODE_FOR
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|(_, _, to)| to.send(code).is_ok())
}

/// The pairing code the PC shows now, if any.
pub fn code_shown() -> Option<String> {
    CODE_SHOWN_BY
        .lock()
        .unwrap()
        .as_ref()
        .map(|(_, code)| code.clone())
}

fn code_changed() {
    if let Some(events) = ON_CODE.get() {
        events.send(crate::events::Event::Pairing);
    }
}
/// The number of the next connection set up.
static NEXT_CONNECTION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The tray's pairing code as one connection sees it: shown by it, and hidden when it
/// ends, but only if it is still the one that showed it (another connection ending
/// leaves the code of the one pairing).
struct TrayCode<'a> {
    tray: Option<&'a tray::Handle>,
    connection: u64,
}

impl<'a> TrayCode<'a> {
    fn new(tray: Option<&'a tray::Handle>) -> Self {
        Self {
            tray,
            connection: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
        }
    }

    fn show(&self, code: String) {
        *CODE_SHOWN_BY.lock().unwrap() = Some((self.connection, code.clone()));
        tray::set_pairing(self.tray, Some(code));
        code_changed();
    }

    /// The attempt ended: a headset waiting for its code to be typed in the settings
    /// window waits no more, if it was this connection's.
    fn code_done(&self) {
        let mut waiting = CODE_FOR.lock().unwrap();
        if waiting
            .as_ref()
            .is_some_and(|(by, _, _)| *by == self.connection)
        {
            *waiting = None;
            drop(waiting);
            code_changed();
        }
    }

    fn hide(&self) {
        self.code_done();
        let mut shown_by = CODE_SHOWN_BY.lock().unwrap();
        if shown_by
            .as_ref()
            .is_some_and(|(by, _)| *by == self.connection)
        {
            *shown_by = None;
            drop(shown_by);
            tray::set_pairing(self.tray, None);
            code_changed();
        }
    }

    #[cfg(test)]
    fn shows(&self) -> bool {
        CODE_SHOWN_BY
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(by, _)| *by == self.connection)
    }
}

impl Drop for TrayCode<'_> {
    fn drop(&mut self) {
        self.hide();
    }
}

/// Held while a headset pairs: one pairing at a time, so the wrong codes counted
/// before each attempt hold for every connection (several at once would each pass
/// the check before any had failed).
static PAIRING: Mutex<()> = Mutex::new(());

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

/// The PC's key and the headsets it trusts, shared by the connections being set up
/// (each on a thread of its own) and the control loop, which forgets them.
pub struct Security {
    pub identity: Identity,
    pub trusted: Mutex<TrustStore>,
    /// What the trusted headsets' discovery tags for this PC are made with (see
    /// `prostir_proto::discovery`): given to them in the session, made anew when the
    /// PC forgets a headset.
    pub find: Mutex<FindSecret>,
    /// Where the find secret is kept.
    find_file: PathBuf,
}

/// The file of the find secret, in `dir`.
pub fn find_secret_file(dir: &std::path::Path) -> PathBuf {
    dir.join("find_secret")
}

/// The find secret kept in `path`, made on first use. A file that is there but does
/// not read is an error, as for the PC's key: a new one would leave every headset
/// unable to find the PC without a word.
pub fn load_find_secret(path: &std::path::Path) -> Result<FindSecret> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let text = text.trim();
            let mut secret = [0u8; 32];
            let ok = text.len() == 64
                && secret.iter_mut().enumerate().all(|(i, b)| {
                    text.get(i * 2..i * 2 + 2)
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                        .map(|v| *b = v)
                        .is_some()
                });
            anyhow::ensure!(
                ok,
                "{} is no find secret (move it away to make a new one; headsets get it at \
                 their next session)",
                path.display()
            );
            Ok(secret)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => save_find_secret(path, new_secret()),
        Err(e) => Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
    }
}

fn save_find_secret(path: &std::path::Path, secret: FindSecret) -> Result<FindSecret> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    secure::write_atomic(path, format!("{}\n", secure::to_hex(&secret)).as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(secret)
}

impl Security {
    pub fn load() -> Result<Self> {
        let dir = config_dir();
        let identity =
            Identity::load_or_create(&dir.join("identity")).context("cannot load the PC's key")?;
        info!("PC key {}", secure::fingerprint(&identity.public));
        // Unreadable, it would read as no headset at all: everyone answered.
        let trusted = TrustStore::try_load(&dir.join("paired_headsets"))
            .context("cannot read the paired headsets")?;
        let find_file = find_secret_file(&dir);
        let find = load_find_secret(&find_file)?;
        Ok(Self {
            identity,
            trusted: Mutex::new(trusted),
            find: Mutex::new(find),
            find_file,
        })
    }

    /// A new find secret (a headset was forgotten): queries made with the old one get
    /// no answer any more. The new one: the headsets still trusted get it in their
    /// session.
    pub fn new_find_secret(&self) -> FindSecret {
        let secret = new_secret();
        if let Err(e) = save_find_secret(&self.find_file, secret) {
            warn!("{e:#}");
        }
        *self.find.lock().unwrap() = secret;
        info!("a new find secret: a forgotten headset can no longer find this PC");
        secret
    }

    /// The find secret now.
    pub fn find_secret(&self) -> FindSecret {
        *self.find.lock().unwrap()
    }
}

/// A connection in packets: the link, and the headset's packets put together so far.
pub struct Connection {
    pub link: Link,
    pub packets: Assembler,
}

impl Connection {
    fn recv(&mut self) -> io::Result<ClientMsg> {
        ClientMsg::recv(&mut *self.link.receiver, &mut self.packets)
    }

    /// Sends `msg` whole in the control lane (before the session's writer runs).
    pub fn send(&mut self, msg: HostMsg) -> io::Result<()> {
        packet::send_whole(
            &mut *self.link.sender,
            lane::CONTROL,
            msg.encode()?,
            FRAGMENT,
        )
    }

    fn set_idle_limit(&mut self, idle: Duration) {
        self.link.receiver.set_idle_limit(Some(idle));
    }
}

/// Encrypts the connection, checks the versions, pairs the headset if either side asks,
/// and returns the connection once the headset is trusted and asked to open the
/// session, with the parts of the headset's state its `Open` carried. The caller
/// answers `Snapshot` once it can serve the session (its monitors made, its writer
/// running), or an `Error` saying why it cannot.
pub fn accept(
    stream: TcpStream,
    security: &Security,
    tray: Option<&tray::Handle>,
) -> Result<(Connection, Vec<Part>)> {
    accept_within(stream, security, tray, SETUP_TIME)
}

/// As [`accept`], with `setup` for everything up to `Open` (but pairing).
fn accept_within(
    stream: TcpStream,
    security: &Security,
    tray: Option<&tray::Handle>,
    setup: Duration,
) -> Result<(Connection, Vec<Part>)> {
    // A vanished headset does not hold the PC (the transport also gives up a write
    // after a few seconds, and the deadline closes a setup that drags on).
    stream.set_read_timeout(Some(READ_TIME))?;
    // The deadline covers the Noise handshake too: a peer that sends a byte now and
    // then, each within the read timeout, must not hold the PC for hours. Until the
    // link exists, the deadline closes the socket itself.
    let socket = stream.try_clone()?;
    let closer = Closer::new(move || {
        let _ = socket.shutdown(std::net::Shutdown::Both);
    });
    let deadline = Deadline::new(&closer, setup)?;
    let mut link = NoiseTcp::accept(
        stream,
        &security.identity,
        Arc::new(crate::platform::TcpOptions),
    )?;
    // However this connection ends, the tray stops showing a code it showed.
    let tray_code = TrayCode::new(tray);
    let peer = link.peer.clone();
    // Versions first, before pairing: this exchange is the same in every version, so
    // a headset of any version learns which side to update.
    match handshake::read_first(&mut *link.receiver)? {
        First::Hello {
            version, min, app, ..
        } if !prostir_proto::compatible(version, min) => {
            let app = peer_version(&app);
            let update = if min > PROTOCOL_VERSION {
                Update::Pc
            } else {
                Update::Headset
            };
            let first = incompatible(&mut link, update, &app)?;
            close_gently(&mut link);
            return Err(turned_away(
                first,
                format!("headset speaks protocol v{version} (app {app}), PC v{PROTOCOL_VERSION}"),
            ));
        }
        // Two development builds of different revisions: the same protocol version,
        // other messages.
        First::Hello { app, .. } if !prostir_proto::same_revision(&app) => {
            let app = peer_version(&app);
            let first = incompatible(&mut link, Update::Both, &app)?;
            close_gently(&mut link);
            return Err(turned_away(
                first,
                format!(
                    "headset build {app} reads other messages than this one ({})",
                    prostir_proto::app_version(env!("CARGO_PKG_VERSION"))
                ),
            ));
        }
        First::Hello { alone: true, .. } => {}
        // Packets follow the hello: one that shares its record leaves bytes they
        // cannot account for.
        First::Hello { alone: false, .. } => bail!("the headset's hello shares its record"),
        // Only headsets from before protocol 2 (0.1) start with anything else: they
        // could ask to pair before saying hello.
        First::Other => {
            let first = incompatible(&mut link, Update::Headset, "0.1.x")?;
            close_gently(&mut link);
            return Err(turned_away(
                first,
                format!("headset from before protocol {PROTOCOL_VERSION}: no hello first"),
            ));
        }
    }
    handshake::send_from_pc(
        &mut *link.sender,
        &HostMsg::Welcome {
            version: PROTOCOL_VERSION,
            language: crate::i18n::code().into(),
        },
    )?;
    // From here on, packets.
    let mut conn = Connection {
        link,
        packets: ClientMsg::assembler(),
    };
    let mut attempts = 0;
    loop {
        match conn.recv()? {
            ClientMsg::Open { parts } if security.trusted.lock().unwrap().contains(&peer.key) => {
                conn.link.receiver.set_idle_limit(None);
                drop(deadline);
                return Ok((conn, parts));
            }
            ClientMsg::Open { .. } => {
                info!(
                    "unknown headset {}: pairing needed",
                    secure::fingerprint(&peer.key)
                );
                conn.send(HostMsg::PairingRequired)?;
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
                        &mut conn,
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
                let _pairing = match PAIRING.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(e)) => e.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        info!("another headset is pairing: this one is told to wait");
                        refuse(
                            &mut conn,
                            t(
                                "Інший шолом саме сполучається з цим ПК. Спробуйте за хвилину.",
                                "Another headset is pairing with this PC right now. Try again in a minute.",
                            ),
                        )?;
                        // Its half of the exchange came with the request.
                        if headset_shows {
                            let _ = conn.recv();
                        }
                        continue;
                    }
                };
                LIMITER.lock().unwrap().begin();
                // The user needs time to read and type the code.
                conn.set_idle_limit(PAIRING_TIME);
                deadline.extend(PAIRING_TIME);
                // `None`: no attempt (the dialog was cancelled or there is none), which
                // is not a wrong code.
                let paired = match obtain_code(headset_shows, &name, &tray_code) {
                    Ok(code) => pair(&mut conn, &security.identity, &peer, &code).map(Some),
                    Err(NoCode::Cancelled) => refuse(
                        &mut conn,
                        t("На ПК код не введено", "No code was typed on the PC"),
                    )
                    .map(|_| None),
                    Err(NoCode::NoDialog) => refuse(
                        &mut conn,
                        t(
                            "На ПК немає вікна для коду. Виберіть «Навпаки: код на ноутбуці, ввести тут».",
                            "The PC has no dialog for the code. Choose \"The other way: code on the PC, type it here\".",
                        ),
                    )
                    .map(|_| None),
                };
                conn.set_idle_limit(READ_TIME);
                deadline.extend(setup);
                // The headset sent its half of the exchange with its request: with no
                // attempt made, it is read and dropped so the next request can follow.
                if headset_shows && matches!(paired, Ok(None)) {
                    let _ = conn.recv();
                }
                // Every attempt that did not succeed counted towards the slowdown (from
                // its start): a cancelled dialog or a broken exchange too, so a peer
                // cannot keep the PC showing dialogs and codes for free.
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
                        tell(crate::events::Event::Paired {
                            name: if name.is_empty() {
                                t("Шолом", "Headset").into()
                            } else {
                                name.clone()
                            },
                        });
                        *PC_CODE.lock().unwrap() = None;
                        tray_code.hide();
                        let label = if name.is_empty() {
                            "headset".into()
                        } else {
                            name
                        };
                        security.trusted.lock().unwrap().add(peer.key, &label)?;
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
                        tell(crate::events::Event::WrongCode);
                    }
                }
                tray_code.code_done();
            }
            other => bail!("unexpected message before the session: {}", brief(&other)),
        }
    }
}

/// The code for this attempt: typed on the PC (the headset shows it), or generated
/// here and shown in the tray and a notification (the headset asks for it).
fn obtain_code(headset_shows: bool, name: &str, tray: &TrayCode) -> Result<String, NoCode> {
    if headset_shows {
        if !WINDOW_TYPES.load(Ordering::Acquire) {
            return ask_code();
        }
        // The settings window is open: the code is typed there.
        let (to, code) = std::sync::mpsc::channel();
        let name = if name.is_empty() {
            t("Шолом", "Headset").to_owned()
        } else {
            name.chars().take(64).collect()
        };
        *CODE_FOR.lock().unwrap() = Some((tray.connection, name, to));
        code_changed();
        // The window shows the headset waiting until the attempt ends (`code_done`).
        return code
            .recv_timeout(PAIRING_TIME)
            .map_err(|_| NoCode::Cancelled);
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
    tray.show(shown.clone());
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
pub fn close_gently(link: &mut Link) {
    let _ = link.sender.finish();
    let until = Instant::now() + Duration::from_millis(500);
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        if left.is_zero() {
            break;
        }
        link.receiver.set_idle_limit(Some(left));
        if link.receiver.recv().is_err() {
            break;
        }
    }
}

/// Which side has to be updated for a headset and a PC to talk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Update {
    Pc,
    Headset,
    /// Development builds of different revisions (see `prostir_proto::PROTO_REVISION`).
    Both,
}

/// The headset speaks another protocol: tell both the headset and the person at the PC
/// which side needs the update. `false` when this was logged and shown a moment ago.
fn incompatible(link: &mut Link, update: Update, headset_app: &str) -> Result<bool> {
    let ours = prostir_proto::app_version(env!("CARGO_PKG_VERSION"));
    let headset_app = if headset_app.is_empty() {
        t("старіша", "older")
    } else {
        headset_app
    };
    let (title, body) = match update {
        Update::Pc => (
            t("Оновіть Prostir на ПК", "Update Prostir on the PC"),
            format!(
                "{} {headset_app}, {} {ours}. prostir.ishawyha.dev",
                t("Застосунок у шоломі новіший:", "The headset app is newer:"),
                t("а на ПК", "the PC has")
            ),
        ),
        Update::Headset => (
            t("Оновіть Prostir у шоломі", "Update Prostir in the headset"),
            format!(
                "{} {headset_app}, {} {ours}.",
                t("Застосунок у шоломі старіший:", "The headset app is older:"),
                t("а на ПК", "the PC has")
            ),
        ),
        Update::Both => (
            t(
                "Збірки шолома й ПК з різних версій гілки",
                "The headset and PC builds are from different versions of the branch",
            ),
            format!(
                "{} {headset_app}, {} {ours}. {}",
                t("У шоломі", "The headset has"),
                t("а на ПК", "the PC has"),
                t("Оновіть обидві.", "Update both.")
            ),
        ),
    };
    // Headsets from before `Incompatible` existed still show this text.
    let error = HostMsg::Error {
        text: format!("{title}. {body}"),
    };
    handshake::send_from_pc(&mut *link.sender, &error)?;
    let answer = HostMsg::Incompatible {
        protocol: PROTOCOL_VERSION,
        app: ours.clone(),
    };
    handshake::send_from_pc(&mut *link.sender, &answer)?;
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

fn refuse(conn: &mut Connection, why: &str) -> Result<bool> {
    conn.send(HostMsg::Error { text: why.into() })?;
    conn.send(HostMsg::PairResult { tag: None })?;
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
fn pair(conn: &mut Connection, identity: &Identity, peer: &PeerInfo, code: &str) -> Result<bool> {
    let (pairing, msg) = Pairing::start(code);
    conn.send(HostMsg::PairMsg { msg })?;
    let theirs = match conn.recv()? {
        ClientMsg::PairMsg { msg } => msg,
        other => bail!(
            "expected the headset's pairing message, got {}",
            brief(&other)
        ),
    };
    let key = pairing.finish(&theirs)?;
    let tag = match conn.recv()? {
        ClientMsg::PairConfirm { tag } => tag,
        other => bail!("expected a pairing confirmation, got {}", brief(&other)),
    };
    let own = &identity.public;
    let ok = tags_match(&tag, &confirmation(&key, "headset", peer, own));
    conn.send(HostMsg::PairResult {
        tag: ok.then(|| confirmation(&key, "pc", peer, own)),
    })?;
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    use prostir_proto::handshake::Answer;
    use prostir_proto::link::tcp::Plain;
    use prostir_proto::link::{StreamReader, StreamWriter};

    /// The headset's end of a connection in a test.
    struct Headset {
        link: Link,
        packets: Assembler,
        id: Identity,
    }

    impl Headset {
        fn connect(stream: TcpStream, id: Identity) -> Self {
            let link = NoiseTcp::connect(stream, &id, Arc::new(Plain)).unwrap();
            Self {
                link,
                packets: HostMsg::assembler(),
                id,
            }
        }

        fn send(&mut self, msg: ClientMsg) {
            let msg = msg.encode().unwrap();
            packet::send_whole(&mut *self.link.sender, lane::CONTROL, msg, FRAGMENT).unwrap();
        }

        fn recv(&mut self) -> io::Result<HostMsg> {
            HostMsg::recv(&mut *self.link.receiver, &mut self.packets)
        }

        /// Says hello as this version does; packets follow a welcome.
        fn hello(&mut self, version: u32) -> Answer {
            let hello = ClientMsg::Hello {
                version,
                min: version,
                app: prostir_proto::app_version("0.2.0"),
            };
            // Alone in its record, as protocol 2 says hello.
            let mut bytes = Vec::new();
            hello.write(&mut bytes).unwrap();
            self.write_as_v1(&bytes);
            handshake::read_answer(&mut *self.link.receiver).unwrap()
        }

        /// Bytes as a headset of protocol 1 (0.1.x) writes them: one stream.
        fn write_as_v1(&mut self, bytes: &[u8]) {
            StreamWriter(&mut *self.link.sender)
                .write_all(bytes)
                .unwrap();
        }

        /// The messages a headset of protocol 1 reads, the way it reads them: tag,
        /// length, payload, from one stream.
        fn read_as_v1(&mut self) -> Vec<(u8, Vec<u8>)> {
            use std::io::Read;
            let mut r = StreamReader::new(&mut *self.link.receiver);
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
    }

    /// The headset's side of a pairing attempt, as the Quest app does it.
    fn headset(mut h: Headset, code: &str) -> bool {
        let peer = h.link.peer.clone();
        let (p, msg) = Pairing::start(code);
        h.send(ClientMsg::PairMsg { msg });
        let HostMsg::PairMsg { msg: theirs } = h.recv().unwrap() else {
            panic!("expected PairMsg")
        };
        let key = p.finish(&theirs).unwrap();
        h.send(ClientMsg::PairConfirm {
            tag: confirmation(&key, "headset", &peer, &h.id.public),
        });
        let HostMsg::PairResult { tag } = h.recv().unwrap() else {
            panic!("expected PairResult")
        };
        tag.is_some_and(|tag| tags_match(&tag, &confirmation(&key, "pc", &peer, &h.id.public)))
    }

    fn identity(name: &str) -> Identity {
        let dir = std::env::temp_dir().join(format!("prostir-{name}-{}", std::process::id()));
        let id = Identity::load_or_create(&dir.join("key")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        id
    }

    fn run(pc_code: &'static str, headset_code: &'static str) -> (bool, bool) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let id = identity(&format!("pair-h-{pc_code}{headset_code}"));
        let h = std::thread::spawn(move || {
            headset(
                Headset::connect(TcpStream::connect(addr).unwrap(), id),
                headset_code,
            )
        });
        let (s, _) = l.accept().unwrap();
        let id = identity(&format!("pair-p-{pc_code}{headset_code}"));
        let link = NoiseTcp::accept(s, &id, Arc::new(Plain)).unwrap();
        let peer = link.peer.clone();
        let mut conn = Connection {
            link,
            packets: ClientMsg::assembler(),
        };
        let pc = pair(&mut conn, &id, &peer, pc_code).unwrap();
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
                trusted: Mutex::new(TrustStore::load(&dir.join("trusted"))),
                find: Mutex::new([5; 32]),
                find_file: find_secret_file(&dir),
            };
            Self { dir, security }
        }
    }

    /// The find secret is made once and kept; a file that does not read is an error,
    /// not a new secret; a new one replaces it in the file and in memory.
    #[test]
    fn the_find_secret_is_kept_and_made_anew_on_purpose_only() {
        let pc = TestPc::new("find-secret");
        let path = find_secret_file(&pc.dir);
        let first = load_find_secret(&path).unwrap();
        assert_eq!(load_find_secret(&path).unwrap(), first, "kept");
        let security = Security {
            identity: Identity::load_or_create(&pc.dir.join("key")).unwrap(),
            trusted: Mutex::new(TrustStore::load(&pc.dir.join("trusted"))),
            find: Mutex::new(first),
            find_file: path.clone(),
        };
        let second = security.new_find_secret();
        assert_ne!(second, first);
        assert_eq!(security.find_secret(), second);
        assert_eq!(load_find_secret(&path).unwrap(), second, "written");
        std::fs::write(&path, "not hex\n").unwrap();
        assert!(load_find_secret(&path).is_err(), "not replaced silently");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not hex\n");
    }

    impl Drop for TestPc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Runs `headset` against a PC that accepts one connection; what each side got.
    fn connect<T: Send + 'static>(
        pc: &mut TestPc,
        headset: impl FnOnce(&mut Headset) -> T + Send + 'static,
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
            let got = headset(&mut Headset::connect(stream, id));
            std::fs::remove_dir_all(dir).unwrap();
            got
        });
        let (s, _) = l.accept().unwrap();
        // The headset reads until the PC hangs up.
        // A snapshot, as the session would send it, then the PC hangs up.
        let accepted = accept(s, &pc.security, None).map(|(mut conn, _)| {
            let _ = conn.send(HostMsg::Snapshot { parts: Vec::new() });
            conn.link.closer.close();
        });
        (accepted, h.join().unwrap())
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
        expected
            .extend_from_slice(prostir_proto::app_version(env!("CARGO_PKG_VERSION")).as_bytes());
        assert_eq!(incompatible, &expected);
    }

    /// A headset from another protocol version learns which side to update.
    #[test]
    fn other_protocol_is_told_to_update() {
        let mut pc = TestPc::new("ver");
        let (refused, answer) = connect(&mut pc, |h| h.hello(PROTOCOL_VERSION + 1));
        assert!(refused.is_err(), "the session must not start");
        assert_eq!(
            answer,
            Answer::Incompatible {
                protocol: PROTOCOL_VERSION,
                app: prostir_proto::app_version(env!("CARGO_PKG_VERSION")),
            }
        );
        // Older headsets show the text before it; it names the side to update.
        let (_, msgs) = connect(&mut pc, |h| {
            h.write_as_v1(&{
                let mut hello = Vec::new();
                ClientMsg::Hello {
                    version: 9,
                    min: 9,
                    app: "9.9.9".into(),
                }
                .write(&mut hello)
                .unwrap();
                hello
            });
            h.read_as_v1()
        });
        let [(6, text), (17, _)] = &msgs[..] else {
            panic!("expected an error and Incompatible, got {msgs:?}")
        };
        assert!(String::from_utf8_lossy(text).contains("Prostir"));
    }

    /// A development build of another revision speaks the same protocol version but
    /// other messages: it is turned away, told by the PC's revision which side is older.
    #[test]
    fn another_development_revision_is_turned_away() {
        let Some(ours) = prostir_proto::PROTO_REVISION else {
            return;
        };
        let mut pc = TestPc::new("rev");
        let (refused, answer) = connect(&mut pc, move |h| {
            let hello = ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                min: PROTOCOL_VERSION,
                app: format!("0.2.0-dev+p{}", ours + 1),
            };
            let mut bytes = Vec::new();
            hello.write(&mut bytes).unwrap();
            h.write_as_v1(&bytes);
            handshake::read_answer(&mut *h.link.receiver).unwrap()
        });
        assert!(refused.is_err(), "the session must not start");
        let Answer::Incompatible { protocol, app } = answer else {
            panic!("expected Incompatible, got {answer:?}");
        };
        assert_eq!(protocol, PROTOCOL_VERSION);
        assert_eq!(prostir_proto::revision_of(&app), Some(ours));
        assert!(
            !prostir_proto::peer_is_newer(protocol, &app),
            "the headset is newer"
        );
    }

    /// A 0.1 headset says hello in protocol 1 and is told to update, in bytes it reads.
    #[test]
    fn a_protocol_1_hello_is_told_to_update() {
        let mut pc = TestPc::new("v1");
        let (refused, msgs) = connect(&mut pc, |h| {
            // Hello as 0.1.1 sends it: tag 101, length, version 1, min 1, its version.
            let mut hello = vec![101, 13, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0];
            hello.extend_from_slice(b"0.1.1");
            h.write_as_v1(&hello);
            h.read_as_v1()
        });
        assert!(refused.is_err(), "the session must not start");
        assert_told_to_update(&msgs);
    }

    /// A 0.1 headset that does not trust the PC yet asks to pair before saying hello:
    /// it is told to update instead of pairing in a protocol it does not speak.
    #[test]
    fn a_protocol_1_pairing_request_is_told_to_update() {
        let mut pc = TestPc::new("v1pair");
        let (refused, msgs) = connect(&mut pc, |h| {
            // PairBegin as 0.1.1 sends it: the headset shows the code, then its name;
            // right after it, its 33-byte SPAKE2 message, which the PC never reads.
            let mut begin = vec![109, 11, 0, 0, 0, 1];
            begin.extend_from_slice(b"Meta Quest");
            let (_, msg) = Pairing::start("123456");
            begin.extend_from_slice(&[110, 33, 0, 0, 0]);
            begin.extend_from_slice(&msg);
            h.write_as_v1(&begin);
            h.read_as_v1()
        });
        assert!(refused.is_err(), "no pairing and no session");
        assert_told_to_update(&msgs);
        assert!(pc.security.trusted.lock().unwrap().is_empty());
    }

    /// The tests that show a code in the tray, one at a time (the tray's code is one).
    static TRAY_TESTS: Mutex<()> = Mutex::new(());

    /// A connection that ends leaves the tray's code alone unless it showed it: one
    /// trusted headset connecting does not hide the code another is pairing with.
    #[test]
    fn only_the_connection_that_showed_the_code_hides_it() {
        let _tray = TRAY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let pairing = TrayCode::new(None);
        pairing.show("123 456".into());
        drop(TrayCode::new(None));
        assert!(pairing.shows(), "another connection's end hid the code");
        let later = TrayCode::new(None);
        later.show("654 321".into());
        drop(pairing);
        assert!(later.shows(), "an earlier connection hid the newer code");
        drop(later);
        assert_eq!(code_shown(), None);
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
            l.begin();
            l.failed();
            assert!(l.wait().is_none());
        }
        // Counted from its start: the pause holds while it runs.
        l.begin();
        let first = l.wait().expect("a pause after the free ones");
        assert!(first > Duration::from_secs(25) && first <= Duration::from_secs(30));
        // Its end does not count it again.
        l.failed();
        assert!(l.wait().unwrap() <= Duration::from_secs(30));
        l.begin();
        assert!(l.wait().unwrap() > first);
        for _ in 0..20 {
            l.begin();
        }
        assert!(l.wait().unwrap() <= Duration::from_secs(900));
        l.succeeded();
        assert!(l.wait().is_none());
    }

    /// A peer that trickles its Noise handshake (a long length, then a byte now and
    /// then, each within the read timeout) is cut off at the setup deadline: it cannot
    /// hold the PC, which accepts one connection at a time.
    #[test]
    fn a_trickling_peer_is_cut_off_in_time() {
        use std::io::Write;
        let pc = TestPc::new("trickle");
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(&[0xff, 0xff]).unwrap();
            while s.write_all(&[0]).is_ok() {
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let (s, _) = l.accept().unwrap();
        let start = Instant::now();
        let refused = accept_within(s, &pc.security, None, Duration::from_millis(500));
        assert!(refused.is_err(), "no session");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        peer.join().unwrap();
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
        pc.security
            .trusted
            .lock()
            .unwrap()
            .add(key, "test headset")
            .unwrap();
        let (refused, after) = connect(&mut pc, |h| {
            assert!(matches!(h.hello(PROTOCOL_VERSION), Answer::Welcome { .. }));
            h.send(ClientMsg::request(
                prostir_proto::state::ReqId(1),
                prostir_proto::request::Request::FindMouse,
            ));
            h.recv().map_err(|e| e.kind())
        });
        let err = refused.unwrap_err().to_string();
        assert!(err.contains("before the session"), "{err}");
        assert_eq!(
            after,
            Err(std::io::ErrorKind::UnexpectedEof),
            "closed, no snapshot"
        );
    }

    /// "Code on the PC": the headset asks first, the PC makes and shows its code, the
    /// user types it in the headset, and the headset is trusted afterwards.
    #[test]
    fn code_shown_on_the_pc_pairs() {
        let _tray = TRAY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let mut pc = TestPc::new("pcc");
        let (accepted, (ok, opened)) = connect(&mut pc, |h| {
            assert!(matches!(h.hello(PROTOCOL_VERSION), Answer::Welcome { .. }));
            h.send(ClientMsg::Open { parts: Vec::new() });
            assert_eq!(h.recv().unwrap(), HostMsg::PairingRequired);
            h.send(ClientMsg::PairBegin {
                headset_shows: false,
                name: "test headset".into(),
            });
            let HostMsg::PairMsg { msg: theirs } = h.recv().unwrap() else {
                panic!("expected PairMsg")
            };
            // What the user reads in the tray and types in the headset.
            let code = PC_CODE.lock().unwrap().clone().unwrap().0;
            let (p, msg) = Pairing::start(&code);
            h.send(ClientMsg::PairMsg { msg });
            let key = p.finish(&theirs).unwrap();
            let peer = h.link.peer.clone();
            h.send(ClientMsg::PairConfirm {
                tag: confirmation(&key, "headset", &peer, &h.id.public),
            });
            let HostMsg::PairResult { tag } = h.recv().unwrap() else {
                panic!("expected PairResult")
            };
            h.send(ClientMsg::Open { parts: Vec::new() });
            (tag.is_some(), h.recv().unwrap())
        });
        assert!(ok, "the codes must match");
        assert_eq!(opened, HostMsg::Snapshot { parts: Vec::new() });
        assert!(accepted.is_ok());
        assert_eq!(pc.security.trusted.lock().unwrap().len(), 1);
    }
}
