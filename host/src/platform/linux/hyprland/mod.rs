//! Hyprland: virtual monitors are headless outputs (`output create headless`), placed
//! with monitor rules, and captured with `wlr-screencopy` (see `screencopy.rs`).
//!
//! Everything goes through Hyprland's IPC socket, the same one `hyprctl` uses.
//! Monitor rules are set with Lua (`eval`) on Lua configs (Hyprland 0.53 and newer)
//! and with `keyword` on `hyprland.conf` ones.

pub mod screencopy;
pub mod windows;

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{info, warn};
use serde_json::Value;

use crate::platform::{Geometry, MonitorSpec};

/// Names of Prostir's outputs; anything else is a real display.
pub const PREFIX: &str = "Prostir-";

/// Whether Prostir runs inside Hyprland.
pub fn running() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

fn socket_path() -> Option<PathBuf> {
    socket("socket")
}

/// `.socket.sock` takes requests, `.socket2.sock` streams events.
fn socket(kind: &str) -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let instance = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    Some(
        PathBuf::from(runtime)
            .join("hypr")
            .join(instance)
            .join(format!(".{kind}.sock")),
    )
}

/// One IPC request, e.g. `j/monitors all` or `output create headless Prostir-1`.
pub fn request(command: &str) -> Result<String> {
    let path = socket_path().context("not running inside Hyprland")?;
    let mut socket =
        UnixStream::connect(&path).with_context(|| format!("cannot reach Hyprland at {path:?}"))?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.write_all(command.as_bytes())?;
    let mut reply = String::new();
    socket.read_to_string(&mut reply)?;
    Ok(reply)
}

/// An output as Hyprland reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub name: String,
    /// Mode in pixels.
    pub width: i32,
    pub height: i32,
    pub refresh: f64,
    /// Logical position.
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub focused: bool,
    pub disabled: bool,
}

impl Monitor {
    fn from_json(v: &Value) -> Option<Self> {
        let int = |k: &str| v.get(k)?.as_i64().map(|n| n as i32);
        Some(Self {
            name: v.get("name")?.as_str()?.to_owned(),
            width: int("width")?,
            height: int("height")?,
            refresh: v.get("refreshRate").and_then(Value::as_f64).unwrap_or(60.0),
            x: int("x")?,
            y: int("y")?,
            scale: v.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
            focused: v.get("focused").and_then(Value::as_bool).unwrap_or(false),
            disabled: v.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// Size on the desktop, after scaling.
    pub fn logical(&self) -> (i32, i32) {
        (
            (f64::from(self.width) / self.scale).round() as i32,
            (f64::from(self.height) / self.scale).round() as i32,
        )
    }
}

pub fn parse_monitors(json: &str) -> Vec<Monitor> {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(Monitor::from_json).collect())
        })
        .unwrap_or_default()
}

/// All outputs, including disabled ones.
pub fn monitors() -> Vec<Monitor> {
    request("j/monitors all")
        .map(|json| parse_monitors(&json))
        .unwrap_or_default()
}

/// The output with the mouse (Hyprland's focused monitor).
pub fn focused() -> Option<String> {
    let json = request("j/monitors").ok()?;
    parse_monitors(&json)
        .into_iter()
        .find(|m| m.focused)
        .map(|m| m.name)
}

pub fn geometries() -> HashMap<String, Geometry> {
    monitors()
        .into_iter()
        .map(|m| {
            let (w, h) = m.logical();
            let g = Geometry {
                x: m.x,
                y: m.y,
                w,
                h,
                priority: None,
                enabled: !m.disabled,
            };
            (m.name, g)
        })
        .collect()
}

/// The display to mirror: an external monitor if one is on (the one in front of you,
/// as in KDE where the laptop panel goes off with the lid), else the laptop panel, but
/// never the panel of a closed laptop.
pub fn physical_output() -> Option<String> {
    pick_physical(&monitors(), crate::platform::linux::system::lid_open())
}

fn pick_physical(monitors: &[Monitor], lid_open: bool) -> Option<String> {
    let mut names: Vec<&str> = monitors
        .iter()
        // HEADLESS-N: virtual outputs of other tools (Sunshine, wayvnc), not displays.
        .filter(|m| !m.disabled && !m.name.starts_with(PREFIX) && !m.name.starts_with("HEADLESS-"))
        .map(|m| m.name.as_str())
        .filter(|n| lid_open || !is_panel(n))
        .collect();
    names.sort_by_key(|n| (is_panel(n), *n));
    names.first().map(|n| (*n).to_owned())
}

fn is_panel(name: &str) -> bool {
    ["eDP", "LVDS", "DSI"].iter().any(|p| name.starts_with(p))
}

/// A monitor rule: mode, logical position and scale of one output.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub refresh: f64,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
}

impl Rule {
    fn mode(&self) -> String {
        let refresh = format!("{:.3}", self.refresh);
        let refresh = refresh.trim_end_matches('0').trim_end_matches('.');
        format!("{}x{}@{refresh}", self.width, self.height)
    }

    pub fn lua(&self) -> String {
        format!(
            "hl.monitor({{output=\"{}\", mode=\"{}\", position=\"{}x{}\", scale={}}})",
            self.name,
            self.mode(),
            self.x,
            self.y,
            self.scale
        )
    }

    pub fn keyword(&self) -> String {
        format!(
            "keyword monitor {},{},{}x{},{}",
            self.name,
            self.mode(),
            self.x,
            self.y,
            self.scale
        )
    }
}

/// Every rule Prostir has set, by output: a config reload drops runtime rules, and
/// these are put back afterwards.
static APPLIED: Mutex<BTreeMap<String, Rule>> = Mutex::new(BTreeMap::new());

/// Which way monitor rules are accepted: 0 not known yet, 1 Lua, 2 keyword.
static DIALECT: AtomicU8 = AtomicU8::new(0);

fn ok(reply: &str) -> bool {
    reply.trim() == "ok" || reply.lines().all(|l| l.trim() == "ok")
}

/// Applies monitor rules in one request and remembers them.
pub fn apply(rules: &[Rule]) {
    let mut applied = APPLIED.lock().unwrap();
    for r in rules {
        applied.insert(r.name.clone(), r.clone());
    }
    drop(applied);
    send(rules);
}

/// Hyprland re-reads its config whenever the file is saved, which resets our outputs
/// to its own rules. Watches for that and puts Prostir's layout back.
pub fn keep_rules_after_reloads() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("hyprland-events".into())
            .spawn(|| {
                loop {
                    if let Some(path) = socket("socket2")
                        && let Ok(stream) = UnixStream::connect(path)
                    {
                        for line in BufReader::new(stream).lines() {
                            let Ok(line) = line else { break };
                            if line.starts_with("configreloaded>>") {
                                std::thread::sleep(Duration::from_millis(200));
                                reapply();
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
        if let Err(e) = spawned {
            warn!("cannot watch Hyprland events: {e}");
        }
    });
}

/// Puts back the remembered rules: always for our outputs, and for real displays only
/// while they are on (the config may have just switched one off, e.g. a closed lid).
fn reapply() {
    let present = monitors();
    let rules: Vec<Rule> = APPLIED
        .lock()
        .unwrap()
        .values()
        .filter(|r| {
            r.name.starts_with(PREFIX) || present.iter().any(|m| m.name == r.name && !m.disabled)
        })
        .cloned()
        .collect();
    if !rules.is_empty() {
        info!(
            "Hyprland reloaded its config: restoring {} monitor rules",
            rules.len()
        );
        send(&rules);
    }
}

fn send(rules: &[Rule]) {
    if rules.is_empty() {
        return;
    }
    let lua = || {
        format!(
            "eval {}",
            rules.iter().map(Rule::lua).collect::<Vec<_>>().join(" ")
        )
    };
    let keyword = || {
        let all: Vec<String> = rules.iter().map(Rule::keyword).collect();
        format!("[[BATCH]]{}", all.join(";"))
    };
    let attempt = |dialect: u8| {
        let command = if dialect == 1 { lua() } else { keyword() };
        match request(&command) {
            Ok(reply) if ok(&reply) => true,
            Ok(reply) => {
                if DIALECT.load(Ordering::Relaxed) != 0 {
                    warn!("Hyprland refused {command:?}: {}", reply.trim());
                }
                false
            }
            Err(e) => {
                warn!("Hyprland: {e:#}");
                false
            }
        }
    };
    match DIALECT.load(Ordering::Relaxed) {
        0 => {
            for dialect in [1, 2] {
                if attempt(dialect) {
                    DIALECT.store(dialect, Ordering::Relaxed);
                    return;
                }
            }
            warn!("Hyprland accepted no monitor rule for {rules:?}");
        }
        d => {
            attempt(d);
        }
    }
}

/// Our output's name for monitor number `number` (1-based).
pub fn output_name(number: u32) -> String {
    format!("{PREFIX}{number}")
}

/// Creates a headless output (or takes over one left from an earlier run) and gives
/// it the size of `spec`. Placement comes later, with the whole layout.
pub fn create(number: u32, spec: &MonitorSpec) -> Result<String> {
    let name = output_name(number);
    if !monitors().iter().any(|m| m.name == name) {
        let reply = request(&format!("output create headless {name}"))?;
        if !ok(&reply) {
            bail!("Hyprland cannot create {name}: {}", reply.trim());
        }
    }
    // Out of the way until the layout is applied, so it overlaps nothing.
    apply(&[Rule {
        name: name.clone(),
        width: spec.width as i32,
        height: spec.height as i32,
        refresh: 60.0,
        x: 100_000 + 10_000 * number as i32,
        y: 0,
        scale: spec.scale,
    }]);
    info!("created {name} ({spec})");
    Ok(name)
}

pub fn remove(name: &str) {
    APPLIED.lock().unwrap().remove(name);
    match request(&format!("output remove {name}")) {
        Ok(reply) if ok(&reply) => info!("removed {name}"),
        Ok(reply) => warn!("Hyprland did not remove {name}: {}", reply.trim()),
        Err(e) => warn!("cannot remove {name}: {e:#}"),
    }
}

/// Removes outputs a previous run left behind (Hyprland keeps headless outputs after
/// the program that made them is gone), so the windows on them come back.
pub fn remove_leftovers() {
    restore_real_outputs();
    for m in monitors() {
        if m.name.starts_with(PREFIX) {
            remove(&m.name);
        }
    }
}

/// Where real displays stood before Prostir moved them, to put them back after.
static ORIGINAL: Mutex<BTreeMap<String, (i32, i32)>> = Mutex::new(BTreeMap::new());

/// Puts the real displays back where they were and forgets the rules for them, so a
/// later config reload does not move them again.
pub fn restore_real_outputs() {
    let original = std::mem::take(&mut *ORIGINAL.lock().unwrap());
    {
        let mut applied = APPLIED.lock().unwrap();
        for name in original.keys() {
            applied.remove(name);
        }
    }
    let current = monitors();
    let rules: Vec<Rule> = original
        .into_iter()
        .filter_map(|(name, (x, y))| {
            let m = current.iter().find(|m| m.name == name && !m.disabled)?;
            ((m.x, m.y) != (x, y)).then_some(Rule {
                name,
                width: m.width,
                height: m.height,
                refresh: m.refresh,
                x,
                y,
                scale: m.scale,
            })
        })
        .collect();
    if !rules.is_empty() {
        info!("putting {} display(s) back where they were", rules.len());
        send(&rules);
    }
}

/// Moves outputs to new logical positions, keeping their modes and scales.
pub fn place(positions: &[(String, i32, i32)]) {
    let current = monitors();
    {
        let mut original = ORIGINAL.lock().unwrap();
        for (name, _, _) in positions {
            if !name.starts_with(PREFIX)
                && let Some(m) = current.iter().find(|m| &m.name == name)
            {
                original.entry(name.clone()).or_insert((m.x, m.y));
            }
        }
    }
    let rules: Vec<Rule> = positions
        .iter()
        .filter_map(|(name, x, y)| {
            let m = current.iter().find(|m| &m.name == name)?;
            ((m.x, m.y) != (*x, *y)).then(|| Rule {
                name: name.clone(),
                width: m.width,
                height: m.height,
                refresh: m.refresh,
                x: *x,
                y: *y,
                scale: m.scale,
            })
        })
        .collect();
    apply_without_overlaps(&current, rules);
}

/// Logical rectangle `(x, y, w, h)` of a rule.
fn rect(r: &Rule) -> (i32, i32, i32, i32) {
    (
        r.x,
        r.y,
        (f64::from(r.width) / r.scale).round() as i32,
        (f64::from(r.height) / r.scale).round() as i32,
    )
}

fn overlap(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// Whether applying `rules` one after another (as Hyprland does) passes through a
/// moment where two outputs overlap.
fn overlaps_on_the_way(current: &[Monitor], rules: &[Rule]) -> bool {
    let mut at: Vec<(String, (i32, i32, i32, i32))> = current
        .iter()
        .filter(|m| !m.disabled)
        .map(|m| {
            let (w, h) = m.logical();
            (m.name.clone(), (m.x, m.y, w, h))
        })
        .collect();
    for r in rules {
        let moved = rect(r);
        if at
            .iter()
            .any(|(n, other)| n != &r.name && overlap(moved, *other))
        {
            return true;
        }
        match at.iter_mut().find(|(n, _)| n == &r.name) {
            Some(entry) => entry.1 = moved,
            None => at.push((r.name.clone(), moved)),
        }
    }
    false
}

/// Applies the rules; if doing so one by one would overlap outputs for a moment
/// (Hyprland warns about that), first moves the outputs involved out of the way.
fn apply_without_overlaps(current: &[Monitor], rules: Vec<Rule>) {
    if overlaps_on_the_way(current, &rules) {
        let aside: Vec<Rule> = rules
            .iter()
            .enumerate()
            .map(|(i, r)| Rule {
                x: 200_000 + 10_000 * i as i32,
                y: 0,
                ..r.clone()
            })
            .collect();
        send(&aside);
    }
    apply(&rules);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITORS: &str = r#"[{"id":0,"name":"eDP-1","width":2880,"height":1800,
        "refreshRate":120.00000,"x":0,"y":0,"scale":2.0,"focused":false,"disabled":false},
        {"id":1,"name":"Prostir-1","width":2560,"height":1440,"refreshRate":60.0,
        "x":1440,"y":0,"scale":1.0,"focused":true,"disabled":false}]"#;

    #[test]
    fn reads_monitors() {
        let m = parse_monitors(MONITORS);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].logical(), (1440, 900));
        assert!(m[1].focused);
        assert_eq!(m[1].name, output_name(1));
    }

    #[test]
    fn writes_rules_in_both_dialects() {
        let r = Rule {
            name: "Prostir-2".into(),
            width: 1920,
            height: 1080,
            refresh: 60.0,
            x: -1920,
            y: 360,
            scale: 1.25,
        };
        assert_eq!(
            r.lua(),
            r#"hl.monitor({output="Prostir-2", mode="1920x1080@60", position="-1920x360", scale=1.25})"#
        );
        assert_eq!(
            r.keyword(),
            "keyword monitor Prostir-2,1920x1080@60,-1920x360,1.25"
        );
        let odd = Rule {
            refresh: 59.951,
            ..r
        };
        assert!(odd.lua().contains("1920x1080@59.951"));
    }

    #[test]
    fn mirrors_the_external_monitor_and_never_a_closed_lid() {
        let mut m = parse_monitors(MONITORS);
        m.push(Monitor {
            name: "HDMI-A-1".into(),
            ..m[0].clone()
        });
        assert_eq!(pick_physical(&m, true).as_deref(), Some("HDMI-A-1"));
        m.pop();
        assert_eq!(pick_physical(&m, true).as_deref(), Some("eDP-1"));
        assert_eq!(pick_physical(&m, false), None);
    }

    #[test]
    fn notices_overlaps_on_the_way() {
        let hdmi = Monitor {
            name: "HDMI-A-1".into(),
            width: 1920,
            height: 1080,
            refresh: 60.0,
            x: 0,
            y: 0,
            scale: 1.0,
            focused: false,
            disabled: false,
        };
        let rule = |name: &str, x, y| Rule {
            name: name.into(),
            width: 2560,
            height: 1440,
            refresh: 60.0,
            x,
            y,
            scale: 1.0,
        };
        let hdmi_down = Rule {
            name: "HDMI-A-1".into(),
            width: 1920,
            height: 1080,
            ..rule("", 320, 2000)
        };
        // Prostir-3 lands on the HDMI display before the HDMI display moves away.
        assert!(overlaps_on_the_way(
            std::slice::from_ref(&hdmi),
            &[rule("Prostir-3", 0, 560), hdmi_down.clone()]
        ));
        // The other way round there is no such moment.
        assert!(!overlaps_on_the_way(
            &[hdmi],
            &[hdmi_down, rule("Prostir-3", 0, 560)]
        ));
    }
}
