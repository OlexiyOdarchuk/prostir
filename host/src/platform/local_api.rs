//! The local API on D-Bus (see `crate::api` and `LOCAL-API.md`): the service on the
//! session bus, and the client `prostir ctl` and `prostir open` use. The same on
//! every Linux desktop and with the fake one (the end-to-end tests run it on a
//! private bus). Only programs of the same user reach the session bus, so the same
//! trust as the tray, which shows the same things.
//!
//! The service runs on zbus's own thread; its methods only hand a request to the
//! control loop and wait (asleep) for the reply. One more thread, `local-api`,
//! takes what the control loop sends out (states, events, replies) in order: a
//! reply never goes out before the `StateChanged` with what it changed, and
//! `StateChanged` goes out at most ten times a second. Each of them sleeps until
//! something comes, so an idle PC still does not wake.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};
use zbus::blocking::Connection;

use crate::api::{
    self, Answer, ApiError, BUS_NAME, CtlReply, ERROR_PREFIX, Failure, INTERFACE, Note,
    OBJECT_PATH, Outlet, Reply, Request, STATE_VERSION, STATES_PER_SECOND,
};
use crate::ctl::{ClientError, CtlClient, Watched};
use crate::events::{Event, EventSink};

/// The state as `GetState` gives it: its revision and its JSON.
type Shown = Arc<Mutex<(u32, String)>>;

/// The service while it runs: dropping it leaves the bus.
pub struct Service {
    _bus: Connection,
    outlet: Outlet,
}

impl Service {
    /// Leaves the bus once every reply given so far went out (`quit` is answered
    /// too), waiting a second at most.
    pub fn finish(self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.outlet.send(Note::Answer(Box::new(move |_| {
            let _ = tx.send(());
        })));
        let _ = rx.recv_timeout(Duration::from_secs(1));
        // The replies handed to zbus a moment ago are written by its own thread.
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Offers the API on the session bus: requests go to the control loop as events,
/// and what the control loop sends to `outlet`'s other end (`notes`) goes out. `None`
/// (logged) without a session bus, or when another program has the name.
pub fn serve(events: EventSink, outlet: Outlet, notes: Receiver<Note>) -> Option<Service> {
    // The first state, there before the name is: `GetState` always has one.
    let mut first = (0, String::new());
    if let Ok(Note::State(revision, state)) = notes.try_recv() {
        first = (revision, state.to_json());
    }
    let shown: Shown = Arc::new(Mutex::new(first));
    let object = Object {
        events,
        outlet: outlet.clone(),
        shown: shown.clone(),
    };
    let bus = session()
        .and_then(|b| b.name(BUS_NAME))
        .and_then(|b| b.serve_at(OBJECT_PATH, object))
        .and_then(|b| b.build());
    let bus = match bus {
        Ok(bus) => bus,
        Err(e) => {
            warn!("no local API on D-Bus ({BUS_NAME}): {e}");
            return None;
        }
    };
    let out = bus.clone();
    let started = std::thread::Builder::new()
        .name("local-api".into())
        .spawn(move || send_out(&out, &notes, &shown));
    if let Err(e) = started {
        warn!("no local API on D-Bus: {e}");
        return None;
    }
    info!("local API on D-Bus as {BUS_NAME}");
    Some(Service { _bus: bus, outlet })
}

/// A connection to the session bus, its socket connected here: zbus connecting by
/// itself does it on a thread of its pool, which stays and wakes twice a second for
/// good, and an idle PC must not wake at all.
fn session() -> zbus::Result<zbus::blocking::connection::Builder<'static>> {
    use std::os::unix::net::UnixStream;
    use zbus::address::transport::{Transport, UnixSocket};
    let address = zbus::Address::session()?;
    if let Transport::Unix(unix) = address.transport() {
        let stream = match unix.path() {
            UnixSocket::File(path) => Some(UnixStream::connect(path)?),
            UnixSocket::Abstract(name) => {
                use std::os::linux::net::SocketAddrExt;
                use std::os::unix::ffi::OsStrExt;
                let at = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
                Some(UnixStream::connect_addr(&at)?)
            }
            _ => None,
        };
        if let Some(stream) = stream {
            return Ok(zbus::blocking::connection::Builder::async_io_unix_stream(
                stream,
            ));
        }
    }
    zbus::blocking::connection::Builder::session()
}

/// The shortest time between two `StateChanged`.
const STATE_GAP: Duration = Duration::from_millis(1000 / STATES_PER_SECOND as u64);

/// Sends out what the control loop gives, in order, until it is gone.
fn send_out(bus: &Connection, notes: &Receiver<Note>, shown: &Shown) {
    // A state not sent yet (it came too soon after the last one), and when the last
    // one went.
    let mut waiting: Option<u32> = None;
    let mut last: Option<Instant> = None;
    let emit = |revision: u32, last: &mut Option<Instant>| {
        let json = shown.lock().unwrap().1.clone();
        signal(bus, "StateChanged", &(revision, json));
        *last = Some(Instant::now());
    };
    loop {
        let note = match (waiting, last) {
            (Some(_), Some(at)) => {
                let left = (at + STATE_GAP).saturating_duration_since(Instant::now());
                match notes.recv_timeout(left) {
                    Ok(note) => Some(note),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            _ => match notes.recv() {
                Ok(note) => Some(note),
                Err(_) => return,
            },
        };
        match note {
            None => {
                if let Some(revision) = waiting.take() {
                    emit(revision, &mut last);
                }
            }
            Some(Note::State(revision, state)) => {
                *shown.lock().unwrap() = (revision, state.to_json());
                if last.is_some_and(|at| at.elapsed() < STATE_GAP) {
                    waiting = Some(revision);
                } else {
                    waiting = None;
                    emit(revision, &mut last);
                }
            }
            Some(note) => {
                // Whatever comes next finds the state out already.
                if let Some(revision) = waiting.take() {
                    emit(revision, &mut last);
                }
                match note {
                    Note::Event(event) => headset_signal(bus, &event),
                    Note::Answer(deliver) => deliver(shown.lock().unwrap().0),
                    Note::State(..) => unreachable!("handled above"),
                }
            }
        }
    }
}

fn signal<B>(bus: &Connection, name: &str, body: &B)
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    if let Err(e) = bus.emit_signal(None::<()>, OBJECT_PATH, INTERFACE, name, body) {
        warn!("D-Bus signal {name}: {e}");
    }
}

fn headset_signal(bus: &Connection, event: &api::Event) {
    match event {
        api::Event::HeadsetConnected { headset, name, via } => signal(
            bus,
            "HeadsetConnected",
            &(headset.as_str(), name.as_str(), via.name()),
        ),
        api::Event::HeadsetDisconnected { headset, reason } => signal(
            bus,
            "HeadsetDisconnected",
            &(headset.as_str(), reason.name()),
        ),
        api::Event::HeadsetWorn { headset, worn } => {
            signal(bus, "HeadsetWorn", &(headset.as_str(), *worn))
        }
    }
}

/// The D-Bus errors, one for each [`ApiError`]: `io.github.OlexiyOdarchuk.Prostir1.Error.*`.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "io.github.OlexiyOdarchuk.Prostir1.Error")]
enum BusError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Unsupported(String),
    NotAllowed(String),
    UnknownDisplay(String),
    Limit(String),
    BadSize(String),
    LastDisplay(String),
    NoWindow(String),
    Desktop(String),
    Encoder(String),
    Capture(String),
    Sound(String),
    NotOnDisplays(String),
    NoSuchProfile(String),
    BadArgument(String),
    NotConnected(String),
    TimedOut(String),
    Disconnected(String),
    Other(String),
}

impl From<Failure> for BusError {
    fn from(f: Failure) -> Self {
        let text = f.text;
        match f.error {
            ApiError::Unsupported => Self::Unsupported(text),
            ApiError::NotAllowed => Self::NotAllowed(text),
            ApiError::UnknownDisplay => Self::UnknownDisplay(text),
            ApiError::Limit => Self::Limit(text),
            ApiError::BadSize => Self::BadSize(text),
            ApiError::LastDisplay => Self::LastDisplay(text),
            ApiError::NoWindow => Self::NoWindow(text),
            ApiError::Desktop => Self::Desktop(text),
            ApiError::Encoder => Self::Encoder(text),
            ApiError::Capture => Self::Capture(text),
            ApiError::Sound => Self::Sound(text),
            ApiError::NotOnDisplays => Self::NotOnDisplays(text),
            ApiError::NoSuchProfile => Self::NoSuchProfile(text),
            ApiError::BadArgument => Self::BadArgument(text),
            ApiError::NotConnected => Self::NotConnected(text),
            ApiError::TimedOut => Self::TimedOut(text),
            ApiError::Disconnected => Self::Disconnected(text),
            ApiError::Other => Self::Other(text),
        }
    }
}

/// The object on the bus.
struct Object {
    events: EventSink,
    outlet: Outlet,
    shown: Shown,
}

impl Object {
    /// Hands `request` to the control loop and waits for what came of it (asleep).
    async fn ask(&self, request: Request) -> Result<Answer, BusError> {
        let (tx, rx) = async_channel::bounded(1);
        let outlet = self.outlet.clone();
        let reply = Reply::new(move |reply: CtlReply| {
            outlet.send(Note::Answer(Box::new(move |revision| {
                let _ = tx.try_send(reply.answer(revision));
            })));
        });
        if !self.events.send(Event::Ctl(request, reply)) {
            return Err(Failure::new(ApiError::Other, "Prostir is quitting").into());
        }
        let answer = rx
            .recv()
            .await
            .map_err(|_| BusError::from(Failure::new(ApiError::Other, "Prostir is quitting")))?;
        match answer.failure() {
            Some(failure) => Err(failure.into()),
            None => Ok(answer),
        }
    }

    /// Asks and leaves out what the answer says.
    async fn done(&self, request: Request) -> Result<(), BusError> {
        self.ask(request).await.map(|_| ())
    }
}

#[zbus::interface(name = "io.github.OlexiyOdarchuk.Prostir1")]
impl Object {
    /// Any request, as JSON; the answer as JSON (a failure is a D-Bus error).
    async fn call(&self, request_json: String) -> Result<String, BusError> {
        let request = Request::from_json(&request_json)?;
        Ok(self.ask(request).await?.to_json())
    }

    /// The whole state: its revision and its JSON.
    fn get_state(&self) -> (u32, String) {
        self.shown.lock().unwrap().clone()
    }

    async fn recenter(&self) -> Result<(), BusError> {
        self.done(Request::Recenter).await
    }

    async fn line_up(&self) -> Result<(), BusError> {
        self.done(Request::LineUp).await
    }

    async fn show_mouse(&self) -> Result<(), BusError> {
        self.done(Request::ShowMouse).await
    }

    async fn reset_arrangement(&self) -> Result<(), BusError> {
        self.done(Request::ResetArrangement).await
    }

    async fn profile(&self, name: String) -> Result<(), BusError> {
        self.done(Request::Profile { name }).await
    }

    async fn cinema(&self, on: bool) -> Result<(), BusError> {
        self.done(Request::Cinema { on }).await
    }

    async fn focus(&self, on: bool) -> Result<(), BusError> {
        self.done(Request::Focus { on }).await
    }

    async fn follow(&self, on: bool) -> Result<(), BusError> {
        self.done(Request::Follow { on }).await
    }

    async fn set_mic(&self, on: bool) -> Result<(), BusError> {
        self.done(Request::SetMic { on }).await
    }

    async fn no_limits(&self, on: bool) -> Result<(), BusError> {
        self.done(Request::NoLimits { on }).await
    }

    /// Closer (below 0) or further.
    async fn distance(&self, steps: i16) -> Result<(), BusError> {
        let steps = i8::try_from(steps)
            .map_err(|_| Failure::new(ApiError::BadArgument, "steps: -128 to 127"))?;
        self.done(Request::Distance { steps }).await
    }

    async fn preset(&self, preset: u8) -> Result<(), BusError> {
        self.done(Request::Preset { preset }).await
    }

    /// A virtual monitor: its id.
    async fn add_monitor(&self, w: u32, h: u32, scale_percent: u16) -> Result<u32, BusError> {
        let request = Request::AddMonitor {
            w,
            h,
            scale: scale_percent,
        };
        Ok(self.ask(request).await?.id.unwrap_or_default())
    }

    async fn remove_monitor(&self, id: u32) -> Result<(), BusError> {
        self.done(Request::RemoveMonitor { id }).await
    }

    async fn show_physical(&self, id: u32, shown: bool) -> Result<(), BusError> {
        self.done(Request::ShowPhysical { id, shown }).await
    }

    async fn set_quality(&self, mbit: u32) -> Result<(), BusError> {
        self.done(Request::SetQuality { mbit }).await
    }

    /// Opens pairing: the seconds it stays open.
    async fn pair_new_headset(&self) -> Result<u32, BusError> {
        Ok(self
            .ask(Request::PairNewHeadset)
            .await?
            .seconds_left
            .unwrap_or_default())
    }

    async fn stop_pairing(&self) -> Result<(), BusError> {
        self.done(Request::StopPairing).await
    }

    async fn forget_headset(&self, fingerprint_or_name: String) -> Result<(), BusError> {
        self.done(Request::ForgetHeadset {
            headset: fingerprint_or_name,
        })
        .await
    }

    async fn show_settings(&self) -> Result<(), BusError> {
        self.done(Request::ShowSettings).await
    }

    async fn quit(&self) -> Result<(), BusError> {
        self.done(Request::Quit).await
    }

    /// The PC app's version.
    #[zbus(property)]
    fn version(&self) -> String {
        prostir_proto::app_version(env!("CARGO_PKG_VERSION"))
    }

    /// The `v` of the state's JSON.
    #[zbus(property)]
    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    #[zbus(signal)]
    async fn state_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        revision: u32,
        state_json: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn headset_connected(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        headset: &str,
        name: &str,
        via: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn headset_disconnected(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        headset: &str,
        reason: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn headset_worn(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        headset: &str,
        worn: bool,
    ) -> zbus::Result<()>;
}

/// The client: `prostir ctl` and `prostir open` on the session bus.
pub struct Client {
    bus: Connection,
}

impl Client {
    /// The session bus, with a reply waited for `timeout` at most.
    pub fn connect(timeout: Duration) -> Result<Self, ClientError> {
        session()
            .map(|b| b.method_timeout(timeout))
            .and_then(|b| b.build())
            .map(|bus| Self { bus })
            .map_err(|e| ClientError::NoBus(e.to_string()))
    }

    fn method(&self, name: &str, body: &impl SerializeBody) -> Result<zbus::Message, ClientError> {
        body.call(&self.bus, name).map_err(client_error)
    }
}

/// A method call's body (a helper so `method` takes any tuple).
trait SerializeBody {
    fn call(&self, bus: &Connection, name: &str) -> zbus::Result<zbus::Message>;
}

impl<B: serde::Serialize + zbus::zvariant::DynamicType> SerializeBody for B {
    fn call(&self, bus: &Connection, name: &str) -> zbus::Result<zbus::Message> {
        bus.call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), name, self)
    }
}

/// A bus error as a client understands it.
fn client_error(e: zbus::Error) -> ClientError {
    match e {
        zbus::Error::MethodError(name, text, _) => {
            let name = name.as_str();
            let text = text.unwrap_or_default();
            if let Some(error) = name.strip_prefix(ERROR_PREFIX) {
                return ClientError::Refused(Failure::new(ApiError::from_name(error), text));
            }
            match name {
                "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
                | "org.freedesktop.DBus.Error.UnknownObject" => ClientError::NotRunning,
                "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout" => {
                    ClientError::TimedOut
                }
                _ => ClientError::Broken(format!("{name}: {text}")),
            }
        }
        zbus::Error::InputOutput(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            ClientError::TimedOut
        }
        e => ClientError::Broken(e.to_string()),
    }
}

fn broken(e: impl std::fmt::Display) -> ClientError {
    ClientError::Broken(e.to_string())
}

impl CtlClient for Client {
    fn call(&self, request: &Request) -> Result<Answer, ClientError> {
        let msg = self.method("Call", &(request.to_json(),))?;
        let json: String = msg.body().deserialize().map_err(broken)?;
        serde_json::from_str(&json).map_err(broken)
    }

    fn state(&self) -> Result<(u32, String), ClientError> {
        let msg = self.method("GetState", &())?;
        msg.body().deserialize().map_err(broken)
    }

    fn running(&self) -> bool {
        let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&self.bus) else {
            return false;
        };
        dbus.name_has_owner(BUS_NAME.try_into().expect("a valid name"))
            .unwrap_or(false)
    }

    fn watch(&self, each: &mut dyn FnMut(Watched) -> bool) -> Result<(), ClientError> {
        use zbus::message::Type;
        let dbus = zbus::blocking::fdo::DBusProxy::new(&self.bus).map_err(broken)?;
        let ours = zbus::MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(INTERFACE)
            .and_then(|b| b.path(OBJECT_PATH))
            .map_err(broken)?
            .build();
        let owner = zbus::MatchRule::builder()
            .msg_type(Type::Signal)
            .sender("org.freedesktop.DBus")
            .and_then(|b| b.interface("org.freedesktop.DBus"))
            .and_then(|b| b.member("NameOwnerChanged"))
            .and_then(|b| b.arg(0, BUS_NAME))
            .map_err(broken)?
            .build();
        let messages = zbus::blocking::MessageIterator::from(&self.bus);
        dbus.add_match_rule(ours).map_err(broken)?;
        dbus.add_match_rule(owner).map_err(broken)?;
        for msg in messages {
            let msg = msg.map_err(broken)?;
            let header = msg.header();
            if header.message_type() != Type::Signal {
                continue;
            }
            let member = header.member().map(|m| m.to_string()).unwrap_or_default();
            let body = msg.body();
            let seen = match member.as_str() {
                "NameOwnerChanged" => match body.deserialize::<(String, String, String)>() {
                    Ok((name, _, new)) if name == BUS_NAME => Some(if new.is_empty() {
                        Watched::Vanished
                    } else {
                        Watched::Appeared
                    }),
                    _ => None,
                },
                "StateChanged" => body
                    .deserialize::<(u32, String)>()
                    .ok()
                    .map(|(revision, json)| Watched::State(revision, json)),
                "HeadsetConnected" => body.deserialize::<(String, String, String)>().ok().map(
                    |(headset, name, via)| {
                        Watched::Event(api::Event::HeadsetConnected {
                            headset,
                            name,
                            via: match via.as_str() {
                                "usb" => api::Via::Usb,
                                "wifi" => api::Via::Wifi,
                                _ => api::Via::Other,
                            },
                        })
                    },
                ),
                "HeadsetDisconnected" => {
                    body.deserialize::<(String, String)>()
                        .ok()
                        .map(|(headset, reason)| {
                            Watched::Event(api::Event::HeadsetDisconnected {
                                headset,
                                reason: api::Reason::from_name(&reason),
                            })
                        })
                }
                "HeadsetWorn" => {
                    body.deserialize::<(String, bool)>()
                        .ok()
                        .map(|(headset, worn)| {
                            Watched::Event(api::Event::HeadsetWorn { headset, worn })
                        })
                }
                _ => None,
            };
            if let Some(seen) = seen
                && !each(seen)
            {
                return Ok(());
            }
        }
        Ok(())
    }
}
