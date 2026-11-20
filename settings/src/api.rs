//! The window's side of Prostir's local API (step 6.7 in plans/step-6-platform.md,
//! section 7): the whole state as JSON from `GetState` and `StateChanged`, requests
//! as JSON through `Call`.
//!
//! The service does not exist yet; these types are what the window reads and sends,
//! written against the plan, and the proposal for the service's `state_json` and
//! `CtlRequest`. Everything is lenient on purpose: a missing field takes its default,
//! an unknown value of a closed set reads as `Other`, unknown fields are ignored, so a
//! newer service with more in its state does not break an older window.

use serde::{Deserialize, Serialize};

/// The service's name on the session bus.
pub const BUS_NAME: &str = "io.github.OlexiyOdarchuk.Prostir";
pub const OBJECT_PATH: &str = "/io/github/OlexiyOdarchuk/Prostir";
pub const INTERFACE: &str = "io.github.OlexiyOdarchuk.Prostir1";
/// Prefix of the service's errors: `<prefix>.NotConnected` and so on.
pub const ERROR_PREFIX: &str = "io.github.OlexiyOdarchuk.Prostir1.Error.";

/// The `"v"` of the state this window reads.
pub const STATE_VERSION: u32 = 1;

/// Everything the window shows, as one JSON object.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub v: u32,
    pub pc: Pc,
    /// The window's own look, kept by the service with the rest of the settings.
    pub window: WindowLook,
    /// "Start with the system"; none: this PC cannot set it from here.
    pub autostart: Option<bool>,
    /// Paired headsets.
    pub headsets: Vec<Headset>,
    /// A headset being connected (encryption, versions) now, by name.
    pub connecting: Option<String>,
    pub displays: Vec<Display>,
    /// The desktop's primary display (where the panel is).
    pub primary: Option<u32>,
    pub limits: Limits,
    pub sound: Sound,
    /// The pairing window, while it is open.
    pub pairing: Option<Pairing>,
    /// Headsets that asked for this PC recently, by name (discovery).
    pub nearby: Vec<String>,
    /// What is wrong now, in the PC's language.
    pub problems: Vec<Problem>,
}

impl State {
    /// The headset in a session now.
    pub fn connected(&self) -> Option<(&Headset, &Session)> {
        self.headsets
            .iter()
            .find_map(|h| h.session.as_ref().map(|s| (h, s)))
    }

    pub fn display(&self, id: u32) -> Option<&Display> {
        self.displays.iter().find(|d| d.id == id)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pc {
    /// The name headsets see.
    pub name: String,
    /// The PC app's version.
    pub app: String,
    /// `KDE Plasma`, `Hyprland`.
    pub desktop: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    Light,
    Dark,
    /// As the desktop says (the XDG portal's `color-scheme`); also any mode a newer
    /// service knows and this window does not.
    #[default]
    #[serde(other)]
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowLook {
    pub theme: ThemeMode,
    /// `ink` or `graphite`.
    pub palette: String,
    /// A language code (`uk`, `en`); none: the desktop's.
    pub language: Option<String>,
}

impl Default for WindowLook {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            palette: "ink".into(),
            language: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Headset {
    /// The fingerprint of its key: how requests name it.
    pub key: String,
    pub name: String,
    pub model: String,
    /// When it was last connected, in Unix seconds.
    pub last_seen: Option<u64>,
    /// Its session, while it is connected.
    pub session: Option<Session>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    #[default]
    Wifi,
    Usb,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub via: Via,
    /// On the head; streaming pauses while it is not.
    pub worn: bool,
    pub battery: Option<Battery>,
    /// The headset's profile in use.
    pub profile: Option<String>,
    /// Whether the headset lets Prostir use its microphone.
    pub mic_allowed: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayKind {
    /// A monitor made for the headset.
    #[default]
    Virtual,
    /// A screen of the PC itself.
    Physical,
    /// A small output for one window (a floating panel in the headset).
    Window,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayState {
    #[default]
    Starting,
    Running,
    /// Could not start; see `failure`. It keeps its place and is tried again.
    Failed,
    /// A PC screen that is not connected now (lid closed, cable out).
    Absent,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mode {
    pub w: u32,
    pub h: u32,
    /// Percent: 100, 125, 150.
    pub scale: u16,
}

/// Where a display is on the PC's desktop, in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeskRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Display {
    pub id: u32,
    pub kind: DisplayKind,
    /// `Prostir-2`, a PC screen's model, a window's title.
    pub name: String,
    /// The desktop's name for the output (`eDP-1`, `Virtual-2`).
    pub output: String,
    /// A PC screen: the laptop's own panel.
    pub builtin: bool,
    /// A PC screen: shown in the headset.
    pub shown: bool,
    pub mode: Mode,
    pub desk: Option<DeskRect>,
    pub state: DisplayState,
    pub failure: Option<Problem>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Sizes a new monitor can have, `[w, h]`, as the headset and the PC allow.
    pub sizes: Vec<[u32; 2]>,
    /// Scales in percent.
    pub scales: Vec<u16>,
    /// How many Prostir monitors fit at once; none: not known.
    pub displays_max: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicState {
    #[default]
    Off,
    Ready,
    Failed,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sound {
    /// The PC's sound plays in the headset (the speakers are quiet meanwhile).
    pub to_headset: bool,
    /// The headset's volume, while one is connected.
    pub volume: Option<Volume>,
    /// The headset microphone is wanted on the PC.
    pub mic_on: bool,
    /// The PC's end of it (the virtual microphone programs record from).
    pub mic: MicState,
    pub mic_failure: Option<Problem>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Volume {
    pub level: u8,
    pub muted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pairing {
    /// Seconds until the pairing window closes, when this state was made.
    pub seconds_left: u32,
    /// How long it was opened for (3 minutes).
    pub seconds_total: u32,
    /// A code made here, to type in the headset (when the headset asks for one).
    pub pc_code: Option<String>,
    /// A headset showing a code and waiting for it to be typed here, by name.
    pub code_for: Option<String>,
    /// How the last attempt ended, if one did.
    pub last: Option<PairResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairResult {
    /// A new headset is trusted now.
    Paired { name: String },
    /// The codes did not match.
    Wrong,
    /// One this window does not know, with or without data.
    Other,
}

impl<'de> Deserialize<'de> for PairResult {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Read by hand: serde's `other` takes only unknown names without data.
        let v = serde_json::Value::deserialize(d)?;
        Ok(match &v {
            serde_json::Value::String(s) if s == "wrong" => PairResult::Wrong,
            serde_json::Value::Object(m) => match m.get("paired").and_then(|p| p.get("name")) {
                Some(serde_json::Value::String(name)) => PairResult::Paired { name: name.clone() },
                _ => PairResult::Other,
            },
            _ => PairResult::Other,
        })
    }
}

/// Something wrong, with the service's error code and its text in the PC's language.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Problem {
    pub code: String,
    pub text: String,
}

/// A state and its number: a newer state has a larger revision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub revision: u32,
    pub state: Box<State>,
}

impl Snapshot {
    /// The state from `GetState` or `StateChanged`.
    pub fn parse(revision: u32, json: &str) -> Result<Self, ClientError> {
        let state: State =
            serde_json::from_str(json).map_err(|e| ClientError::Broken(e.to_string()))?;
        if state.v > STATE_VERSION {
            log::warn!(
                "the service's state is version {}, this window reads {STATE_VERSION}",
                state.v
            );
        }
        Ok(Self {
            revision,
            state: Box::new(state),
        })
    }
}

/// What the window asks of the service through `Call`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    AddMonitor {
        w: u32,
        h: u32,
        scale: u16,
    },
    RemoveMonitor {
        id: u32,
    },
    /// Try a failed display again now.
    RetryMonitor {
        id: u32,
    },
    ShowPhysical {
        id: u32,
        shown: bool,
    },
    SetScale {
        id: u32,
        scale: u16,
    },
    SetPrimary {
        id: u32,
    },
    /// Move a display on the desktop; the service checks and may snap it.
    Place {
        id: u32,
        x: i32,
        y: i32,
    },
    /// Open the pairing window; replies `{"seconds_left": n}`.
    PairNewHeadset,
    StopPairing,
    /// The code a headset shows, typed here.
    PairingCode {
        code: String,
    },
    ForgetHeadset {
        headset: String,
    },
    SoundToHeadset {
        on: bool,
    },
    Volume {
        level: u8,
    },
    SetMic {
        on: bool,
    },
    Autostart {
        on: bool,
    },
    WindowLook {
        theme: ThemeMode,
        palette: String,
        language: Option<String>,
    },
}

impl Request {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("requests serialise")
    }
}

/// What `Call` answered: a JSON object, `{}` when there is nothing to say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply(pub serde_json::Value);

impl Reply {
    pub fn parse(json: &str) -> Result<Self, ClientError> {
        serde_json::from_str(json)
            .map(Reply)
            .map_err(|e| ClientError::Broken(e.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// No session bus to talk on.
    NoBus(String),
    /// The service is not on the bus.
    NotRunning,
    /// The service said no: its error code (`NotConnected`, `Limit`) and text.
    Refused { code: String, message: String },
    /// No answer in time.
    TimedOut,
    /// An answer that does not read, or the connection broke.
    Broken(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NoBus(e) => write!(f, "no session bus: {e}"),
            ClientError::NotRunning => write!(f, "Prostir is not running"),
            ClientError::Refused { code, message } => write!(f, "{code}: {message}"),
            ClientError::TimedOut => write!(f, "no answer in time"),
            ClientError::Broken(e) => write!(f, "{e}"),
        }
    }
}

/// A change the service tells about without being asked.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// A new state (`StateChanged`).
    State(Snapshot),
    /// The service appeared on the bus.
    Appeared,
    /// The service left the bus.
    Vanished,
}

/// Where a client puts changes.
pub type Sink = Box<dyn Fn(Change) + Send + Sync>;

/// A way to talk to the service: D-Bus on Linux, a mock in tests and pictures, later a
/// named pipe on Windows. Calls block; the window makes them off its own thread.
/// Calls and changes come from different threads, so a slow call never holds up a
/// change: the client is shared.
pub trait Client: Send + Sync + 'static {
    /// The whole state.
    fn get_state(&self) -> Result<Snapshot, ClientError>;
    /// One request and its reply.
    fn call(&self, request: &Request) -> Result<Reply, ClientError>;
    /// Starts telling `sink` about changes, for as long as the client lives.
    fn watch(&self, sink: Sink) -> Result<(), ClientError>;
    /// Whether the service is there at all (its name has an owner); none: cannot
    /// tell. A service that is there but did not answer is asked again.
    fn present(&self) -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged_objects() {
        let r = Request::AddMonitor {
            w: 1920,
            h: 1080,
            scale: 125,
        };
        assert_eq!(
            r.to_json(),
            r#"{"request":"add_monitor","w":1920,"h":1080,"scale":125}"#
        );
        assert_eq!(
            Request::PairNewHeadset.to_json(),
            r#"{"request":"pair_new_headset"}"#
        );
        let back: Request = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn a_state_reads_leniently() {
        let s = Snapshot::parse(
            7,
            r#"{"v":1,"future":{"x":1},"headsets":[{"name":"Quest 3","session":{"via":"bluetooth","worn":true}}],
                "displays":[{"id":3,"kind":"hologram","state":"failed","failure":{"code":"Desktop","text":"No."}}],
                "pairing":{"seconds_left":100,"last":{"paired":{"name":"Quest 3S"}}}}"#,
        )
        .unwrap();
        assert_eq!(s.revision, 7);
        let (h, session) = s.state.connected().unwrap();
        assert_eq!(h.name, "Quest 3");
        assert_eq!(session.via, Via::Other);
        assert_eq!(s.state.displays[0].kind, DisplayKind::Other);
        assert_eq!(s.state.displays[0].state, DisplayState::Failed);
        assert_eq!(s.state.window, WindowLook::default());
        assert_eq!(
            s.state.pairing.unwrap().last,
            Some(PairResult::Paired {
                name: "Quest 3S".into()
            })
        );
        assert!(matches!(
            Snapshot::parse(1, "[1"),
            Err(ClientError::Broken(_))
        ));
    }

    #[test]
    fn unknown_values_of_a_newer_service_read_as_something_known() {
        let s = Snapshot::parse(
            1,
            r#"{"window":{"theme":"high-contrast"},"pairing":{"last":{"cancelled":{"by":"headset"}}}}"#,
        )
        .unwrap();
        assert_eq!(s.state.window.theme, ThemeMode::System);
        assert_eq!(s.state.pairing.unwrap().last, Some(PairResult::Other));
        let s = Snapshot::parse(
            1,
            r#"{"window":{"theme":"dark"},"pairing":{"last":"timeout"}}"#,
        )
        .unwrap();
        assert_eq!(s.state.window.theme, ThemeMode::Dark);
        assert_eq!(s.state.pairing.unwrap().last, Some(PairResult::Other));
        let s = Snapshot::parse(1, r#"{"pairing":{"last":"wrong"}}"#).unwrap();
        assert_eq!(s.state.pairing.unwrap().last, Some(PairResult::Wrong));
    }

    #[test]
    fn a_full_state_goes_there_and_back() {
        let s = crate::mock::Mock::connected().state().clone();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(*Snapshot::parse(1, &json).unwrap().state, s);
    }
}
