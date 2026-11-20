//! Commands for the headset from a named pipe, until `prostir ctl` comes (0.2.0 step
//! 6): with `PROSTIR_TEST_CTL=/path/to/fifo` the app reads one command a line from that
//! pipe and appends what came of each to `/path/to/fifo.replies`, as
//! `LINE => done`, `LINE => failed CODE: DETAIL` and so on. The end-to-end tests drive
//! the headset's commands with it, and it lets a person try them by hand.
//!
//! A line is `recenter`, `profile NAME`, `cinema on|off`, `focus on|off`,
//! `distance N`, `preset N`, `lineup`, `showmouse`, `follow on|off`, `reset`,
//! `nolimits on|off`, or `raw K` (a command of kind `K` with no fields, for
//! tests of kinds the headset does not know).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc;

use log::{info, warn};
use prostir_proto::request::{Asked, Command};

use crate::events::EventSink;
use crate::platform::TrayCommand;

/// Reads commands from the pipe `PROSTIR_TEST_CTL` names, if it is set.
pub fn spawn(events: EventSink) {
    let Some(path) = std::env::var_os("PROSTIR_TEST_CTL").map(PathBuf::from) else {
        return;
    };
    info!("commands for the headset from {}", path.display());
    let replies = {
        let mut p = path.clone().into_os_string();
        p.push(".replies");
        PathBuf::from(p)
    };
    let started = std::thread::Builder::new()
        .name("ctl-hook".into())
        .spawn(move || {
            loop {
                // Opening a pipe waits for a writer; at its end, the next one.
                let pipe = match std::fs::File::open(&path) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("{}: {e}", path.display());
                        return;
                    }
                };
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    let line = line.trim().to_owned();
                    if line.is_empty() {
                        continue;
                    }
                    let answer = match parse(&line) {
                        Some(asked) => {
                            let (tx, rx) = mpsc::channel();
                            let sent = events.tray(TrayCommand::Headset {
                                asked,
                                reply: Some(tx),
                            });
                            if !sent {
                                return;
                            }
                            rx.recv().unwrap_or_else(|_| "no answer".into())
                        }
                        None => "not a command".into(),
                    };
                    let written = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&replies)
                        .and_then(|mut f| writeln!(f, "{line} => {answer}"));
                    if let Err(e) = written {
                        warn!("{}: {e}", replies.display());
                    }
                }
            }
        });
    if let Err(e) = started {
        warn!(
            "no commands from {}: {e}",
            std::env::var("PROSTIR_TEST_CTL").unwrap_or_default()
        );
    }
}

/// The command a line names.
fn parse(line: &str) -> Option<Asked<Command>> {
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    let on = || match rest {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    };
    Some(Asked::Known(match word {
        "recenter" => Command::Recenter,
        "profile" if !rest.is_empty() => Command::Profile { name: rest.into() },
        "cinema" => Command::Cinema { on: on()? },
        "focus" => Command::FocusMode { on: on()? },
        "distance" => Command::Distance {
            steps: rest.parse().ok()?,
        },
        "preset" => Command::Preset {
            preset: rest.parse().ok()?,
        },
        "lineup" => Command::LineUp,
        "showmouse" => Command::ShowMouse,
        "follow" => Command::Follow { on: on()? },
        "reset" => Command::ResetArrangement,
        "nolimits" => Command::NoLimits { on: on()? },
        "raw" => {
            return Some(Asked::Unknown {
                kind: rest.parse().ok()?,
            });
        }
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_name_commands() {
        assert_eq!(parse("recenter"), Some(Asked::Known(Command::Recenter)));
        assert_eq!(
            parse("profile Робота"),
            Some(Asked::Known(Command::Profile {
                name: "Робота".into()
            }))
        );
        assert_eq!(
            parse("distance -2"),
            Some(Asked::Known(Command::Distance { steps: -2 }))
        );
        assert_eq!(
            parse("nolimits on"),
            Some(Asked::Known(Command::NoLimits { on: true }))
        );
        assert_eq!(parse("raw 200"), Some(Asked::Unknown { kind: 200 }));
        assert_eq!(parse("cinema maybe"), None);
        assert_eq!(parse("fly"), None);
        // The headset's microphone switch is the user's, in the headset.
        assert_eq!(parse("mic on"), None);
    }
}
