//! A service in memory, for tests, pictures and trying the window before the real one
//! exists (`PROSTIR_SETTINGS_MOCK=1`). It answers every request the window sends by
//! changing its state, the way the service is meant to.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::*;

/// How long the pairing window stays open.
pub const PAIR_SECONDS: u32 = 180;

/// A code that never matches, to try the "wrong code" state.
pub const WRONG_CODE: &str = "000000";

struct Inner {
    state: State,
    revision: u32,
    running: bool,
    sink: Option<Arc<Sink>>,
    /// Real time: the pairing window runs out. Frozen: it stays as made (pictures).
    live: bool,
    pairing_since: Option<Instant>,
    calls: Vec<Request>,
    /// The next request is refused with this error.
    refuse: Option<ClientError>,
}

/// The mock service.
pub struct Mock {
    inner: Arc<Mutex<Inner>>,
}

/// A second handle on the same mock: switch it on and off, read what was asked.
#[derive(Clone)]
pub struct MockHandle {
    inner: Arc<Mutex<Inner>>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn display(id: u32, name: &str, w: u32, h: u32, x: i32, y: i32) -> Display {
    Display {
        id,
        kind: DisplayKind::Virtual,
        name: name.into(),
        output: format!("Virtual-{id}"),
        builtin: false,
        shown: true,
        mode: Mode { w, h, scale: 100 },
        desk: Some(DeskRect { x, y, w, h }),
        state: DisplayState::Running,
        failure: None,
    }
}

/// The PC's own screen, as a laptop has.
fn laptop(id: u32) -> Display {
    Display {
        kind: DisplayKind::Physical,
        name: "eDP-1".into(),
        output: "eDP-1".into(),
        builtin: true,
        mode: Mode {
            w: 1920,
            h: 1200,
            scale: 125,
        },
        desk: Some(DeskRect {
            x: 2880,
            y: 2000,
            w: 1536,
            h: 960,
        }),
        ..display(id, "eDP-1", 1920, 1200, 0, 0)
    }
}

fn base() -> State {
    State {
        v: STATE_VERSION,
        pc: Pc {
            name: "workstation".into(),
            app: env!("CARGO_PKG_VERSION").into(),
            desktop: "KDE Plasma".into(),
        },
        window: WindowLook::default(),
        autostart: Some(true),
        headsets: vec![
            Headset {
                key: "5f2a9c".into(),
                name: "Quest 3".into(),
                model: "Quest 3".into(),
                last_seen: Some(now_unix() - 2 * 3600),
                session: None,
            },
            Headset {
                key: "c81e07".into(),
                name: "Quest 3S".into(),
                model: "Quest 3S".into(),
                last_seen: Some(now_unix() - 26 * 3600),
                session: None,
            },
        ],
        connecting: None,
        displays: vec![laptop(4)],
        primary: Some(4),
        limits: Limits {
            sizes: vec![
                [1920, 1080],
                [2560, 1440],
                [3840, 2160],
                [3440, 1440],
                [1440, 2560],
            ],
            scales: vec![100, 125, 150],
            displays_max: Some(6),
        },
        sound: Sound {
            to_headset: true,
            volume: None,
            mic_on: true,
            mic: MicState::Off,
            mic_failure: None,
        },
        pairing: None,
        nearby: Vec::new(),
        problems: Vec::new(),
    }
}

impl Mock {
    fn with(state: State, running: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state,
                revision: 1,
                running,
                sink: None,
                live: false,
                pairing_since: None,
                calls: Vec::new(),
                refuse: None,
            })),
        }
    }

    /// A headset in a session with three monitors and the laptop's screen shown.
    pub fn connected() -> Self {
        let mut s = base();
        s.headsets[0].session = Some(Session {
            via: Via::Wifi,
            worn: true,
            battery: Some(Battery {
                percent: 72,
                charging: false,
            }),
            profile: Some("Робота".into()),
            mic_allowed: Some(true),
        });
        s.headsets[0].last_seen = Some(now_unix());
        s.displays = vec![
            display(1, "Prostir-1", 2560, 1440, 0, 560),
            display(2, "Prostir-2", 2560, 1440, 2560, 560),
            display(3, "Prostir-3", 1440, 2560, 5120, 0),
            laptop(4),
        ];
        s.primary = Some(2);
        s.sound.volume = Some(Volume {
            level: 64,
            muted: false,
        });
        s.sound.mic = MicState::Ready;
        Self::with(s, true)
    }

    /// Paired headsets, none connected: only the PC's own screen.
    pub fn waiting() -> Self {
        Self::with(base(), true)
    }

    /// A headset found this PC and the connection is being set up.
    pub fn connecting() -> Self {
        let mut s = base();
        s.connecting = Some("Quest 3".into());
        Self::with(s, true)
    }

    /// Connected, but one monitor could not be made.
    pub fn failed_monitor() -> Self {
        let m = Self::connected();
        {
            let mut i = m.inner.lock().unwrap();
            let mut d = display(5, "Prostir-4", 1920, 1080, 0, 0);
            d.desk = None;
            d.state = DisplayState::Failed;
            d.failure = Some(Problem {
                code: "Desktop".into(),
                text: "KWin не відповів за 10 секунд, Prostir спробує ще раз.".into(),
            });
            i.state.displays.insert(3, d);
        }
        m
    }

    /// A PC that has never paired a headset.
    pub fn no_headsets() -> Self {
        let mut s = base();
        s.headsets.clear();
        Self::with(s, true)
    }

    /// The service is not on the bus.
    pub fn not_running() -> Self {
        Self::with(base(), false)
    }

    /// With the pairing window open: a headset shows a code to type here.
    pub fn pairing(mut self) -> Self {
        {
            let mut i = self.inner.lock().unwrap();
            i.state.pairing = Some(Pairing {
                seconds_left: 161,
                seconds_total: PAIR_SECONDS,
                pc_code: None,
                code_for: Some("Quest 3S".into()),
                last: None,
            });
            i.state.nearby = vec!["Quest 3S".into()];
        }
        self = self.frozen();
        self
    }

    /// With the window's look as the service keeps it.
    pub fn look(self, theme: ThemeMode, palette: &str) -> Self {
        self.inner.lock().unwrap().state.window = WindowLook {
            theme,
            palette: palette.into(),
            language: None,
        };
        self
    }

    /// Changes the state before the window sees it (tests).
    pub fn edit(self, f: impl FnOnce(&mut State)) -> Self {
        f(&mut self.inner.lock().unwrap().state);
        self
    }

    /// Time runs: the pairing window counts down and closes, a headset turns up
    /// a few seconds after it opens.
    pub fn live(self) -> Self {
        self.inner.lock().unwrap().live = true;
        self
    }

    fn frozen(self) -> Self {
        self.inner.lock().unwrap().live = false;
        self
    }

    pub fn handle(&self) -> MockHandle {
        MockHandle {
            inner: self.inner.clone(),
        }
    }

    /// The state now (tests).
    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }
}

impl MockHandle {
    /// Starts or stops the service, telling the window as the bus would.
    pub fn set_running(&self, on: bool) {
        let sink = {
            let mut i = self.inner.lock().unwrap();
            i.running = on;
            i.sink.clone()
        };
        if let Some(sink) = sink {
            sink(if on {
                Change::Appeared
            } else {
                Change::Vanished
            });
        }
    }

    /// Refuses the next request with `e`, as a service with a problem would.
    pub fn refuse_next(&self, e: ClientError) {
        self.inner.lock().unwrap().refuse = Some(e);
    }

    /// Every request asked so far.
    pub fn calls(&self) -> Vec<Request> {
        self.inner.lock().unwrap().calls.clone()
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    /// Moves time on: the pairing window, a headset turning up (live mocks only).
    pub fn tick(&self) {
        let changed = {
            let mut i = self.inner.lock().unwrap();
            i.tick()
        };
        if changed {
            self.inner.lock().unwrap().publish();
        }
    }
}

impl Inner {
    fn publish(&mut self) {
        self.revision += 1;
        if let Some(sink) = self.sink.clone() {
            let snap = Snapshot {
                revision: self.revision,
                state: Box::new(self.state.clone()),
            };
            sink(Change::State(snap));
        }
    }

    /// The pairing window in real time; whether anything changed.
    fn tick(&mut self) -> bool {
        let (Some(since), true) = (self.pairing_since, self.live) else {
            return false;
        };
        let gone = since.elapsed().as_secs() as u32;
        let Some(p) = &mut self.state.pairing else {
            return false;
        };
        if gone >= PAIR_SECONDS {
            self.state.pairing = None;
            self.state.nearby.clear();
            self.pairing_since = None;
            return true;
        }
        p.seconds_left = PAIR_SECONDS - gone;
        if gone >= 4 && p.code_for.is_none() {
            p.code_for = Some("Quest 3S".into());
            self.state.nearby = vec!["Quest 3S".into()];
            return true;
        }
        false
    }

    fn display_mut(&mut self, id: u32) -> Result<&mut Display, ClientError> {
        self.state
            .displays
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or_else(|| refused("UnknownDisplay", "Такого дисплея немає."))
    }

    fn apply(&mut self, r: &Request) -> Result<Reply, ClientError> {
        let s = &mut self.state;
        let connected = s.connected().is_some();
        let mut reply = serde_json::json!({});
        match r {
            Request::AddMonitor { w, h, scale } => {
                if !connected {
                    return Err(refused("NotConnected", "Шолом не підключено."));
                }
                let count = s
                    .displays
                    .iter()
                    .filter(|d| d.kind == DisplayKind::Virtual)
                    .count() as u32;
                if s.limits.displays_max.is_some_and(|m| count >= m) {
                    return Err(refused("Limit", "Більше моніторів не вміщається."));
                }
                let id = s.displays.iter().map(|d| d.id).max().unwrap_or(0) + 1;
                let n = (1..)
                    .find(|n| !s.displays.iter().any(|d| d.name == format!("Prostir-{n}")))
                    .unwrap_or(1);
                let right = s
                    .displays
                    .iter()
                    .filter_map(|d| d.desk)
                    .map(|r| r.x + r.w as i32)
                    .max()
                    .unwrap_or(0);
                let mut d = display(id, &format!("Prostir-{n}"), *w, *h, right, 0);
                d.mode.scale = *scale;
                s.displays.insert(count as usize, d);
                reply = serde_json::json!({ "id": id });
            }
            Request::RemoveMonitor { id } => {
                let before = s.displays.len();
                s.displays
                    .retain(|d| !(d.id == *id && d.kind == DisplayKind::Virtual));
                if s.displays.len() == before {
                    return Err(refused("UnknownDisplay", "Такого монітора немає."));
                }
                if s.primary == Some(*id) {
                    s.primary = s.displays.first().map(|d| d.id);
                }
            }
            Request::RetryMonitor { id } => {
                let d = self.display_mut(*id)?;
                d.state = DisplayState::Running;
                d.failure = None;
                d.desk = Some(DeskRect {
                    x: 0,
                    y: 0,
                    w: d.mode.w,
                    h: d.mode.h,
                });
            }
            Request::ShowPhysical { id, shown } => self.display_mut(*id)?.shown = *shown,
            Request::SetScale { id, scale } => self.display_mut(*id)?.mode.scale = *scale,
            Request::SetPrimary { id } => {
                self.display_mut(*id)?;
                self.state.primary = Some(*id);
            }
            Request::Place { id, x, y } => {
                let d = self.display_mut(*id)?;
                if let Some(r) = &mut d.desk {
                    r.x = *x;
                    r.y = *y;
                }
            }
            Request::PairNewHeadset => {
                s.pairing = Some(Pairing {
                    seconds_left: PAIR_SECONDS,
                    seconds_total: PAIR_SECONDS,
                    ..Default::default()
                });
                self.pairing_since = Some(Instant::now());
                reply = serde_json::json!({ "seconds_left": PAIR_SECONDS });
            }
            Request::StopPairing => {
                s.pairing = None;
                s.nearby.clear();
                self.pairing_since = None;
            }
            Request::PairingCode { code } => {
                let Some(p) = &mut s.pairing else {
                    return Err(refused("NotAllowed", "Сполучення не відкрито."));
                };
                let Some(name) = p.code_for.clone() else {
                    return Err(refused("NotAllowed", "Жоден шолом не чекає на код."));
                };
                if code == WRONG_CODE || code.len() != 6 {
                    p.last = Some(PairResult::Wrong);
                } else {
                    let taken = s.headsets.iter().filter(|h| h.name == name).count();
                    let shown = if taken == 0 {
                        name
                    } else {
                        format!("{name} ({})", taken + 1)
                    };
                    s.headsets.push(Headset {
                        key: format!("{:06x}", 0xa0000 + s.headsets.len()),
                        name: shown,
                        model: "Quest 3S".into(),
                        last_seen: Some(now_unix()),
                        session: None,
                    });
                    // Pairing closes once a headset is paired.
                    s.pairing = None;
                    s.nearby.clear();
                    self.pairing_since = None;
                }
            }
            Request::ForgetHeadset { headset } => {
                let before = s.headsets.len();
                s.headsets.retain(|h| h.key != *headset);
                if s.headsets.len() == before {
                    return Err(refused("BadArgument", "Такого шолома немає."));
                }
            }
            Request::SoundToHeadset { on } => s.sound.to_headset = *on,
            Request::Volume { level } => match &mut s.sound.volume {
                Some(v) => v.level = (*level).min(100),
                None => return Err(refused("NotConnected", "Шолом не підключено.")),
            },
            Request::SetMic { on } => {
                s.sound.mic_on = *on;
                s.sound.mic = if *on && connected {
                    MicState::Ready
                } else {
                    MicState::Off
                };
            }
            Request::Autostart { on } => s.autostart = Some(*on),
            Request::WindowLook {
                theme,
                palette,
                language,
            } => {
                s.window = WindowLook {
                    theme: *theme,
                    palette: palette.clone(),
                    language: language.clone(),
                }
            }
        }
        Ok(Reply(reply))
    }
}

fn refused(code: &str, message: &str) -> ClientError {
    ClientError::Refused {
        code: code.into(),
        message: message.into(),
    }
}

impl Client for Mock {
    fn get_state(&mut self) -> Result<Snapshot, ClientError> {
        let mut i = self.inner.lock().unwrap();
        if !i.running {
            return Err(ClientError::NotRunning);
        }
        i.tick();
        Ok(Snapshot {
            revision: i.revision,
            state: Box::new(i.state.clone()),
        })
    }

    fn call(&mut self, request: &Request) -> Result<Reply, ClientError> {
        let mut i = self.inner.lock().unwrap();
        if !i.running {
            return Err(ClientError::NotRunning);
        }
        i.calls.push(request.clone());
        if let Some(e) = i.refuse.take() {
            return Err(e);
        }
        let reply = i.apply(request)?;
        i.publish();
        Ok(reply)
    }

    fn watch(&mut self, sink: Sink) -> Result<(), ClientError> {
        let live = {
            let mut i = self.inner.lock().unwrap();
            i.sink = Some(Arc::new(sink));
            i.live
        };
        if live {
            // The pairing window counts down without the window asking.
            let handle = self.handle();
            std::thread::Builder::new()
                .name("mock-clock".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(Duration::from_millis(500));
                        if Arc::strong_count(&handle.inner) < 2 {
                            return;
                        }
                        handle.tick();
                    }
                })
                .map_err(|e| ClientError::Broken(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(m: &mut Mock, r: Request) -> Result<Reply, ClientError> {
        m.call(&r)
    }

    #[test]
    fn requests_change_the_state() {
        let mut m = Mock::connected();
        let reply = call(
            &mut m,
            Request::AddMonitor {
                w: 1920,
                h: 1080,
                scale: 125,
            },
        )
        .unwrap();
        let id = reply.0["id"].as_u64().unwrap() as u32;
        let s = m.state();
        let d = s.display(id).unwrap();
        assert_eq!((d.name.as_str(), d.mode.scale), ("Prostir-4", 125));
        call(&mut m, Request::SetPrimary { id }).unwrap();
        call(&mut m, Request::Place { id, x: -1920, y: 0 }).unwrap();
        assert_eq!(m.state().primary, Some(id));
        assert_eq!(m.state().display(id).unwrap().desk.unwrap().x, -1920);
        call(&mut m, Request::RemoveMonitor { id }).unwrap();
        assert!(m.state().display(id).is_none());
        assert!(matches!(
            call(&mut m, Request::RemoveMonitor { id: 4 }),
            Err(ClientError::Refused { code, .. }) if code == "UnknownDisplay"
        ));
    }

    #[test]
    fn pairing_takes_a_right_code_and_closes() {
        let mut m = Mock::connected().pairing();
        call(
            &mut m,
            Request::PairingCode {
                code: WRONG_CODE.into(),
            },
        )
        .unwrap();
        assert_eq!(m.state().pairing.unwrap().last, Some(PairResult::Wrong));
        call(
            &mut m,
            Request::PairingCode {
                code: "482913".into(),
            },
        )
        .unwrap();
        let s = m.state();
        assert!(s.pairing.is_none());
        assert_eq!(s.headsets.last().unwrap().name, "Quest 3S (2)");
    }

    #[test]
    fn not_running_says_so() {
        let mut m = Mock::not_running();
        assert_eq!(m.get_state(), Err(ClientError::NotRunning));
        assert_eq!(
            call(&mut m, Request::StopPairing),
            Err(ClientError::NotRunning)
        );
    }
}
