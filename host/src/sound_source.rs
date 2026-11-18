//! Which monitor the PC's sound comes from: the window of the app that is playing.
//! PipeWire says which processes play (their streams carry the process id), the
//! compositor says where their windows are. The headset then places the sound on that
//! side (the "sound from the screen" option).

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::windows;

/// Looks every second and a half, off the session loop (asking KWin takes a moment).
pub struct Watcher {
    on: Arc<AtomicBool>,
    found: Arc<Mutex<Option<String>>>,
}

impl Watcher {
    pub fn start() -> Self {
        let on = Arc::new(AtomicBool::new(true));
        let found = Arc::new(Mutex::new(None));
        let (running, slot) = (on.clone(), found.clone());
        let _ = std::thread::Builder::new()
            .name("sound-source".into())
            .spawn(move || {
                while running.load(Ordering::Relaxed) {
                    let now = output();
                    *slot.lock().unwrap() = now;
                    std::thread::sleep(Duration::from_millis(1500));
                }
            });
        Self { on, found }
    }

    /// The output sound comes from, as last found.
    pub fn output(&self) -> Option<String> {
        self.found.lock().unwrap().clone()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.on.store(false, Ordering::Relaxed);
    }
}

/// A stream that is playing: process id and binary name.
fn playing() -> Vec<(u32, String)> {
    let Ok(out) = Command::new("pactl")
        .args(["-f", "json", "list", "sink-inputs"])
        .output()
    else {
        return Vec::new();
    };
    let Ok(Value::Array(inputs)) = serde_json::from_slice::<Value>(&out.stdout) else {
        return Vec::new();
    };
    inputs
        .iter()
        .filter(|i| i.get("corked").and_then(Value::as_bool) == Some(false))
        .filter_map(|i| {
            let props = i.get("properties")?;
            let get = |k: &str| props.get(k).and_then(Value::as_str).unwrap_or("");
            // Prostir's own streams (the headset microphone and the like) do not count.
            let binary = get("application.process.binary");
            if binary.is_empty() || binary == "prostir" || binary.starts_with("pw-") {
                return None;
            }
            Some((
                get("application.process.id").parse().ok()?,
                binary.to_owned(),
            ))
        })
        .collect()
}

/// The parent of process `pid`.
fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ...": comm may hold spaces and parentheses.
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The output whose window plays sound now, if one can be told.
pub fn output() -> Option<String> {
    let playing = playing();
    if playing.is_empty() {
        return None;
    }
    let windows = windows::all_windows();
    for (pid, binary) in &playing {
        // The process itself or one of its parents (browsers play from a helper
        // process).
        let mut p = Some(*pid);
        for _ in 0..6 {
            let Some(id) = p else { break };
            if let Some(w) = windows.iter().find(|w| w.0 == id) {
                return Some(w.2.clone());
            }
            p = parent(id).filter(|&pp| pp > 1);
        }
        // Else by name: a window whose class is the program's name.
        let name = binary.to_lowercase();
        if let Some(w) = windows.iter().find(|w| {
            let class = w.1.to_lowercase();
            !class.is_empty() && (class.contains(&name) || name.contains(&class))
        }) {
            return Some(w.2.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents_are_read_from_proc() {
        let me = std::process::id();
        assert!(parent(me).is_some_and(|p| p > 0 && p != me));
    }
}

#[cfg(test)]
mod live {
    /// Prints what the desktop reports (run by hand: `cargo test -- --ignored live`).
    #[test]
    #[ignore]
    fn live_windows_and_sound() {
        for w in crate::windows::all_windows() {
            eprintln!("window {w:?}");
        }
        eprintln!("playing {:?}", super::playing());
        eprintln!("sound from {:?}", super::output());
    }
}
