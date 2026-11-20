//! `prostir ctl`: the local API from a terminal, a script or a hot key, and `prostir
//! open` (the menu entry). Every command is one request to the running Prostir (see
//! `crate::api`) through a [`CtlClient`]: D-Bus on Linux, a named pipe on Windows
//! later.
//!
//! Without `--json` the result is one line for people; with it, one JSON object (one
//! a line for `watch`). Exit codes: [`DONE`], [`FAILED`], [`NOT_RUNNING`],
//! [`TIMED_OUT`], [`USAGE`].

use std::time::{Duration, Instant};

use crate::api::{
    self, Answer, ApiError, CALL_SECONDS, CodecChoice, DisplayState, Failure, Request, State,
};
use crate::i18n::t;

/// Done.
pub const DONE: i32 = 0;
/// It did not happen (the error's name is in the text or the JSON).
pub const FAILED: i32 = 1;
/// Prostir is not running, or no headset is connected for a command for the headset.
pub const NOT_RUNNING: i32 = 2;
/// No answer in time.
pub const TIMED_OUT: i32 = 3;
/// Arguments that do not read (as `EX_USAGE`).
pub const USAGE: i32 = 64;

/// How long `ctl` waits for a reply: a command for the headset takes up to 30 s.
pub const WAIT: Duration = Duration::from_secs(CALL_SECONDS);

/// What a client sees while it watches.
#[derive(Debug, Clone, PartialEq)]
pub enum Watched {
    State(u32, String),
    Event(api::Event),
    /// Prostir came on the bus.
    Appeared,
    /// Prostir left the bus.
    Vanished,
}

/// Why a client got no answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    /// No bus (or pipe) to talk on.
    NoBus(String),
    /// Prostir is not running.
    NotRunning,
    /// Prostir said no.
    Refused(Failure),
    /// No answer in time.
    TimedOut,
    /// An answer that does not read, or the connection broke.
    Broken(String),
}

/// The way to the running Prostir.
pub trait CtlClient {
    fn call(&self, request: &Request) -> Result<Answer, ClientError>;
    /// The whole state: its revision and its JSON.
    fn state(&self) -> Result<(u32, String), ClientError>;
    /// Whether Prostir is there now.
    fn running(&self) -> bool;
    /// Calls `each` with what happens until it returns `false`.
    fn watch(&self, each: &mut dyn FnMut(Watched) -> bool) -> Result<(), ClientError>;
}

/// What `ctl` was asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Ctl {
    Ask(Request),
    Status,
    Watch,
    Monitors,
    Headsets,
    AutostartStatus,
    Help,
}

/// The arguments after `ctl`, and whether `--json` was among them.
pub fn parse(args: &[String]) -> Result<(Ctl, bool), String> {
    let json = args.iter().any(|a| a == "--json");
    let cpu = args.iter().any(|a| a == "--cpu");
    let all = args.iter().any(|a| a == "--all");
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !matches!(*a, "--json" | "--cpu" | "--all"))
        .collect();
    let unexpected = |w: &str| format!("unexpected {w:?}");
    let on = |w: Option<&&str>| match w.copied() {
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => Err(format!("expected on or off, not {other:?}")),
        None => Err("expected on or off".to_owned()),
    };
    let number = |w: Option<&&str>, what: &str| -> Result<u32, String> {
        let w = w.ok_or_else(|| format!("expected {what}"))?;
        w.parse()
            .map_err(|_| format!("{what}: not a number: {w:?}"))
    };
    let ask = |r: Request| Ok(Ctl::Ask(r));
    let (first, rest) = match words.split_first() {
        Some((first, rest)) => (*first, rest),
        None => return Ok((Ctl::Help, json)),
    };
    // Each takes as many words as it needs; more is a mistake.
    let mut takes = 0;
    let result: Result<Ctl, String> = match first {
        "help" | "--help" | "-h" => {
            takes = rest.len();
            Ok(Ctl::Help)
        }
        "status" => Ok(Ctl::Status),
        "watch" => Ok(Ctl::Watch),
        "recenter" => ask(Request::Recenter),
        "lineup" => ask(Request::LineUp),
        "showmouse" => ask(Request::ShowMouse),
        "reset" => ask(Request::ResetArrangement),
        "profile" => {
            takes = rest.len();
            let name = rest.join(" ");
            if name.is_empty() {
                Err("expected a profile's name".to_owned())
            } else {
                ask(Request::Profile { name })
            }
        }
        "cinema" | "focus" | "follow" | "mic" | "nolimits" => {
            takes = 1;
            let on = on(rest.first())?;
            ask(match first {
                "cinema" => Request::Cinema { on },
                "focus" => Request::Focus { on },
                "follow" => Request::Follow { on },
                "mic" => Request::SetMic { on },
                _ => Request::NoLimits { on },
            })
        }
        "closer" | "farther" => {
            takes = rest.len().min(1);
            let n = match rest.first() {
                Some(n) => n
                    .parse::<i8>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("steps: 1 to 127, not {n:?}"))?,
                None => 1,
            };
            let steps = if first == "closer" { -n } else { n };
            ask(Request::Distance { steps })
        }
        "preset" => {
            takes = 1;
            let n = number(rest.first(), "a preset")?;
            let preset = u8::try_from(n).map_err(|_| format!("preset: 0 to 255, not {n}"))?;
            ask(Request::Preset { preset })
        }
        "monitor" => match rest.first().copied() {
            Some("list") => {
                takes = 1;
                Ok(Ctl::Monitors)
            }
            Some("add") => {
                takes = 2;
                let (w, h, scale) = size(rest.get(1).copied().ok_or("expected WxH")?)?;
                ask(Request::AddMonitor { w, h, scale })
            }
            Some(word @ ("remove" | "retry" | "primary")) => {
                takes = 2;
                let id = number(rest.get(1), "a display's id")?;
                ask(match word {
                    "remove" => Request::RemoveMonitor { id },
                    "retry" => Request::RetryMonitor { id },
                    _ => Request::SetPrimary { id },
                })
            }
            _ => Err("expected monitor list, add WxH, remove ID, retry ID or primary ID".into()),
        },
        "screen" => {
            takes = 2;
            let shown = match rest.first().copied() {
                Some("show") => true,
                Some("hide") => false,
                _ => return Err("expected screen show ID or screen hide ID".into()),
            };
            let id = number(rest.get(1), "a display's id")?;
            ask(Request::ShowPhysical { id, shown })
        }
        "quality" => {
            takes = 1;
            let mbit = number(rest.first(), "Mbit/s")?;
            ask(Request::SetQuality { mbit })
        }
        "sharp" => {
            takes = 1;
            ask(Request::SetSharp {
                on: on(rest.first())?,
            })
        }
        "codec" => {
            takes = 1;
            let codec = match rest.first().copied() {
                Some("h264") => CodecChoice::H264,
                Some("h265") => CodecChoice::H265,
                Some("auto") => CodecChoice::Auto,
                _ => return Err("expected codec h264, h265 or auto".into()),
            };
            ask(Request::SetCodec { codec, cpu })
        }
        "sound" => {
            takes = 1;
            match rest.first().copied() {
                Some("headset") => ask(Request::SoundToHeadset { on: true }),
                Some("pc") => ask(Request::SoundToHeadset { on: false }),
                _ => Err("expected sound headset or sound pc".into()),
            }
        }
        "pair" => match rest.first().copied() {
            None => ask(Request::PairNewHeadset),
            Some("stop") => {
                takes = 1;
                ask(Request::StopPairing)
            }
            Some(other) => Err(unexpected(other)),
        },
        "headsets" => Ok(Ctl::Headsets),
        "forget" if all => ask(Request::ForgetAll),
        "forget" => {
            takes = rest.len();
            let headset = rest.join(" ");
            if headset.is_empty() {
                Err("expected a headset (its key or name) or --all".to_owned())
            } else {
                ask(Request::ForgetHeadset { headset })
            }
        }
        "autostart" => {
            takes = 1;
            match rest.first().copied() {
                Some("status") => Ok(Ctl::AutostartStatus),
                w => ask(Request::Autostart {
                    on: on(w.as_ref())?,
                }),
            }
        }
        "count" => {
            takes = 1;
            ask(Request::SetCounting {
                on: on(rest.first())?,
            })
        }
        "settings" => ask(Request::ShowSettings),
        "quit" => ask(Request::Quit),
        other => Err(format!("no command {other:?}")),
    };
    let ctl = result?;
    if (cpu && first != "codec") || (all && first != "forget") {
        return Err("--cpu goes with codec, --all with forget".into());
    }
    if let Some(extra) = rest.get(takes) {
        return Err(unexpected(extra));
    }
    Ok((ctl, json))
}

/// `2560x1440`, `2560x1440@1.25` or `2560x1440@125%`: the size and the scale in
/// percent.
fn size(text: &str) -> Result<(u32, u32, u16), String> {
    let bad = || format!("expected WxH, WxH@1.25 or WxH@125%, not {text:?}");
    let (size, scale) = match text.split_once('@') {
        Some((size, scale)) => {
            let percent = match scale.strip_suffix('%') {
                Some(p) => p.parse::<f64>().map_err(|_| bad())?,
                None => scale.parse::<f64>().map_err(|_| bad())? * 100.0,
            };
            (size, percent.round())
        }
        None => (text, 100.0),
    };
    let (w, h) = size.split_once(['x', 'X']).ok_or_else(bad)?;
    let w = w.parse().map_err(|_| bad())?;
    let h = h.parse().map_err(|_| bad())?;
    if !(1.0..=1000.0).contains(&scale) {
        return Err(bad());
    }
    Ok((w, h, scale as u16))
}

const HELP: &str = "\
prostir ctl COMMAND [--json]

  status                         what Prostir is doing
  watch                          what happens, a line each, until Ctrl+C
  recenter | lineup | showmouse | reset
  profile NAME
  cinema | focus | follow | mic | nolimits  on|off
  closer [N] | farther [N]
  preset N
  monitor list | add WxH[@1.25|@125%] | remove ID | retry ID | primary ID
  screen show ID | screen hide ID
  quality MBIT | sharp on|off | codec h264|h265|auto [--cpu]
  sound headset|pc
  pair | pair stop
  headsets | forget HEADSET | forget --all
  autostart on|off|status
  count on|off
  settings
  quit

Exit codes: 0 done, 1 failed, 2 Prostir not running or no headset connected,
3 no answer in time, 64 wrong arguments.
";

/// Where `ctl` writes: the lines for people and JSON to `out`, problems to `err`.
pub struct Output<'a> {
    pub out: &'a mut dyn std::io::Write,
    pub err: &'a mut dyn std::io::Write,
}

/// Runs `ctl` with `args` against `client`: the exit code.
pub fn run(args: &[String], client: Result<&dyn CtlClient, ClientError>, io: &mut Output) -> i32 {
    let (ctl, json) = match parse(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            let _ = writeln!(io.err, "prostir ctl: {e}\n\n{HELP}");
            return USAGE;
        }
    };
    if ctl == Ctl::Help {
        let _ = write!(io.out, "{HELP}");
        return DONE;
    }
    let client = match client {
        Ok(c) => c,
        Err(e) => return failed(&e, json, io, false),
    };
    match ctl {
        Ctl::Help => DONE,
        Ctl::Ask(request) => {
            let for_headset = request.for_headset();
            match client.call(&request) {
                Ok(answer) => {
                    if json {
                        let _ = writeln!(io.out, "{}", answer.to_json());
                    } else {
                        let _ = writeln!(io.out, "{}", done_line(&answer));
                    }
                    DONE
                }
                Err(e) => failed(&e, json, io, for_headset),
            }
        }
        Ctl::Status | Ctl::Monitors | Ctl::Headsets | Ctl::AutostartStatus => {
            match client.state() {
                Ok((revision, text)) => show_state(&ctl, revision, &text, json, io),
                Err(ClientError::NotRunning) if ctl == Ctl::Status => {
                    if json {
                        let _ = writeln!(io.out, r#"{{"running":false}}"#);
                    } else {
                        let _ = writeln!(io.out, "{}", t("Не працює", "Not running"));
                    }
                    NOT_RUNNING
                }
                Err(e) => failed(&e, json, io, false),
            }
        }
        Ctl::Watch => watch(client, json, io),
    }
}

/// The exit code for `e`, saying why.
fn failed(e: &ClientError, json: bool, io: &mut Output, for_headset: bool) -> i32 {
    let (code, text) = match e {
        ClientError::NoBus(why) => (
            NOT_RUNNING,
            format!("{}: {why}", t("Немає сеансової шини", "No session bus")),
        ),
        ClientError::NotRunning => (
            NOT_RUNNING,
            t("Prostir не працює", "Prostir is not running").to_owned(),
        ),
        ClientError::TimedOut => (
            TIMED_OUT,
            t("Немає відповіді вчасно", "No answer in time").to_owned(),
        ),
        ClientError::Broken(why) => (FAILED, why.clone()),
        ClientError::Refused(f) => {
            let code = match f.error {
                ApiError::TimedOut => TIMED_OUT,
                ApiError::NotConnected if for_headset => NOT_RUNNING,
                _ => FAILED,
            };
            if json {
                let answer = Answer {
                    error: Some(f.error),
                    text: Some(f.text.clone()),
                    ..Answer::default()
                };
                let _ = writeln!(io.out, "{}", answer.to_json());
            }
            (code, f.to_string())
        }
    };
    let _ = writeln!(io.err, "prostir ctl: {text}");
    code
}

/// The line for people when a request was done.
fn done_line(answer: &Answer) -> String {
    match (answer.id, answer.seconds_left) {
        (Some(id), _) => format!("{} {id}", t("Додано, номер", "Added, id")),
        (_, Some(left)) => format!(
            "{} {left} {}",
            t("Сполучення відкрите ще", "Pairing is open for"),
            t("с", "s")
        ),
        _ => t("Зроблено", "Done").to_owned(),
    }
}

fn show_state(ctl: &Ctl, revision: u32, text: &str, json: bool, io: &mut Output) -> i32 {
    let state = match State::from_json(text) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(io.err, "prostir ctl: {e}");
            return FAILED;
        }
    };
    let out = &mut io.out;
    let _ = match (ctl, json) {
        (Ctl::Status, true) => writeln!(out, "{text}"),
        (Ctl::Status, false) => writeln!(out, "{}", status_line(&state)),
        (Ctl::Monitors, true) => writeln!(out, "{}", to_json(&state.displays)),
        (Ctl::Monitors, false) => {
            for d in &state.displays {
                let _ = writeln!(out, "{}", display_line(d, state.primary));
            }
            Ok(())
        }
        (Ctl::Headsets, true) => writeln!(out, "{}", to_json(&state.headsets)),
        (Ctl::Headsets, false) => {
            for h in &state.headsets {
                let _ = writeln!(out, "{}", headset_line(h));
            }
            Ok(())
        }
        (_, true) => writeln!(
            out,
            r#"{{"autostart":{},"revision":{revision}}}"#,
            to_json(&state.autostart)
        ),
        (_, false) => writeln!(
            out,
            "{}",
            match state.autostart {
                Some(true) => t("увімкнено", "on"),
                Some(false) => t("вимкнено", "off"),
                None => t("недоступно", "not available"),
            }
        ),
    };
    DONE
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// One line on what Prostir is doing.
pub fn status_line(state: &State) -> String {
    let monitors = state
        .displays
        .iter()
        .filter(|d| d.kind == api::DisplayKind::Virtual)
        .count();
    match (state.connected(), state.pc.phase) {
        (Some((headset, session)), _) => {
            let via = match session.via {
                api::Via::Usb => "USB",
                _ => "Wi-Fi",
            };
            let worn = if session.worn {
                ""
            } else {
                t(", знято", ", taken off")
            };
            format!(
                "{} {} ({via}{worn}), {}: {monitors}",
                t("Підключено", "Connected:"),
                headset.name,
                t("моніторів", "monitors"),
            )
        }
        (None, api::Phase::Lingering) => format!(
            "{}: {monitors}",
            t(
                "Чекає на шолом, монітори лишаються",
                "Waiting for the headset, monitors kept"
            )
        ),
        _ => t("Чекає на шолом", "Waiting for a headset").to_owned(),
    }
}

fn display_line(d: &api::Display, primary: Option<u32>) -> String {
    let kind = match d.kind {
        api::DisplayKind::Virtual => t("монітор", "monitor"),
        api::DisplayKind::Physical => t("екран ПК", "PC display"),
        api::DisplayKind::Window => t("вікно", "window"),
        api::DisplayKind::Other => "",
    };
    let state = match d.state {
        DisplayState::Starting => t("запускається", "starting"),
        DisplayState::Running => t("працює", "running"),
        DisplayState::Failed => t("не вдалося", "failed"),
        DisplayState::Absent => t("немає", "absent"),
        DisplayState::Other => "",
    };
    let mut line = format!(
        "{}\t{}\t{kind}\t{}x{} {}%\t{state}",
        d.id, d.name, d.mode.w, d.mode.h, d.mode.scale
    );
    if d.kind == api::DisplayKind::Physical {
        line.push_str(if d.shown {
            t(", показано", ", shown")
        } else {
            t(", приховано", ", hidden")
        });
    }
    if primary == Some(d.id) {
        line.push_str(t(", головний", ", primary"));
    }
    if let Some(f) = &d.failure {
        line.push_str(&format!(": {}", f.text));
    }
    line
}

fn headset_line(h: &api::Headset) -> String {
    let role = match h.role {
        api::Role::Guest => t("гість", "guest"),
        _ => t("головний", "main"),
    };
    let connected = if h.session.is_some() {
        t(", підключено", ", connected")
    } else {
        ""
    };
    format!("{}\t{}\t{role}{connected}", h.key, h.name)
}

/// `watch`: a line for each event and each change of what `status` says, until
/// stopped; while Prostir is not running it says so and waits for it.
fn watch(client: &dyn CtlClient, json: bool, io: &mut Output) -> i32 {
    let mut said = String::new();
    let say = |io: &mut Output, line: String| {
        let _ = writeln!(io.out, "{line}");
        let _ = io.out.flush();
    };
    let waiting = t(
        "Prostir не працює, чекаю",
        "Prostir is not running, waiting",
    );
    if !client.running() {
        let line = if json {
            r#"{"event":"vanished"}"#.to_owned()
        } else {
            waiting.to_owned()
        };
        say(io, line);
    } else if let Ok((revision, text)) = client.state() {
        let line = state_line(revision, &text, json, &mut said);
        if let Some(line) = line {
            say(io, line);
        }
    }
    let result = client.watch(&mut |seen| {
        let line = match seen {
            Watched::State(revision, text) => state_line(revision, &text, json, &mut said),
            Watched::Event(event) if json => Some(to_json(&event)),
            Watched::Event(event) => event_line(&event),
            Watched::Appeared if json => Some(r#"{"event":"appeared"}"#.to_owned()),
            Watched::Appeared => Some(t("Prostir працює", "Prostir is running").to_owned()),
            Watched::Vanished if json => Some(r#"{"event":"vanished"}"#.to_owned()),
            Watched::Vanished => {
                said.clear();
                Some(waiting.to_owned())
            }
        };
        if let Some(line) = line {
            say(io, line);
        }
        true
    });
    match result {
        Ok(()) => DONE,
        Err(e) => failed(&e, json, io, false),
    }
}

/// What a new state shows: with `--json` the whole of it, else the status line when
/// it is not the one said last.
fn state_line(revision: u32, text: &str, json: bool, said: &mut String) -> Option<String> {
    if json {
        return Some(format!(
            r#"{{"event":"state_changed","revision":{revision},"state":{text}}}"#
        ));
    }
    let line = status_line(&State::from_json(text).ok()?);
    if line == *said {
        return None;
    }
    *said = line.clone();
    Some(line)
}

fn event_line(event: &api::Event) -> Option<String> {
    Some(match event {
        api::Event::HeadsetConnected { headset, name, via } => format!(
            "{} {name} ({headset}, {})",
            t("Шолом підключився:", "Headset connected:"),
            via.name()
        ),
        api::Event::HeadsetDisconnected { headset, reason } => format!(
            "{} {headset} ({})",
            t("Шолом відключився:", "Headset disconnected:"),
            reason.name()
        ),
        api::Event::HeadsetWorn { headset, worn } => format!(
            "{} {headset}",
            if *worn {
                t("Шолом вдягнули:", "Headset put on:")
            } else {
                t("Шолом зняли:", "Headset taken off:")
            }
        ),
        api::Event::Other => return None,
    })
}

/// `prostir open` (the menu entry): the settings window, with Prostir started first
/// if it is not running. `start` starts it; the window comes from Prostir.
pub fn open(client: &dyn CtlClient, start: &dyn Fn() -> std::io::Result<()>) -> i32 {
    if !client.running() {
        if let Err(e) = start() {
            eprintln!("prostir open: {e}");
            return NOT_RUNNING;
        }
        let until = Instant::now() + Duration::from_secs(10);
        while !client.running() {
            if Instant::now() >= until {
                eprintln!(
                    "prostir open: {}",
                    t(
                        "Prostir не запустився за 10 с",
                        "Prostir did not start within 10 s"
                    )
                );
                return NOT_RUNNING;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    match client.call(&Request::ShowSettings) {
        Ok(_) => DONE,
        // Prostir runs, which is what matters without the window.
        Err(ClientError::Refused(f)) => {
            eprintln!("prostir open: {f}");
            DONE
        }
        Err(ClientError::TimedOut) => TIMED_OUT,
        Err(_) => NOT_RUNNING,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    fn asks(line: &str) -> Request {
        match parse(&args(line)) {
            Ok((Ctl::Ask(r), _)) => r,
            other => panic!("{line}: {other:?}"),
        }
    }

    /// Every command reads as its request.
    #[test]
    fn commands_read_as_requests() {
        assert_eq!(asks("recenter"), Request::Recenter);
        assert_eq!(asks("lineup"), Request::LineUp);
        assert_eq!(asks("showmouse"), Request::ShowMouse);
        assert_eq!(asks("reset"), Request::ResetArrangement);
        assert_eq!(
            asks("profile Робота вдома"),
            Request::Profile {
                name: "Робота вдома".into()
            }
        );
        assert_eq!(asks("cinema on"), Request::Cinema { on: true });
        assert_eq!(asks("focus off"), Request::Focus { on: false });
        assert_eq!(asks("follow on"), Request::Follow { on: true });
        assert_eq!(asks("mic off"), Request::SetMic { on: false });
        assert_eq!(asks("nolimits on"), Request::NoLimits { on: true });
        assert_eq!(asks("closer"), Request::Distance { steps: -1 });
        assert_eq!(asks("closer 3"), Request::Distance { steps: -3 });
        assert_eq!(asks("farther 2"), Request::Distance { steps: 2 });
        assert_eq!(asks("preset 4"), Request::Preset { preset: 4 });
        let monitor = |w, h, scale| Request::AddMonitor { w, h, scale };
        assert_eq!(asks("monitor add 2560x1440"), monitor(2560, 1440, 100));
        assert_eq!(asks("monitor add 2560x1440@1.25"), monitor(2560, 1440, 125));
        assert_eq!(asks("monitor add 1920X1080@150%"), monitor(1920, 1080, 150));
        assert_eq!(
            asks("monitor remove 103"),
            Request::RemoveMonitor { id: 103 }
        );
        assert_eq!(asks("monitor retry 103"), Request::RetryMonitor { id: 103 });
        assert_eq!(asks("monitor primary 103"), Request::SetPrimary { id: 103 });
        assert_eq!(
            asks("screen show 104"),
            Request::ShowPhysical {
                id: 104,
                shown: true
            }
        );
        assert_eq!(
            asks("screen hide 104"),
            Request::ShowPhysical {
                id: 104,
                shown: false
            }
        );
        assert_eq!(asks("quality 50"), Request::SetQuality { mbit: 50 });
        assert_eq!(asks("sharp on"), Request::SetSharp { on: true });
        assert_eq!(
            asks("codec h264 --cpu"),
            Request::SetCodec {
                codec: CodecChoice::H264,
                cpu: true
            }
        );
        assert_eq!(
            asks("codec auto"),
            Request::SetCodec {
                codec: CodecChoice::Auto,
                cpu: false
            }
        );
        assert_eq!(asks("sound pc"), Request::SoundToHeadset { on: false });
        assert_eq!(asks("sound headset"), Request::SoundToHeadset { on: true });
        assert_eq!(asks("pair"), Request::PairNewHeadset);
        assert_eq!(asks("pair stop"), Request::StopPairing);
        assert_eq!(
            asks("forget Quest 3"),
            Request::ForgetHeadset {
                headset: "Quest 3".into()
            }
        );
        assert_eq!(asks("forget --all"), Request::ForgetAll);
        assert_eq!(asks("autostart on"), Request::Autostart { on: true });
        assert_eq!(asks("count off"), Request::SetCounting { on: false });
        assert_eq!(asks("settings"), Request::ShowSettings);
        assert_eq!(asks("quit"), Request::Quit);
    }

    /// The commands that only read, `--json` anywhere, and help.
    #[test]
    fn reading_commands_and_flags() {
        let read = |line: &str| parse(&args(line)).unwrap();
        assert_eq!(read("status"), (Ctl::Status, false));
        assert_eq!(read("status --json"), (Ctl::Status, true));
        assert_eq!(read("--json watch"), (Ctl::Watch, true));
        assert_eq!(read("monitor list"), (Ctl::Monitors, false));
        assert_eq!(read("headsets --json"), (Ctl::Headsets, true));
        assert_eq!(read("autostart status"), (Ctl::AutostartStatus, false));
        assert_eq!(read(""), (Ctl::Help, false));
        assert_eq!(read("--help"), (Ctl::Help, false));
    }

    /// What does not read says why.
    #[test]
    fn wrong_arguments() {
        for line in [
            "fly",
            "cinema",
            "cinema maybe",
            "recenter now",
            "closer 0",
            "closer -1",
            "farther 200",
            "preset 300",
            "monitor add big",
            "monitor add 2560x1440@x",
            "monitor remove",
            "monitor remove one",
            "screen show",
            "quality fast",
            "codec vp9",
            "sound loud",
            "pair now",
            "forget",
            "profile",
            "status --cpu",
            "recenter --all",
            "autostart maybe",
        ] {
            assert!(parse(&args(line)).is_err(), "{line}");
        }
    }

    /// A client that answers as a test says and keeps what it was asked.
    struct Fake {
        call: Result<Answer, ClientError>,
        state: Result<(u32, String), ClientError>,
        asked: RefCell<Vec<Request>>,
    }

    impl Fake {
        fn answering(call: Result<Answer, ClientError>) -> Self {
            Self {
                call,
                state: Err(ClientError::NotRunning),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl CtlClient for Fake {
        fn call(&self, request: &Request) -> Result<Answer, ClientError> {
            self.asked.borrow_mut().push(request.clone());
            self.call.clone()
        }

        fn state(&self) -> Result<(u32, String), ClientError> {
            self.state.clone()
        }

        fn running(&self) -> bool {
            self.state.is_ok()
        }

        fn watch(&self, _each: &mut dyn FnMut(Watched) -> bool) -> Result<(), ClientError> {
            Ok(())
        }
    }

    fn ctl(line: &str, client: &Fake) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            &args(line),
            Ok(client),
            &mut Output {
                out: &mut out,
                err: &mut err,
            },
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn refused(error: ApiError) -> Result<Answer, ClientError> {
        Err(ClientError::Refused(Failure::new(error, "why")))
    }

    /// Exit codes for each way a request ends.
    #[test]
    fn exit_codes() {
        let done = Fake::answering(Ok(Answer {
            id: Some(7),
            revision: 2,
            ..Answer::default()
        }));
        let (code, out, _) = ctl("monitor add 800x600 --json", &done);
        assert_eq!((code, out.trim()), (DONE, r#"{"id":7,"revision":2}"#));
        assert_eq!(
            done.asked.borrow()[0],
            Request::AddMonitor {
                w: 800,
                h: 600,
                scale: 100
            }
        );
        let (code, _, err) = ctl("recenter", &Fake::answering(refused(ApiError::NotAllowed)));
        assert_eq!(code, FAILED);
        assert!(err.contains("NotAllowed: why"), "{err}");
        let (code, out, _) = ctl(
            "recenter --json",
            &Fake::answering(refused(ApiError::NotConnected)),
        );
        assert_eq!(
            code, NOT_RUNNING,
            "no headset for a command for the headset"
        );
        assert_eq!(
            out.trim(),
            r#"{"error":"NotConnected","text":"why","revision":0}"#
        );
        let not_connected = Fake::answering(refused(ApiError::NotConnected));
        assert_eq!(ctl("quality 30", &not_connected).0, FAILED);
        assert_eq!(
            ctl("lineup", &Fake::answering(refused(ApiError::TimedOut))).0,
            TIMED_OUT
        );
        assert_eq!(
            ctl("lineup", &Fake::answering(Err(ClientError::TimedOut))).0,
            TIMED_OUT
        );
        assert_eq!(
            ctl("lineup", &Fake::answering(Err(ClientError::NotRunning))).0,
            NOT_RUNNING
        );
        let (code, out, _) = ctl("status --json", &Fake::answering(Ok(Answer::default())));
        assert_eq!((code, out.trim()), (NOT_RUNNING, r#"{"running":false}"#));
        let (code, _, err) = ctl("cinema maybe", &done);
        assert_eq!(code, USAGE);
        assert!(err.contains("on or off"), "{err}");
    }

    /// `status` and `monitor list` from the state.
    #[test]
    fn state_for_people() {
        let state = State {
            v: 1,
            displays: vec![api::Display {
                id: 103,
                name: "Prostir-1".into(),
                mode: api::Mode {
                    w: 640,
                    h: 360,
                    scale: 100,
                },
                state: DisplayState::Running,
                ..api::Display::default()
            }],
            primary: Some(103),
            ..State::default()
        };
        let client = Fake {
            state: Ok((5, state.to_json())),
            ..Fake::answering(Ok(Answer::default()))
        };
        let (code, out, _) = ctl("status", &client);
        assert_eq!(code, DONE);
        assert_eq!(out.trim(), t("Чекає на шолом", "Waiting for a headset"));
        let (_, out, _) = ctl("monitor list", &client);
        let line = format!(
            "103\tProstir-1\t{}\t640x360 100%\t{}{}",
            t("монітор", "monitor"),
            t("працює", "running"),
            t(", головний", ", primary")
        );
        assert_eq!(out.trim(), line);
        let (_, out, _) = ctl("autostart status --json", &client);
        assert_eq!(out.trim(), r#"{"autostart":null,"revision":5}"#);
    }

    /// `open` starts Prostir when it is not running, then asks for the window; a
    /// missing window still leaves Prostir running.
    #[test]
    fn open_starts_prostir_first() {
        let running = Fake {
            state: Ok((1, "{}".into())),
            ..Fake::answering(refused(ApiError::Unsupported))
        };
        let started = std::cell::Cell::new(false);
        let start = || {
            started.set(true);
            Ok(())
        };
        assert_eq!(open(&running, &start), DONE);
        assert!(!started.get(), "already running");
        assert_eq!(running.asked.borrow()[0], Request::ShowSettings);
        let gone = Fake::answering(Ok(Answer::default()));
        let fail = || Err(std::io::Error::other("no program"));
        assert_eq!(open(&gone, &fail), NOT_RUNNING);
    }
}
