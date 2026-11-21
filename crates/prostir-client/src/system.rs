//! What the app asks the system. The `system` thread asks now and then, away from the
//! frame (a call into Java takes its time): for now the battery, once a minute and
//! every 15 s while the palm menu shows, read afresh when it appears. What does not
//! change (the headset's model) is asked once at the start.

#![expect(
    clippy::disallowed_methods,
    reason = "the system thread is part of the shell"
)]

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jni::objects::{JString, JValue};
use jni::strings::JNIStr;
use jni::{jni_sig, jni_str};
use log::warn;

use crate::mic::with_activity;

/// How often the battery is read, and while the palm menu shows it.
const BATTERY_EVERY: Duration = Duration::from_secs(60);
const BATTERY_EVERY_ON_PALM: Duration = Duration::from_secs(15);
/// How long the app waits for the thread when it closes.
const STOP_WITHIN: Duration = Duration::from_millis(300);

/// What the thread found.
pub enum SystemEvent {
    /// The battery: percent and whether it is charging; `None` when Android did not
    /// say.
    Battery(Option<(u8, bool)>),
}

enum SystemCmd {
    /// The palm menu (which shows the battery) appeared or went.
    PalmMenu(bool),
}

/// The running thread, for one start of the app: it stops when this is dropped.
pub struct System {
    cmds: Option<Sender<SystemCmd>>,
    events: Receiver<SystemEvent>,
    thread: Option<JoinHandle<()>>,
}

impl System {
    /// Starts the thread; the battery was just read (see [`battery`]), so its first
    /// reading comes a minute later.
    pub fn start() -> Self {
        let (cmds, from_app) = mpsc::channel();
        let (to_app, events) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("system".into())
            .spawn(move || run(&from_app, &to_app))
            .inspect_err(|e| warn!("no system thread: {e}"))
            .ok();
        Self {
            cmds: Some(cmds),
            events,
            thread,
        }
    }

    /// The palm menu appeared (`true`) or went.
    pub fn palm_menu(&self, shown: bool) {
        if let Some(cmds) = &self.cmds {
            let _ = cmds.send(SystemCmd::PalmMenu(shown));
        }
    }

    /// The next thing the thread found, if there is one; never waits.
    pub fn try_recv(&self) -> Option<SystemEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for System {
    fn drop(&mut self) {
        // Its channel closes, and the thread ends at once (unless in a call into Java).
        self.cmds = None;
        if let Some(thread) = self.thread.take() {
            join_within("system", thread, STOP_WITHIN);
        }
    }
}

fn run(cmds: &Receiver<SystemCmd>, events: &Sender<SystemEvent>) {
    let mut palm = false;
    let mut read_at = Instant::now();
    loop {
        let every = if palm {
            BATTERY_EVERY_ON_PALM
        } else {
            BATTERY_EVERY
        };
        let wait = (read_at + every).saturating_duration_since(Instant::now());
        let read = match cmds.recv_timeout(wait) {
            Ok(SystemCmd::PalmMenu(shown)) => {
                let appeared = shown && !palm;
                palm = shown;
                appeared
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if read {
            read_at = Instant::now();
            if events.send(SystemEvent::Battery(battery())).is_err() {
                return;
            }
        }
    }
}

/// Waits up to `limit` for `thread` to end. One that does not is left running, and the
/// log says so: a new start of the app in the same process does not wait for it.
pub fn join_within(name: &str, thread: JoinHandle<()>, limit: Duration) {
    let until = Instant::now() + limit;
    while !thread.is_finished() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !thread.is_finished() {
        warn!(
            "the {name} thread did not stop within {} ms",
            limit.as_millis()
        );
    } else if thread.join().is_err() {
        warn!("the {name} thread panicked");
    }
}

/// What the headset is, as the system names it (`Build.MANUFACTURER` and
/// `Build.MODEL`, see [`crate::device::model_name`]); empty when it does not say.
pub fn model() -> String {
    let said = with_activity(|env, _| {
        let mut field = |name: &JNIStr| -> jni::errors::Result<String> {
            let value = env
                .get_static_field(
                    jni_str!("android/os/Build"),
                    name,
                    jni_sig!("Ljava/lang/String;"),
                )?
                .l()?;
            if value.is_null() {
                return Ok(String::new());
            }
            env.cast_local::<JString<'_>>(value)?.try_to_string(env)
        };
        Ok((field(jni_str!("MANUFACTURER"))?, field(jni_str!("MODEL"))?))
    });
    let (manufacturer, model) = said.unwrap_or_default();
    crate::device::model_name(&manufacturer, &model)
}

/// The headset's battery: percent and whether it is charging (`BatteryManager`).
pub fn battery() -> Option<(u8, bool)> {
    with_activity(|env, activity| {
        let name = env.new_string("batterymanager")?;
        let manager = env
            .call_method(
                activity,
                jni_str!("getSystemService"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                &[JValue::Object(&name)],
            )?
            .l()?;
        let property = |env: &mut jni::Env<'_>, id| {
            env.call_method(
                &manager,
                jni_str!("getIntProperty"),
                jni_sig!("(I)I"),
                &[JValue::Int(id)],
            )?
            .i()
        };
        // BatteryManager.BATTERY_PROPERTY_CAPACITY
        let percent = property(env, 4)?;
        // BATTERY_PROPERTY_STATUS: 2 is charging (isCharging also says yes when full
        // and plugged in).
        let status = property(env, 6)?;
        let charging = status == 2;
        Ok((percent.clamp(0, 100) as u8, charging))
    })
}
