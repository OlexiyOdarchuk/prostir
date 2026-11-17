//! Encrypted connections and pairing on the PC side.

use crate::i18n::t;
use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{info, warn};
use prostir_proto::secure::{
    self, Identity, Pairing, SecureReader, SecureWriter, TrustStore, confirmation, tags_match,
};
use prostir_proto::{ClientMsg, HostMsg, PROTOCOL_VERSION};

use crate::tray;

/// Wrong codes allowed per connection before giving up.
const MAX_ATTEMPTS: u32 = 5;

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
    let (reader, mut writer, peer) = secure::handshake(stream, &security.identity, false)?;
    let mut reader = BufReader::new(reader);
    let mut attempts = 0;
    loop {
        match ClientMsg::read(&mut reader)? {
            ClientMsg::Hello { version } if version != PROTOCOL_VERSION => {
                bail!("headset speaks protocol v{version}, PC v{PROTOCOL_VERSION}")
            }
            ClientMsg::Hello { .. } if security.trusted.contains(&peer.key) => {
                HostMsg::Welcome {
                    version: PROTOCOL_VERSION,
                    language: crate::i18n::code().into(),
                }
                .write(&mut writer)?;
                writer.flush()?;
                stream.set_read_timeout(None)?;
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
                // The user needs time to read and type the code.
                stream.set_read_timeout(Some(Duration::from_secs(300)))?;
                let paired = match obtain_code(headset_shows, tray) {
                    Some(code) => pair(&mut reader, &mut writer, &security.identity, &peer, &code),
                    None => refuse(
                        &mut writer,
                        t("На ноутбуці код не введено", "No code was typed on the PC"),
                    ),
                };
                tray::set_pairing(tray, None);
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                match paired? {
                    true => {
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
                    false => warn!("pairing failed: the codes did not match"),
                }
            }
            other => bail!("unexpected message before pairing: {other:?}"),
        }
    }
}

/// The code for this attempt: typed on the PC (the headset shows it), or generated
/// here and shown in the tray and a notification (the headset asks for it).
fn obtain_code(headset_shows: bool, tray: Option<&tray::Handle>) -> Option<String> {
    if headset_shows {
        return ask_code();
    }
    let code = secure::pairing_code();
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
    Some(code)
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
fn ask_code() -> Option<String> {
    let text = t(
        "Шолом хоче підключитися.\nВведіть код, який він показує:",
        "A headset wants to connect.\nType the code it shows:",
    );
    let attempts: [(&str, Vec<&str>); 2] = [
        ("kdialog", vec!["--title", "Prostir", "--inputbox", text]),
        ("zenity", vec!["--entry", "--title=Prostir", "--text", text]),
    ];
    for (program, args) in attempts {
        match Command::new(program).args(&args).output() {
            Ok(out) if out.status.success() => {
                let code: String = String::from_utf8_lossy(&out.stdout)
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect();
                return (!code.is_empty()).then_some(code);
            }
            Ok(_) => return None, // cancelled
            Err(_) => continue,   // not installed
        }
    }
    warn!("neither kdialog nor zenity is installed: cannot ask for the pairing code");
    None
}

/// Desktop notification, best effort.
fn notify(title: &str, body: &str) {
    if cfg!(test) {
        return;
    }
    let _ = Command::new("notify-send")
        .args(["--app-name=Prostir", "--icon=prostir", title, body])
        .spawn();
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
}
