//! Headset microphone to the PC: captured with AAudio only while a PC app is
//! recording, in 10 ms slices.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use jni::objects::{JObject, JValue};
use log::{info, warn};
use ndk_sys as ffi;
use prostir_proto::{AUDIO_RATE, ClientMsg, MIC_CHANNELS};

use crate::net::Feedback;

const PERMISSION: &str = "android.permission.RECORD_AUDIO";
/// 10 ms per slice.
const FRAMES: usize = AUDIO_RATE as usize / 100;

/// Records until dropped.
/// The user's microphone switch, from the headset's own settings. Recording happens
/// only while this is on, whatever the PC asks.
static ALLOWED: AtomicBool = AtomicBool::new(true);

pub fn set_allowed(allowed: bool) {
    // Switched on again by hand: the system may ask for access once more.
    if allowed && !ALLOWED.load(Ordering::Relaxed) {
        DENIED.store(false, Ordering::Relaxed);
    }
    ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Access to the microphone was refused: no more system prompts each time a PC app
/// starts recording, and the menu says where to allow it.
static DENIED: AtomicBool = AtomicBool::new(false);

pub fn denied() -> bool {
    DENIED.load(Ordering::Relaxed)
}

/// The user pressed "Ask again": show the system prompt now.
pub fn ask_again() {
    DENIED.store(false, Ordering::Relaxed);
    if !has_permission() {
        request_permission();
    }
}

pub fn allowed() -> bool {
    ALLOWED.load(Ordering::Relaxed)
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
}

impl Recorder {
    pub fn start(feedback: Feedback) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let _ = std::thread::Builder::new()
            .name("mic".into())
            .spawn(move || {
                if !ensure_permission(&flag) {
                    return;
                }
                // A broken input stream (an audio route change) is opened again.
                let mut failures = 0;
                while failures < 5 && !flag.load(Ordering::Relaxed) {
                    match Input::open() {
                        Ok(input) => {
                            if record(&input, &feedback, &flag) {
                                return;
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
fn record(input: &Input, feedback: &Feedback, stop: &AtomicBool) -> bool {
    let mut buf = vec![0i16; FRAMES * MIC_CHANNELS as usize];
    info!("headset microphone on");
    while !stop.load(Ordering::Relaxed) {
        match input.read(&mut buf) {
            Ok(0) => continue,
            Ok(frames) => {
                let pcm: Vec<u8> = buf[..(frames * MIC_CHANNELS as usize).min(buf.len())]
                    .iter()
                    .flat_map(|s| s.to_le_bytes())
                    .collect();
                if feedback.send(ClientMsg::Mic { pcm }).is_err() {
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

/// Asks for the microphone permission once (the headset shows a system dialog) and
/// waits up to a minute for the answer.
fn ensure_permission(stop: &AtomicBool) -> bool {
    if has_permission() {
        DENIED.store(false, Ordering::Relaxed);
        return true;
    }
    if DENIED.load(Ordering::Relaxed) {
        return false;
    }
    request_permission();
    let until = Instant::now() + Duration::from_secs(60);
    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
        if has_permission() {
            return true;
        }
    }
    warn!("no microphone permission");
    DENIED.store(true, Ordering::Relaxed);
    false
}

pub(crate) fn with_activity<T>(
    f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<T>,
) -> Option<T> {
    let ctx = ndk_context::android_context();
    // SAFETY: the VM and activity come from the running NativeActivity.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    // SAFETY: the activity object stays alive while the app runs.
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
    let result = f(&mut env, &activity);
    if result.is_err() {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result.ok()
}

fn has_permission() -> bool {
    with_activity(|env, activity| {
        let name = env.new_string(PERMISSION)?;
        let granted = env
            .call_method(
                activity,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
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
        let list = env.new_object_array(1, "java/lang/String", &name)?;
        env.call_method(
            activity,
            "requestPermissions",
            "([Ljava/lang/String;I)V",
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
            check(ffi::AAudio_createStreamBuilder(&mut builder))?;
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
            let opened = ffi::AAudioStreamBuilder_openStream(builder, &mut stream);
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

/// The headset's battery: percent and whether it is charging (`BatteryManager`).
pub fn battery() -> Option<(u8, bool)> {
    with_activity(|env, activity| {
        let name = env.new_string("batterymanager")?;
        let manager = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&name)],
            )?
            .l()?;
        // BatteryManager.BATTERY_PROPERTY_CAPACITY
        let percent = env
            .call_method(&manager, "getIntProperty", "(I)I", &[JValue::Int(4)])?
            .i()?;
        // BATTERY_PROPERTY_STATUS: 2 is charging (isCharging also says yes when full
        // and plugged in).
        let status = env
            .call_method(&manager, "getIntProperty", "(I)I", &[JValue::Int(6)])?
            .i()?;
        let charging = status == 2;
        Ok((percent.clamp(0, 100) as u8, charging))
    })
}
