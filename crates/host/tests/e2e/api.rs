//! The local API on a session bus of the test's own (`dbus-daemon --session`; every
//! test here is skipped, saying so, where it is not installed): the state and its
//! changes, requests as JSON and as typed methods, `prostir ctl` and its exit codes,
//! the headsets' events, and what a guest is refused.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use prostir_api::{BUS_NAME, INTERFACE, OBJECT_PATH, State};
use prostir_proto::request::{Asked, Command as HeadsetCommand};
use prostir_testkit::View;
use zbus::blocking::Connection;

use crate::pc::{Bus, Options, Pc, TestDir, ctl_on};
use crate::{SOON, START, TWO_MONITORS, connect, session, worn};

/// Calls `method` of the PC app with `body`.
fn call<B>(bus: &Connection, method: &str, body: &B) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    bus.call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, body)
}

/// The state now, and its revision.
fn get_state(bus: &Connection) -> (u32, State) {
    let (revision, json): (u32, String) = call(bus, "GetState", &())
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    (revision, State::from_json(&json).unwrap())
}

/// The name of the D-Bus error `e`, after the prefix of the PC app's errors.
fn error_name(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, _, _) => name
            .as_str()
            .strip_prefix(prostir_api::ERROR_PREFIX)
            .unwrap_or(name.as_str())
            .to_owned(),
        other => panic!("not a method error: {other}"),
    }
}

/// Every `StateChanged` from now on: its revision and state.
fn state_changes(bus: &Connection) -> mpsc::Receiver<(u32, State)> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(INTERFACE)
        .unwrap()
        .member("StateChanged")
        .unwrap()
        .build();
    let messages = zbus::blocking::MessageIterator::for_match_rule(rule, bus, None).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for msg in messages.flatten() {
            let (revision, json): (u32, String) = msg.body().deserialize().unwrap();
            if tx
                .send((revision, State::from_json(&json).unwrap()))
                .is_err()
            {
                return;
            }
        }
    });
    rx
}

/// A request's answer (as JSON) and the states (revision and state) that came before
/// it.
type Seen = (serde_json::Value, Vec<(u32, State)>);

/// What one connection sees, in order, after it sent `requests` through `Call`
/// without waiting: for each, its answer (as JSON) and the states (revision and
/// state) that came before it.
fn calls_in_order(bus: &Connection, requests: &[String]) -> Vec<Seen> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(INTERFACE)
        .unwrap()
        .member("StateChanged")
        .unwrap()
        .build();
    zbus::blocking::fdo::DBusProxy::new(bus)
        .unwrap()
        .add_match_rule(rule)
        .unwrap();
    // Every message to this connection, in the order the bus sends them.
    let messages = zbus::blocking::MessageIterator::from(bus);
    let mut serials = Vec::new();
    for request in requests {
        let call = zbus::Message::method_call(OBJECT_PATH, "Call")
            .unwrap()
            .destination(BUS_NAME)
            .unwrap()
            .interface(INTERFACE)
            .unwrap()
            .build(&(request.as_str(),))
            .unwrap();
        serials.push(call.primary_header().serial_num());
        bus.send(&call).unwrap();
    }
    let mut states = Vec::new();
    let mut answers: Vec<Option<Seen>> = vec![None; requests.len()];
    for msg in messages {
        let msg = msg.unwrap();
        let header = msg.header();
        match header.message_type() {
            zbus::message::Type::Signal
                if header
                    .member()
                    .is_some_and(|m| m.as_str() == "StateChanged") =>
            {
                let (revision, json): (u32, String) = msg.body().deserialize().unwrap();
                states.push((revision, State::from_json(&json).unwrap()));
            }
            zbus::message::Type::MethodReturn => {
                let Some(i) = serials
                    .iter()
                    .position(|s| Some(*s) == header.reply_serial())
                else {
                    continue;
                };
                let json: String = msg.body().deserialize().unwrap();
                answers[i] = Some((serde_json::from_str(&json).unwrap(), states.clone()));
            }
            zbus::message::Type::Error => panic!("{msg:?}"),
            _ => {}
        }
        if answers.iter().all(Option::is_some) {
            break;
        }
    }
    answers.into_iter().map(Option::unwrap).collect()
}

/// How each display encodes is in the state (`encoding.outputs`), with why one is
/// not on the best encoder for it, and the tray says so too: here a hardware encoder
/// that opens one session, so the second monitor goes on with another.
#[test]
fn the_state_says_how_each_display_encodes() {
    let options = Options {
        software: false,
        encoder: Some("sessions=1"),
        ..Options::default()
    };
    let (_dir, mut pc, _keys, _headset) = session("api-encoders", &options);
    pc.wait_log(START, "fake hardware opens at most 1 sessions at once");
    pc.wait_log(START, "encoding: Prostir-2: ");
    let Some(bus) = &pc.bus else {
        return;
    };
    let bus = bus.connect();
    let until = Instant::now() + SOON;
    loop {
        let (_, state) = get_state(&bus);
        let outputs = &state.encoding.outputs;
        if outputs.len() == 2 && outputs[1].note.is_some() {
            assert_eq!(outputs[0].encoder, "fake hardware", "{outputs:?}");
            assert!(outputs[0].hardware && outputs[0].note.is_none());
            assert_eq!(outputs[0].display, state.displays[0].id);
            assert_ne!(outputs[1].encoder, "fake hardware");
            let note = outputs[1].note.as_deref().unwrap();
            assert!(note.contains("fake hardware"), "{note}");
            return;
        }
        assert!(Instant::now() < until, "{outputs:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An answer goes out after the state with its change, for a request the PC carries
/// out and for a command the headset answers (whose change, its "No limits", comes
/// from the headset just before its answer).
#[test]
fn answers_come_after_their_state() {
    let (_dir, pc, _keys, _headset) = session("api-order", &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let bus = bus.connect();
    let add = r#"{"request":"add_monitor","w":320,"h":240}"#.to_owned();
    let (answer, before) = calls_in_order(&bus, &[add]).remove(0);
    let id = answer["id"].as_u64().unwrap() as u32;
    let (revision, state) = before.last().expect("a state before the answer");
    assert_eq!(*revision, answer["revision"].as_u64().unwrap() as u32);
    assert!(
        state.display(id).is_some(),
        "the state before it has the monitor"
    );
    for on in [true, false] {
        let request = format!(r#"{{"request":"no_limits","on":{on}}}"#);
        let (answer, before) = calls_in_order(&bus, &[request]).remove(0);
        let (revision, state) = before.last().expect("a state before the answer");
        assert_eq!(*revision, answer["revision"].as_u64().unwrap() as u32);
        let (_, session) = state.connected().unwrap();
        assert_eq!(
            session.no_limits,
            Some(on),
            "the state before it has the change"
        );
    }
}

/// Forty changes asked at once: `StateChanged` goes out ten times a second at most,
/// and each answer still comes after a state with its change.
#[test]
fn state_changes_are_ten_a_second_at_most() {
    let (_dir, pc, _keys, _headset) = session("api-rate", &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let bus = bus.connect();
    let requests: Vec<String> = (0..40)
        .map(|i| format!(r#"{{"request":"set_quality","mbit":{}}}"#, 10 + i))
        .collect();
    let started = Instant::now();
    let answers = calls_in_order(&bus, &requests);
    let took = started.elapsed();
    let (_, states) = answers.iter().max_by_key(|(_, s)| s.len()).unwrap();
    let allowed = (took.as_secs_f64() * 10.0).ceil() as usize + 1;
    assert!(
        states.len() <= allowed,
        "{} states in {took:?}",
        states.len()
    );
    for (i, (answer, before)) in answers.iter().enumerate() {
        let (revision, state) = before.last().expect("a state before the answer");
        assert!(*revision >= answer["revision"].as_u64().unwrap() as u32);
        assert!(
            state.encoding.bitrate >= 10 + i as u32,
            "answer {i}: {}",
            state.encoding.bitrate
        );
    }
    let (_, last) = answers.last().unwrap().1.last().unwrap();
    assert_eq!(last.encoding.bitrate, 49);
}

/// The whole state, and a request that changes the desktop: its answer comes at once
/// with the new monitor's id, after the `StateChanged` that has the monitor in it.
#[test]
fn the_state_and_its_changes() {
    let (_dir, pc, _keys, mut headset) = session("api-state", &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let bus = bus.connect();
    // The control loop's pass after the session opened says so.
    let started = Instant::now();
    let (revision, state) = loop {
        let (revision, state) = get_state(&bus);
        if state.pc.phase == prostir_api::Phase::Live || started.elapsed() > SOON {
            break (revision, state);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(revision > 0);
    assert_eq!(state.v, prostir_api::STATE_VERSION);
    assert_eq!(state.pc.phase, prostir_api::Phase::Live);
    assert_eq!(state.pc.name, "fake-pc");
    let (h, s) = state.connected().expect("the headset's session");
    assert_eq!(h.name, "headset");
    assert!(s.main && s.worn);
    assert_eq!(state.displays.len(), 2);
    assert!(
        state
            .displays
            .iter()
            .all(|d| d.state == prostir_api::DisplayState::Running && !d.output.is_empty()),
        "{:?}",
        state.displays
    );
    let changes = state_changes(&bus);
    let started = Instant::now();
    let reply: String = call(
        &bus,
        "Call",
        &(r#"{"request":"add_monitor","w":320,"h":240}"#,),
    )
    .unwrap()
    .body()
    .deserialize()
    .unwrap();
    let answer: serde_json::Value = serde_json::from_str(&reply).unwrap();
    let id = answer["id"].as_u64().expect("the monitor's id") as u32;
    let answered = answer["revision"].as_u64().unwrap() as u32;
    assert!(answered > revision, "{reply}");
    assert!(started.elapsed() < SOON);
    // The change went out first.
    let (changed, state) = changes.recv_timeout(SOON).expect("StateChanged");
    assert!(changed <= answered);
    let mut state = state;
    while state.display(id).is_none() {
        state = changes.recv_timeout(SOON).expect("StateChanged with it").1;
    }
    let added = state.display(id).unwrap();
    assert_eq!((added.mode.w, added.mode.h), (320, 240));
    assert_ne!(added.state, prostir_api::DisplayState::Failed);
    headset
        .wait(SOON, "the new monitor", |v| v.order().len() == 3)
        .unwrap();
    // What does not read is refused with the reason's name.
    let bad = call(&bus, "Call", &("recenter",)).unwrap_err();
    assert_eq!(error_name(&bad), "BadArgument");
    let unknown = call(&bus, "Call", &(r#"{"request":"fly"}"#,)).unwrap_err();
    assert_eq!(error_name(&unknown), "Unsupported");
    let last = call(&bus, "RemoveMonitor", &(99_999u32,)).unwrap_err();
    assert_eq!(error_name(&last), "UnknownDisplay");
}

/// `prostir ctl` without Prostir: at once, exit 2, nothing started; wrong arguments
/// 64 whatever runs.
#[test]
fn ctl_without_prostir() {
    let dir = TestDir::new("api-none");
    let Some(bus) = Bus::start(dir.path()) else {
        return;
    };
    let started = Instant::now();
    let status = ctl_on(&bus.address, &["status"]);
    assert_eq!(status.code, 2, "{status:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(status.out.trim(), "Not running");
    let json = ctl_on(&bus.address, &["status", "--json"]);
    assert_eq!(json.json()["running"], false);
    assert_eq!(ctl_on(&bus.address, &["recenter"]).code, 2);
    let wrong = ctl_on(&bus.address, &["cinema", "maybe"]);
    assert_eq!(wrong.code, 64, "{wrong:?}");
    assert!(wrong.err.contains("on or off"), "{wrong:?}");
    assert!(
        !bus.connect()
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(BUS_NAME,),
            )
            .unwrap()
            .body()
            .deserialize::<bool>()
            .unwrap(),
        "nothing took the name"
    );
}

/// The typed methods, as `busctl` calls them from a script or a hot key.
#[test]
fn typed_methods_from_busctl() {
    let (_dir, pc, _keys, mut headset) = session("api-busctl", &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let busctl = |args: &[&str]| {
        Command::new("busctl")
            .arg(format!("--address={}", bus.address))
            // `--`: a negative number is an argument, not an option.
            .args(["call", "--", BUS_NAME, OBJECT_PATH, INTERFACE])
            .args(args)
            .stdin(Stdio::null())
            .output()
    };
    let recenter = match busctl(&["Recenter"]) {
        Ok(r) => r,
        Err(e) if std::env::var_os("CI").is_some() => panic!("no busctl in CI: {e}"),
        Err(e) => {
            eprintln!("\n*** no busctl ({e}): the typed methods are SKIPPED ***\n");
            return;
        }
    };
    assert!(recenter.status.success(), "{recenter:?}");
    let distance = busctl(&["Distance", "n", "-2"]).unwrap();
    assert!(distance.status.success(), "{distance:?}");
    let profile = busctl(&["Profile", "s", "Робота"]).unwrap();
    assert!(profile.status.success(), "{profile:?}");
    headset
        .wait(SOON, "the commands", |v| v.commands.len() == 3)
        .unwrap();
    assert_eq!(
        headset.view().commands,
        [
            Asked::Known(HeadsetCommand::Recenter),
            Asked::Known(HeadsetCommand::Distance { steps: -2 }),
            Asked::Known(HeadsetCommand::Profile {
                name: "Робота".into()
            }),
        ]
    );
    let added = busctl(&["AddMonitor", "uuq", "320", "240", "100"]).unwrap();
    assert!(added.status.success(), "{added:?}");
    let out = String::from_utf8_lossy(&added.stdout);
    let id: u32 = out
        .trim()
        .strip_prefix("u ")
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{out}"));
    headset
        .wait(SOON, "the new monitor", |v| {
            v.order().contains(&prostir_proto::state::DisplayId(id))
        })
        .unwrap();
    let removed = busctl(&["RemoveMonitor", "u", &id.to_string()]).unwrap();
    assert!(removed.status.success(), "{removed:?}");
    let gone = busctl(&["RemoveMonitor", "u", &id.to_string()]).unwrap();
    assert!(!gone.status.success(), "a monitor removed twice");
    let quality = busctl(&["SetQuality", "u", "15"]).unwrap();
    assert!(quality.status.success(), "{quality:?}");
    headset
        .wait(SOON, "the quality", |v| {
            v.encoding().is_some_and(|e| e.wanted.bitrate == 15)
        })
        .unwrap();
}

/// `ctl watch --json` sees the headset connect, come off the head and back, and go.
#[test]
fn ctl_watch_sees_the_headset() {
    let dir = TestDir::new("api-watch");
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let pc = Pc::start(&dir, &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let mut watch = Command::new(env!("CARGO_BIN_EXE_prostir"))
        .args(["ctl", "watch", "--json"])
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (tx, lines) = mpsc::channel();
    let out = watch.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if tx.send(value).is_err() {
                return;
            }
        }
    });
    let next = |event: &str| loop {
        let line = lines
            .recv_timeout(START)
            .unwrap_or_else(|_| panic!("no {event}"));
        if line["event"] == event {
            return line;
        }
    };
    // It says the state it starts from.
    next("state_changed");
    let mut headset = connect(&pc, &mut keys);
    let connected = next("headset_connected");
    assert_eq!(connected["name"], "headset");
    // From 127.0.0.1: as through `adb reverse`.
    assert_eq!(connected["via"], "usb");
    let key = connected["headset"].as_str().unwrap().to_owned();
    assert_eq!(key.len(), 12);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    headset.send(&worn(false)).unwrap();
    let off = next("headset_worn");
    assert_eq!(
        (off["headset"].as_str(), off["worn"].as_bool()),
        (Some(key.as_str()), Some(false))
    );
    headset.send(&worn(true)).unwrap();
    assert_eq!(next("headset_worn")["worn"], true);
    headset.disconnect();
    let gone = next("headset_disconnected");
    assert_eq!(gone["headset"], key.as_str());
    assert_eq!(gone["reason"], "other");
    let _ = watch.kill();
    let _ = watch.wait();
}

/// A guest (a headset its line in `settings.txt` makes one) is someone else's
/// headset: with only a guest connected, commands from the PC are refused, while
/// the PC's own settings still change.
#[test]
fn a_guest_takes_no_commands_from_the_pc() {
    let dir = TestDir::new("api-guest");
    dir.save_monitors(TWO_MONITORS);
    let mut keys = dir.pair();
    let key = prostir_proto::secure::to_hex(&keys.identity.public);
    dir.save_settings(&format!("access {key} guest input\n"));
    let pc = Pc::start(&dir, &Options::default());
    let mut headset = connect(&pc, &mut keys);
    headset
        .wait(START, "every monitor showing", View::all_monitors_show)
        .unwrap();
    let Some(refused) = pc.ctl(&["recenter", "--json"]) else {
        return;
    };
    assert_eq!(refused.code, 1, "{refused:?}");
    assert_eq!(refused.json()["error"], "NotAllowed");
    let bus = pc.bus.as_ref().unwrap().connect();
    let typed = call(&bus, "NoLimits", &(true,)).unwrap_err();
    assert_eq!(error_name(&typed), "NotAllowed");
    headset.take_for(Duration::from_millis(500)).unwrap();
    assert!(headset.view().commands.is_empty(), "no command reached it");
    let (_, state) = get_state(&bus);
    assert_eq!(state.headsets[0].role, prostir_api::Role::Guest);
    assert!(!state.connected().unwrap().1.main);
    let quality = pc.ctl(&["quality", "15"]).unwrap();
    assert_eq!(quality.code, 0, "{quality:?}");
}

/// `ctl quit` is answered, then Prostir says goodbye to the headset and ends.
#[test]
fn ctl_quit_ends_prostir() {
    let (_dir, mut pc, _keys, mut headset) = session("api-quit", &Options::default());
    let Some(quit) = pc.ctl(&["quit"]) else {
        return;
    };
    assert_eq!(quit.code, 0, "{quit:?}");
    headset.wait_closed(SOON).unwrap();
    assert_eq!(
        headset.view().goodbye,
        Some(prostir_proto::PcGoodbye::Quit.code())
    );
    assert!(pc.wait_end(SOON).success());
}

/// A command whose arguments do not fit the protocol (a profile's name over 64 bytes)
/// is refused, and the session goes on; one the headset does not know is refused by
/// the headset.
#[test]
fn a_command_that_does_not_fit_is_refused() {
    let (_dir, pc, _keys, mut headset) = session("api-long", &Options::default());
    let long = "Робота".repeat(10);
    let Some(refused) = pc.ctl(&["profile", &long, "--json"]) else {
        return;
    };
    assert_eq!(refused.code, 1, "{refused:?}");
    assert_eq!(refused.json()["error"], "BadArgument");
    let bus = pc.bus.as_ref().unwrap().connect();
    let typed = call(&bus, "Profile", &(long.as_str(),)).unwrap_err();
    assert_eq!(error_name(&typed), "BadArgument");
    headset.take_for(Duration::from_millis(300)).unwrap();
    assert!(!headset.view().closed, "the session goes on");
    assert!(headset.view().commands.is_empty());
    let done = pc.ctl(&["profile", "Робота"]).unwrap();
    assert_eq!(done.code, 0, "{done:?}");
    headset.refuse_commands(prostir_proto::state::ErrorCode::UNSUPPORTED);
    let unknown = pc.ctl(&["cinema", "on", "--json"]).unwrap();
    assert_eq!(unknown.code, 1, "{unknown:?}");
    assert_eq!(unknown.json()["error"], "Unsupported");
}

/// The session bus restarted: the PC app takes its name again and answers as before.
#[test]
fn the_api_comes_back_after_the_bus_restarts() {
    let (_dir, mut pc, _keys, _headset) = session("api-bus-again", &Options::default());
    if !pc.restart_bus() {
        return;
    }
    pc.wait_log(
        SOON,
        "local API on D-Bus as io.github.OlexiyOdarchuk.Prostir again",
    );
    let status = pc.ctl(&["status", "--json"]).unwrap();
    assert_eq!(status.code, 0, "{status:?}");
    assert_eq!(status.json()["pc"]["phase"], "live");
    let done = pc.ctl(&["recenter"]).unwrap();
    assert_eq!(done.code, 0, "{done:?}");
}

/// Without a headset the set kept for the next one stands `kept`, and is changed there.
#[test]
fn the_kept_set_without_a_headset() {
    let dir = TestDir::new("api-kept");
    dir.save_monitors(TWO_MONITORS);
    let pc = Pc::start(&dir, &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let bus = bus.connect();
    let (_, state) = get_state(&bus);
    assert_eq!(state.pc.phase, prostir_api::Phase::Idle);
    assert_eq!(state.displays.len(), 2);
    assert!(
        state
            .displays
            .iter()
            .all(|d| d.state == prostir_api::DisplayState::Kept),
        "{:?}",
        state.displays
    );
    let first = state.displays[0].id;
    let scale = *state.limits.scales.iter().find(|s| **s != 100).unwrap();
    let rescaled = pc
        .ctl(&["--json", "monitor", "retry", &first.to_string()])
        .unwrap();
    assert_eq!(rescaled.code, 0, "{rescaled:?}");
    let unknown = pc.ctl(&["--json", "monitor", "retry", "99999"]).unwrap();
    assert_eq!(unknown.json()["error"], "UnknownDisplay");
    let reply: String = call(
        &bus,
        "Call",
        &(format!(
            r#"{{"request":"set_scale","id":{first},"scale":{scale}}}"#
        ),),
    )
    .unwrap()
    .body()
    .deserialize()
    .unwrap();
    assert!(reply.contains("revision"), "{reply}");
    let (_, state) = get_state(&bus);
    assert_eq!(state.display(first).unwrap().mode.scale, scale);
    let place = call(
        &bus,
        "Call",
        &(r#"{"request":"place","id":1,"x":0,"y":0}"#,),
    )
    .unwrap_err();
    assert_eq!(error_name(&place), "Unsupported");
}

/// Monitors placed from the PC (a map in the settings window): another scale keeps
/// the monitor's id, and a monitor moved above the row gets a row of its own there.
#[test]
fn monitors_scaled_and_placed_from_the_pc() {
    let (_dir, pc, _keys, mut headset) = session("api-place", &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let bus = bus.connect();
    let started = Instant::now();
    let state = loop {
        let (_, state) = get_state(&bus);
        let placed = state.displays.iter().all(|d| d.desk.is_some());
        if (placed && state.displays.len() == 2) || started.elapsed() > SOON {
            break state;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (a, b) = (state.displays[0].clone(), state.displays[1].clone());
    let (da, db) = (a.desk.unwrap(), b.desk.unwrap());
    assert_eq!(da.y.min(db.y), da.y.min(db.y), "one row");
    // B above A.
    let request = format!(
        r#"{{"request":"place","id":{},"x":{},"y":{}}}"#,
        b.id,
        da.x,
        da.y - 2 * db.h as i32
    );
    let reply: String = call(&bus, "Call", &(request.as_str(),))
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    assert!(reply.contains("revision"), "{reply}");
    let started = Instant::now();
    loop {
        let (_, state) = get_state(&bus);
        let (na, nb) = (
            state.display(a.id).unwrap().desk.unwrap(),
            state.display(b.id).unwrap().desk.unwrap(),
        );
        if nb.y + nb.h as i32 <= na.y {
            break;
        }
        assert!(started.elapsed() < SOON, "not above: {na:?} and {nb:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Another scale: the same monitor, made anew at it.
    let scale = *state
        .limits
        .scales
        .iter()
        .find(|s| **s != a.mode.scale)
        .expect("another scale");
    let request = format!(r#"{{"request":"set_scale","id":{},"scale":{scale}}}"#, a.id);
    call(&bus, "Call", &(request.as_str(),)).unwrap();
    let started = Instant::now();
    loop {
        let (_, state) = get_state(&bus);
        let d = state.display(a.id).expect("the same id");
        if d.mode.scale == scale && d.state == prostir_api::DisplayState::Running {
            break;
        }
        assert!(started.elapsed() < START, "{d:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    headset
        .wait(SOON, "the new scale", |v| {
            v.display(prostir_proto::state::DisplayId(a.id))
                .is_some_and(|d| d.mode.scale == scale)
        })
        .unwrap();
    let bad = call(
        &bus,
        "Call",
        &(format!(r#"{{"request":"set_scale","id":{},"scale":30}}"#, a.id).as_str(),),
    )
    .unwrap_err();
    assert_eq!(error_name(&bad), "BadSize");
}

/// A request over 64 KiB is refused unread.
#[test]
fn a_huge_request_is_refused() {
    let dir = TestDir::new("api-huge");
    dir.save_monitors(TWO_MONITORS);
    let pc = Pc::start(&dir, &Options::default());
    let Some(bus) = &pc.bus else {
        return;
    };
    let bus = bus.connect();
    let huge = format!(r#"{{"request":"profile","name":"{}"}}"#, "x".repeat(70_000));
    let refused = call(&bus, "Call", &(huge.as_str(),)).unwrap_err();
    assert_eq!(error_name(&refused), "BadArgument");
}
