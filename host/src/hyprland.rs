//! Hyprland: virtual monitors are headless outputs (`output create headless`), placed
//! with monitor rules, and captured with `wlr-screencopy` (see `screencopy.rs`).
//!
//! Everything goes through Hyprland's IPC socket, the same one `hyprctl` uses.
//! Monitor rules are set with Lua (`eval`) on Lua configs (Hyprland 0.53 and newer)
//! and with `keyword` on `hyprland.conf` ones.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{info, warn};
use serde_json::Value;

use crate::virtual_output::{Geometry, MonitorSpec};

/// Names of Prostir's outputs; anything else is a real display.
pub const PREFIX: &str = "Prostir-";

/// Whether Prostir runs inside Hyprland.
pub fn running() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let instance = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    Some(
        PathBuf::from(runtime)
            .join("hypr")
            .join(instance)
            .join(".socket.sock"),
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

/// The display to mirror: the laptop panel if there is one, else the first real output.
pub fn physical_output() -> Option<String> {
    let mut names: Vec<String> = monitors()
        .into_iter()
        .filter(|m| !m.disabled && !m.name.starts_with(PREFIX))
        .map(|m| m.name)
        .collect();
    names.sort_by_key(|n| (!is_panel(n), n.clone()));
    names.into_iter().next()
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

/// Which way monitor rules are accepted: 0 not known yet, 1 Lua, 2 keyword.
static DIALECT: AtomicU8 = AtomicU8::new(0);

fn ok(reply: &str) -> bool {
    reply.trim() == "ok" || reply.lines().all(|l| l.trim() == "ok")
}

/// Applies monitor rules in one request.
pub fn apply(rules: &[Rule]) {
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
    match request(&format!("output remove {name}")) {
        Ok(reply) if ok(&reply) => info!("removed {name}"),
        Ok(reply) => warn!("Hyprland did not remove {name}: {}", reply.trim()),
        Err(e) => warn!("cannot remove {name}: {e:#}"),
    }
}

/// Removes outputs a previous run left behind (Hyprland keeps headless outputs after
/// the program that made them is gone), so the windows on them come back.
pub fn remove_leftovers() {
    for m in monitors() {
        if m.name.starts_with(PREFIX) {
            info!("{} is left from an earlier run", m.name);
            remove(&m.name);
        }
    }
}

/// Moves outputs to new logical positions, keeping their modes and scales.
pub fn place(positions: &[(String, i32, i32)]) {
    let current = monitors();
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
}
