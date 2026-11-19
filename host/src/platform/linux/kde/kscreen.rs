//! KScreen through `kscreen-doctor`: where KDE's outputs are, and changing them.

use std::collections::HashMap;
use std::time::Duration;

use log::{info, warn};

use crate::platform::{Geometry, run};

/// KScreen answers within a second; applying a layout can take a little longer.
const TIMEOUT: Duration = Duration::from_secs(5);

fn kscreen_doctor(args: &[impl AsRef<std::ffi::OsStr>]) -> std::io::Result<std::process::Output> {
    run(
        std::process::Command::new("kscreen-doctor").args(args),
        TIMEOUT,
    )
}

/// Logical geometry `(x, y, width, height)` of each enabled output, from KScreen.
pub fn geometries() -> HashMap<String, Geometry> {
    let Ok(out) = kscreen_doctor(&["-o"]) else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut name = None;
    let mut map: HashMap<String, Geometry> = HashMap::new();
    // kscreen-doctor prints "priority N" before "Geometry:".
    let mut priorities: HashMap<String, Option<u32>> = HashMap::new();
    let mut disabled: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = strip_ansi(raw);
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Output:") {
            name = rest.split_whitespace().nth(1).map(str::to_owned);
        } else if let (Some(n), "disabled") = (&name, line) {
            disabled.push(n.clone());
        } else if let (Some(n), Some(rest)) = (&name, line.strip_prefix("priority ")) {
            if let Some(g) = map.get_mut(n) {
                g.priority = rest.trim().parse().ok();
            } else {
                priorities.insert(n.clone(), rest.trim().parse().ok());
            }
        } else if let (Some(n), Some(rest)) = (&name, line.strip_prefix("Geometry:")) {
            let nums: Vec<i32> = rest
                .split(|c: char| !c.is_ascii_digit() && c != '-')
                .filter_map(|t| t.parse().ok())
                .collect();
            if let [x, y, w, h] = nums[..] {
                let priority = priorities.get(n).copied().flatten();
                map.insert(
                    n.clone(),
                    Geometry {
                        x,
                        y,
                        w,
                        h,
                        priority,
                        enabled: true,
                    },
                );
            }
        }
    }
    for n in disabled {
        if let Some(g) = map.get_mut(&n) {
            g.enabled = false;
        }
    }
    map
}

/// Connected real displays that KDE has switched off (e.g. the laptop panel after
/// the lid was closed and opened again while other monitors were on).
pub fn disabled_physical_outputs() -> Vec<String> {
    let Ok(out) = kscreen_doctor(&["-o"]) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut name: Option<String> = None;
    let mut found = Vec::new();
    for raw in text.lines() {
        let line: String = strip_ansi(raw);
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Output:") {
            name = rest.split_whitespace().nth(1).map(str::to_owned);
        } else if line == "disabled"
            && let Some(n) = &name
            && !n.starts_with("Virtual-")
        {
            found.push(n.clone());
        }
    }
    found
}

fn strip_ansi(raw: &str) -> String {
    let mut line = String::new();
    let mut esc = false;
    for c in raw.chars() {
        match (esc, c) {
            (false, '\x1b') => esc = true,
            (true, 'm') => esc = false,
            (false, c) => line.push(c),
            _ => {}
        }
    }
    line
}

/// Applies several `output.<name>.<setting>` changes in one kscreen-doctor call, so
/// KDE never sees an in-between (overlapping) layout. Retries while KScreen has not
/// caught up with just-created outputs.
pub fn apply(settings: &[String]) {
    if settings.is_empty() {
        return;
    }
    for _ in 0..10 {
        match kscreen_doctor(settings) {
            Ok(out) if out.status.success() => {
                info!("KDE layout: {}", settings.join(" "));
                return;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => {
                warn!("cannot run kscreen-doctor: {e}");
                return;
            }
        }
    }
    warn!("kscreen-doctor did not apply {}", settings.join(" "));
}

/// Applies `output.<name>.<setting>` through kscreen-doctor, retrying while KScreen
/// has not caught up with a just-created output.
pub fn set(output: &str, setting: &str) {
    let arg = format!("output.{output}.{setting}");
    for _ in 0..10 {
        match kscreen_doctor(&[&arg]) {
            Ok(out) if out.status.success() => {
                info!("{output}: {setting}");
                return;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => {
                warn!("cannot run kscreen-doctor for {arg}: {e}");
                return;
            }
        }
    }
    warn!("kscreen-doctor did not apply {arg}");
}
