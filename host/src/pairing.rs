//! Encrypted connections and pairing on the PC side.

use crate::i18n::t;
use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{info, warn};
use prostir_proto::secure::{
    self, Identity, Pairing, SecureReader, SecureWriter, TrustStore, confirmation, tags_match,
};
use prostir_proto::{ClientMsg, HostMsg, PROTOCOL_VERSION};

use crate::tray;

/// Wrong codes allowed per connection before giving up.
const MAX_ATTEMPTS: u32 = 3;
/// Wrong codes allowed in a row (across connections) before pairing slows down.
const FREE_FAILURES: u32 = 3;
/// A code made on the PC stays the same for this long, however many attempts use it.
const CODE_LIFETIME: Duration = Duration::from_secs(300);
/// Encryption and hello must be done within this time, however slowly bytes trickle.
const SETUP_TIME: Duration = Duration::from_secs(10);
/// Time to read and type a code.
const PAIRING_TIME: Duration = Duration::from_secs(300);

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
    fn new(stream: &TcpStream, after: Duration) -> Result<Self> {
        let state = Arc::new((
            Mutex::new(Instant::now() + after),
            std::sync::Condvar::new(),
        ));
        let done = Arc::new(AtomicBool::new(false));
        let watched = stream.try_clone()?;
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
                        let _ = watched.shutdown(std::net::Shutdown::Both);
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
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
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

/// Encrypts the connection, pairs the headset if it is new, and returns the
/// encrypted halves once the headset is trusted and has said hello.
pub fn accept(
    stream: &TcpStream,
    security: &mut Security,
    tray: Option<&tray::Handle>,
) -> Result<(BufReader<SecureReader>, SecureWriter)> {
    stream.set_nodelay(true)?;
    // Over Wi-Fi a vanished headset would otherwise block writes for minutes and
    // keep a reconnecting headset waiting in the accept queue.
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let deadline = Deadline::new(stream, SETUP_TIME)?;
    // However this connection ends, the tray stops showing a code for it.
    let _clear_code = ClearTrayCode(tray);
    let (reader, mut writer, peer) = secure::handshake(stream, &security.identity, false)?;
    let mut reader = BufReader::new(reader);
    let mut attempts = 0;
    loop {
        match ClientMsg::read(&mut reader)? {
            ClientMsg::Hello { version, min, app } if !prostir_proto::compatible(version, min) => {
                incompatible(&mut writer, min > PROTOCOL_VERSION, &app)?;
                bail!("headset speaks protocol v{version} (app {app}), PC v{PROTOCOL_VERSION}")
            }
            ClientMsg::Hello { .. } if security.trusted.contains(&peer.key) => {
                HostMsg::Welcome {
                    version: PROTOCOL_VERSION,
                    language: crate::i18n::code().into(),
                }
                .write(&mut writer)?;
                writer.flush()?;
                stream.set_read_timeout(None)?;
                drop(deadline);
                return Ok((reader, writer));
            }
            ClientMsg::Hello { .. } => {
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
                stream.set_read_timeout(Some(PAIRING_TIME))?;
                deadline.extend(PAIRING_TIME);
                let paired = match obtain_code(headset_shows, tray) {
                    Ok(code) => pair(&mut reader, &mut writer, &security.identity, &peer, &code),
                    Err(NoCode::Cancelled) => refuse(
                        &mut writer,
                        t("На ПК код не введено", "No code was typed on the PC"),
                    ),
                    Err(NoCode::NoDialog) => refuse(
                        &mut writer,
                        t(
                            "На ПК немає вікна для коду. Виберіть «Навпаки: код на ноутбуці, ввести тут».",
                            "The PC has no dialog for the code. Choose \"The other way: code on the PC, type it here\".",
                        ),
                    ),
                };
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                deadline.extend(SETUP_TIME);
                match paired? {
                    true => {
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
                    false => {
                        warn!("pairing failed: the codes did not match");
                        LIMITER.lock().unwrap().failed();
                    }
                }
            }
            // Something a newer headset sends that this PC does not know yet.
            ClientMsg::Unknown { .. } => {}
            other => bail!("unexpected message before pairing: {other:?}"),
        }
    }
}

/// The code for this attempt: typed on the PC (the headset shows it), or generated
/// here and shown in the tray and a notification (the headset asks for it).
/// Why there is no code to pair with.
enum NoCode {
    Cancelled,
    /// None of kdialog, zenity, fuzzel, wofi, rofi is installed.
    NoDialog,
}

fn obtain_code(headset_shows: bool, tray: Option<&tray::Handle>) -> Result<String, NoCode> {
    if headset_shows {
        return ask_code();
    }
    let mut current = PC_CODE.lock().unwrap();
    let code = match &*current {
        Some((code, made)) if made.elapsed() < CODE_LIFETIME => code.clone(),
        _ => {
            let code = secure::pairing_code();
            *current = Some((code.clone(), Instant::now()));
            code
        }
    };
    drop(current);
    let shown = format!("{} {}", &code[..3], &code[3..]);
    info!("pairing code: {shown}");
    tray::set_pairing(tray, Some(shown.clone()));
    notify(
        t("Сполучення шолома", "Pairing a headset"),
        &format!(
            "{} {shown}",
            t("Введіть у шоломі код", "Type this code in the headset:")
        ),
    );
    Ok(code)
}

/// The headset speaks another protocol: tell both the headset and the person at the PC
/// which side needs the update.
fn incompatible(writer: &mut SecureWriter, pc_too_old: bool, headset_app: &str) -> Result<()> {
    let ours = env!("CARGO_PKG_VERSION");
    // Comes from a peer that has not proved anything yet: only a version-like string
    // goes into the notification.
    let headset_app: String = headset_app
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || ".+-".contains(*c))
        .take(24)
        .collect();
    let headset_app = if headset_app.is_empty() {
        t("старіша", "older")
    } else {
        headset_app.as_str()
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
    warn!("{title}: {body}");
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
    // Once per headset version, not on every reconnection attempt.
    static TOLD: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut told = TOLD.lock().unwrap();
    if !told.iter().any(|t| t == headset_app) {
        told.push(headset_app.to_owned());
        notify(title, &body);
    }
    Ok(())
}

fn refuse(writer: &mut SecureWriter, why: &str) -> Result<bool> {
    HostMsg::Error { text: why.into() }.write(writer)?;
    HostMsg::PairResult {
        ok: false,
        tag: Vec::new(),
    }
    .write(writer)?;
    writer.flush()?;
    Ok(false)
}

/// One pairing attempt with `code`; `Ok(false)` when the codes did not match.
fn pair(
    reader: &mut BufReader<SecureReader>,
    writer: &mut SecureWriter,
    identity: &Identity,
    peer: &secure::Peer,
    code: &str,
) -> Result<bool> {
    let (pairing, msg) = Pairing::start(code);
    HostMsg::PairMsg { msg }.write(writer)?;
    writer.flush()?;
    let theirs = match ClientMsg::read(reader)? {
        ClientMsg::PairMsg { msg } => msg,
        other => bail!("expected the headset's pairing message, got {other:?}"),
    };
    let key = pairing.finish(&theirs)?;
    let tag = match ClientMsg::read(reader)? {
        ClientMsg::PairConfirm { tag } => tag,
        other => bail!("expected a pairing confirmation, got {other:?}"),
    };
    let own = &identity.public;
    let ok = tags_match(&tag, &confirmation(&key, "headset", peer, own));
    HostMsg::PairResult {
        ok,
        tag: if ok {
            confirmation(&key, "pc", peer, own)
        } else {
            Vec::new()
        },
    }
    .write(writer)?;
    writer.flush()?;
    Ok(ok)
}

/// Asks for the code the headset shows, in a small dialog on the PC.
fn ask_code() -> Result<String, NoCode> {
    let text = t(
        "Шолом хоче підключитися.\nВведіть код, який він показує:",
        "A headset wants to connect.\nType the code it shows:",
    );
    // Launchers in dmenu mode (common on Hyprland and Sway) print what was typed; they
    // show one line, so the prompt is shorter.
    let prompt = t("Код із шолома: ", "Code from the headset: ");
    let attempts: [(&str, Vec<&str>); 5] = [
        ("kdialog", vec!["--title", "Prostir", "--inputbox", text]),
        ("zenity", vec!["--entry", "--title=Prostir", "--text", text]),
        ("fuzzel", vec!["--dmenu", "--lines=0", "--prompt", prompt]),
        ("wofi", vec!["--dmenu", "--lines=1", "--prompt", prompt]),
        ("rofi", vec!["-dmenu", "-l", "0", "-p", prompt]),
    ];
    for (program, args) in attempts {
        let child = Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            continue; // not installed
        };
        // Nobody at the PC: the dialog goes away rather than holding the PC.
        let until = Instant::now() + Duration::from_secs(120);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        if !status.is_some_and(|s| s.success()) {
            return Err(NoCode::Cancelled); // or timed out
        }
        let mut out = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            use std::io::Read;
            let _ = stdout.read_to_string(&mut out);
        }
        let code: String = out.chars().filter(char::is_ascii_digit).collect();
        return if code.is_empty() {
            Err(NoCode::Cancelled)
        } else {
            Ok(code)
        };
    }
    warn!(
        "no dialog program (kdialog, zenity, fuzzel, wofi, rofi): cannot ask for the pairing code"
    );
    Err(NoCode::NoDialog)
}

/// Desktop notification, best effort.
fn notify(title: &str, body: &str) {
    if cfg!(test) {
        return;
    }
    let child = Command::new("notify-send")
        .args(["--app-name=Prostir", "--icon=prostir", title, body])
        .spawn();
    // Reaped in the background, so finished notify-send processes do not pile up.
    if let Ok(mut child) = child {
        let _ = std::thread::Builder::new()
            .name("notify".into())
            .spawn(move || child.wait());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// The headset's side of a pairing attempt, as the Quest app does it.
    fn headset(stream: TcpStream, code: &str) -> bool {
        let dir =
            std::env::temp_dir().join(format!("prostir-pair-h-{}-{code}", std::process::id()));
        let id = Identity::load_or_create(&dir.join("key")).unwrap();
        let (r, mut w, peer) = secure::handshake(&stream, &id, true).unwrap();
        let mut r = BufReader::new(r);
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
        let HostMsg::PairResult { ok, tag } = HostMsg::read(&mut r).unwrap() else {
            panic!("expected PairResult")
        };
        std::fs::remove_dir_all(dir).unwrap();
        ok && tags_match(&tag, &confirmation(&key, "pc", &peer, &id.public))
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
        let (r, mut w, peer) = secure::handshake(&s, &id, false).unwrap();
        let pc = pair(&mut BufReader::new(r), &mut w, &id, &peer, pc_code).unwrap();
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

    /// A headset from another protocol version learns which side to update.
    #[test]
    fn other_protocol_is_told_to_update() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let stream = TcpStream::connect(addr).unwrap();
            let dir = std::env::temp_dir().join(format!("prostir-ver-h-{}", std::process::id()));
            let id = Identity::load_or_create(&dir.join("key")).unwrap();
            let (r, mut w, _) = secure::handshake(&stream, &id, true).unwrap();
            let mut r = BufReader::new(r);
            ClientMsg::Hello {
                version: PROTOCOL_VERSION + 1,
                min: PROTOCOL_VERSION + 1,
                app: "9.9.9".into(),
            }
            .write(&mut w)
            .unwrap();
            let first = HostMsg::read(&mut r).unwrap();
            let second = HostMsg::read(&mut r).unwrap();
            std::fs::remove_dir_all(dir).unwrap();
            (first, second)
        });
        let (s, _) = l.accept().unwrap();
        let dir = std::env::temp_dir().join(format!("prostir-ver-p-{}", std::process::id()));
        let mut security = Security {
            identity: Identity::load_or_create(&dir.join("key")).unwrap(),
            trusted: TrustStore::load(&dir.join("trusted")),
        };
        let refused = accept(&s, &mut security, None);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(refused.is_err(), "the session must not start");
        let (first, second) = h.join().unwrap();
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

    /// "Code on the PC": the headset asks first, the PC makes and shows its code, the
    /// user types it in the headset, and the headset is trusted afterwards.
    #[test]
    fn code_shown_on_the_pc_pairs() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let stream = TcpStream::connect(addr).unwrap();
            let dir = std::env::temp_dir().join(format!("prostir-pcc-h-{}", std::process::id()));
            let id = Identity::load_or_create(&dir.join("key")).unwrap();
            let (r, mut w, peer) = secure::handshake(&stream, &id, true).unwrap();
            let mut r = BufReader::new(r);
            let hello = || ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                min: prostir_proto::MIN_PROTOCOL,
                app: "test".into(),
            };
            hello().write(&mut w).unwrap();
            assert_eq!(HostMsg::read(&mut r).unwrap(), HostMsg::PairingRequired);
            ClientMsg::PairBegin {
                headset_shows: false,
                name: "test headset".into(),
            }
            .write(&mut w)
            .unwrap();
            let HostMsg::PairMsg { msg: theirs } = HostMsg::read(&mut r).unwrap() else {
                panic!("expected PairMsg")
            };
            // What the user reads in the tray and types in the headset.
            let code = PC_CODE.lock().unwrap().clone().unwrap().0;
            let (p, msg) = Pairing::start(&code);
            ClientMsg::PairMsg { msg }.write(&mut w).unwrap();
            let key = p.finish(&theirs).unwrap();
            ClientMsg::PairConfirm {
                tag: confirmation(&key, "headset", &peer, &id.public),
            }
            .write(&mut w)
            .unwrap();
            let HostMsg::PairResult { ok, .. } = HostMsg::read(&mut r).unwrap() else {
                panic!("expected PairResult")
            };
            hello().write(&mut w).unwrap();
            let welcome = HostMsg::read(&mut r).unwrap();
            std::fs::remove_dir_all(dir).unwrap();
            (ok, welcome)
        });
        let (s, _) = l.accept().unwrap();
        let dir = std::env::temp_dir().join(format!("prostir-pcc-p-{}", std::process::id()));
        let mut security = Security {
            identity: Identity::load_or_create(&dir.join("key")).unwrap(),
            trusted: TrustStore::load(&dir.join("trusted")),
        };
        let accepted = accept(&s, &mut security, None);
        let trusted = security.trusted.len();
        std::fs::remove_dir_all(dir).unwrap();
        let (ok, welcome) = h.join().unwrap();
        assert!(ok, "the codes must match");
        assert!(matches!(welcome, HostMsg::Welcome { .. }));
        assert!(accepted.is_ok());
        assert_eq!(trusted, 1);
    }
}
