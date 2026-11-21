//! Headset microphone to the PC: captured with AAudio only while a PC app is
//! recording, in 10 ms slices, as an up stream (`UpStart`, `Up`, `UpEnd`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use jni::objects::{JObject, JValue};
use jni::{jni_sig, jni_str};
use log::{info, warn};
use ndk_sys as ffi;
use prostir_proto::state::{UpFormat, UpKind, permission};
use prostir_proto::{AUDIO_RATE, ClientMsg, MIC_CHANNELS};

use crate::net::Feedback;

const PERMISSION: &str = "android.permission.RECORD_AUDIO";
/// 10 ms per slice.
const FRAMES: usize = AUDIO_RATE as usize / 100;
/// The microphone of one start of the app: the user's switch, what the system allows,
/// the numbers of its up streams. Made at the start and shared: the menu flips the
/// switch, the network thread records while a PC app asks. A new start in the same
/// process makes its own.
#[derive(Debug)]
pub struct Mic {
    /// The number of the next recording's up stream: each one gets its own, so the
    /// end of an old one, still on its way when the microphone is switched off and on
    /// quickly, cannot end the new one on the PC.
    next_up: AtomicU8,
    /// Whether the system lets the app record, as last found (see
    /// `prostir_proto::state::permission`): the PC shows when it does not.
    allowed_by_system: AtomicU8,
    /// The user's microphone switch, from the headset's own settings. Recording
    /// happens only while this is on, whatever the PC asks.
    allowed: AtomicBool,
    /// Access to the microphone was refused: no more system prompts each time a PC
    /// app starts recording, and the menu says where to allow it.
    denied: AtomicBool,
}

impl Mic {
    /// With the user's switch as the settings have it.
    pub fn new(allowed: bool) -> Self {
        Self {
            next_up: AtomicU8::new(0),
            allowed_by_system: AtomicU8::new(permission::UNKNOWN),
            allowed: AtomicBool::new(allowed),
            denied: AtomicBool::new(false),
        }
    }

    pub fn permission(&self) -> u8 {
        self.allowed_by_system.load(Ordering::Relaxed)
    }

    /// Looks whether the system lets the app record now (one call into Android).
    pub fn refresh_permission(&self) {
        let now = if has_permission() {
            permission::GRANTED
        } else if self.denied.load(Ordering::Relaxed) {
            permission::DENIED
        } else {
            permission::UNKNOWN
        };
        self.allowed_by_system.store(now, Ordering::Relaxed);
    }

    pub fn set_allowed(&self, allowed: bool) {
        // Switched on again by hand: the system may ask for access once more.
        if allowed && !self.allowed.load(Ordering::Relaxed) {
            self.denied.store(false, Ordering::Relaxed);
        }
        self.allowed.store(allowed, Ordering::Relaxed);
    }

    pub fn denied(&self) -> bool {
        self.denied.load(Ordering::Relaxed)
    }

    /// The user pressed "Ask again": show the system prompt now.
    pub fn ask_again(&self) {
        self.denied.store(false, Ordering::Relaxed);
        if !has_permission() {
            request_permission();
        }
    }

    pub fn allowed(&self) -> bool {
        self.allowed.load(Ordering::Relaxed)
    }

    /// Asks for the microphone permission once (the headset shows a system dialog)
    /// and waits up to a minute for the answer.
    fn ensure_permission(&self, stop: &AtomicBool) -> bool {
        if has_permission() {
            self.denied.store(false, Ordering::Relaxed);
            self.allowed_by_system
                .store(permission::GRANTED, Ordering::Relaxed);
            return true;
        }
        if self.denied.load(Ordering::Relaxed) {
            self.allowed_by_system
                .store(permission::DENIED, Ordering::Relaxed);
            return false;
        }
        self.allowed_by_system
            .store(permission::ASKING, Ordering::Relaxed);
        request_permission();
        let until = Instant::now() + Duration::from_secs(60);
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            if has_permission() {
                self.allowed_by_system
                    .store(permission::GRANTED, Ordering::Relaxed);
                return true;
            }
        }
        warn!("no microphone permission");
        self.allowed_by_system
            .store(permission::DENIED, Ordering::Relaxed);
        self.denied.store(true, Ordering::Relaxed);
        false
    }
}

/// Records until dropped.
pub struct Recorder {
    stop: Arc<AtomicBool>,
}

impl Recorder {
    pub fn start(mic: Arc<Mic>, feedback: Feedback) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let up = mic.next_up.fetch_add(1, Ordering::Relaxed);
        let _ = std::thread::Builder::new()
            .name("mic".into())
            .spawn(move || {
                if !mic.ensure_permission(&flag) {
                    return;
                }
                let start = ClientMsg::UpStart {
                    up,
                    kind: UpKind::Mic,
                    format: UpFormat::Pcm16 {
                        rate: AUDIO_RATE,
                        channels: MIC_CHANNELS as u8,
                    },
                };
                if feedback.send(start).is_err() {
                    return;
                }
                // A broken input stream (an audio route change) is opened again.
                let mut failures = 0;
                while failures < 5 && !flag.load(Ordering::Relaxed) {
                    match Input::open() {
                        Ok(input) => {
                            if record(&input, up, &feedback, &flag) {
                                break;
                            }
                            failures = 0;
                        }
                        Err(e) => {
                            warn!("no microphone: AAudio error {e}");
                            failures += 1;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
                let _ = feedback.send(ClientMsg::UpEnd { up });
            })
            .inspect_err(|e| warn!("no microphone: {e}"));
        Self { stop }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Records until stopped (`true`) or the stream breaks (`false`).
fn record(input: &Input, up: u8, feedback: &Feedback, stop: &AtomicBool) -> bool {
    let mut buf = vec![0i16; FRAMES * MIC_CHANNELS as usize];
    info!("headset microphone on");
    while !stop.load(Ordering::Relaxed) {
        match input.read(&mut buf) {
            Ok(0) => {}
            Ok(frames) => {
                let pcm: Vec<u8> = buf[..(frames * MIC_CHANNELS as usize).min(buf.len())]
                    .iter()
                    .flat_map(|s| s.to_le_bytes())
                    .collect();
                if feedback.send(ClientMsg::Up { up, data: pcm }).is_err() {
                    break;
                }
            }
            Err(e) => {
                warn!("microphone: AAudio error {e}");
                return false;
            }
        }
    }
    info!("headset microphone off");
    true
}

/// Runs `f` with the app's context: the Application that `ndk_context` holds
/// (android-activity puts it there), not the activity. Enough for system services and
/// permissions; what needs the Activity itself takes it from the `AndroidApp`.
pub(crate) fn with_activity<T>(
    f: impl FnOnce(&mut jni::Env<'_>, &JObject<'_>) -> jni::errors::Result<T>,
) -> Option<T> {
    let ctx = ndk_context::android_context();
    // SAFETY: the VM of the running NativeActivity.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) };
    // In a local frame of its own; a Java exception on the way is caught and cleared,
    // and comes back as the error.
    vm.attach_current_thread(|env| {
        // SAFETY: a global reference to the Application, kept for the life of the
        // process; `JObject` only wraps it, it never deletes it.
        let activity = unsafe { JObject::from_raw(env, ctx.context().cast()) };
        f(env, &activity)
    })
    .inspect_err(|e| warn!("a call into Android failed: {e}"))
    .ok()
}

fn has_permission() -> bool {
    with_activity(|env, activity| {
        let name = env.new_string(PERMISSION)?;
        let granted = env
            .call_method(
                activity,
                jni_str!("checkSelfPermission"),
                jni_sig!("(Ljava/lang/String;)I"),
                &[JValue::Object(&name)],
            )?
            .i()?;
        Ok(granted == 0) // PackageManager.PERMISSION_GRANTED
    })
    .unwrap_or(false)
}

fn request_permission() {
    with_activity(|env, activity| {
        let name = env.new_string(PERMISSION)?;
        let list = env.new_object_array(1, jni_str!("java/lang/String"), &name)?;
        env.call_method(
            activity,
            jni_str!("requestPermissions"),
            jni_sig!("([Ljava/lang/String;I)V"),
            &[JValue::Object(&list), JValue::Int(1)],
        )?;
        Ok(())
    });
}

/// An AAudio input stream (through `ndk-sys`, like the player).
struct Input(*mut ffi::AAudioStream);

// SAFETY: used by the mic thread only.
unsafe impl Send for Input {}

impl Input {
    fn open() -> Result<Self, i32> {
        // SAFETY: plain AAudio calls; the builder is deleted on every path.
        unsafe {
            let mut builder = std::ptr::null_mut();
            check(ffi::AAudio_createStreamBuilder(&raw mut builder))?;
            ffi::AAudioStreamBuilder_setDirection(builder, ffi::AAUDIO_DIRECTION_INPUT as i32);
            ffi::AAudioStreamBuilder_setFormat(builder, ffi::AAUDIO_FORMAT_PCM_I16);
            ffi::AAudioStreamBuilder_setChannelCount(builder, MIC_CHANNELS as i32);
            ffi::AAudioStreamBuilder_setSampleRate(builder, AUDIO_RATE as i32);
            ffi::AAudioStreamBuilder_setPerformanceMode(
                builder,
                ffi::AAUDIO_PERFORMANCE_MODE_LOW_LATENCY as i32,
            );
            ffi::AAudioStreamBuilder_setInputPreset(
                builder,
                ffi::AAUDIO_INPUT_PRESET_VOICE_COMMUNICATION as i32,
            );
            let mut stream = std::ptr::null_mut();
            let opened = ffi::AAudioStreamBuilder_openStream(builder, &raw mut stream);
            ffi::AAudioStreamBuilder_delete(builder);
            check(opened)?;
            let input = Self(stream);
            check(ffi::AAudioStream_requestStart(stream))?;
            Ok(input)
        }
    }

    /// Reads up to `buf.len()` samples, waiting at most 100 ms; returns frames read.
    fn read(&self, buf: &mut [i16]) -> Result<usize, i32> {
        let frames = (buf.len() / MIC_CHANNELS as usize) as i32;
        // SAFETY: `buf` has room for `frames` frames; the stream is open.
        let n =
            unsafe { ffi::AAudioStream_read(self.0, buf.as_mut_ptr().cast(), frames, 100_000_000) };
        if n < 0 { Err(n) } else { Ok(n as usize) }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: opened by `Input::open`, closed once.
        unsafe {
            ffi::AAudioStream_requestStop(self.0);
            ffi::AAudioStream_close(self.0);
        }
    }
}

fn check(result: i32) -> Result<(), i32> {
    if result < 0 { Err(result) } else { Ok(()) }
}
