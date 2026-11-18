//! Hyprland's windows, over its IPC socket: the active one, moving it, the mouse.

use anyhow::{Result, bail};
use log::warn;
use serde_json::Value;

use super::request;
use crate::platform::ActiveWindow;

pub fn active() -> Option<ActiveWindow> {
    let json = request("j/activewindow").ok()?;
    let v: Value = serde_json::from_str(&json).ok()?;
    let size = v.get("size")?.as_array()?;
    let monitor = v.get("monitor").and_then(Value::as_i64);
    let on = monitors()
        .into_iter()
        .find(|(id, _, _)| Some(*id) == monitor);
    Some(ActiveWindow {
        id: v.get("address")?.as_str()?.to_owned(),
        width: size.first()?.as_i64()? as i32,
        height: size.get(1)?.as_i64()? as i32,
        scale: on.as_ref().map_or(1.0, |m| m.2),
        output: on.map(|m| m.1).unwrap_or_default(),
    })
}

/// Where the mouse is on the desktop (logical pixels).
pub fn cursor() -> Option<(i32, i32)> {
    let json = request("j/cursorpos").ok()?;
    let v: Value = serde_json::from_str(&json).ok()?;
    Some((v.get("x")?.as_i64()? as i32, v.get("y")?.as_i64()? as i32))
}

/// Every window: (process id, class, output name).
pub fn all_windows() -> Vec<(u32, String, String)> {
    let monitors = monitors();
    let Ok(json) = request("j/clients") else {
        return Vec::new();
    };
    let Ok(Value::Array(clients)) = serde_json::from_str::<Value>(&json) else {
        return Vec::new();
    };
    clients
        .iter()
        .filter_map(|c| {
            let pid = c.get("pid")?.as_i64()? as u32;
            let class = c.get("class")?.as_str()?.to_owned();
            let monitor = c.get("monitor")?.as_i64()?;
            let output = monitors.iter().find(|m| m.0 == monitor)?.1.clone();
            Some((pid, class, output))
        })
        .collect()
}

/// Puts the mouse at `x`,`y` (logical desktop pixels).
pub fn move_cursor(x: i32, y: i32) -> bool {
    dispatch(
        &format!("hl.dsp.cursor.move({{x = {x}, y = {y}}})"),
        &format!("movecursor {x} {y}"),
    )
}

/// Moves window `w` to `output`; `fill`: full screen there (a window panel).
pub fn move_to(w: &ActiveWindow, output: &str, fill: bool) -> Result<()> {
    let address = &w.id;
    // Focus it first: the dispatchers act on the active window.
    dispatch(
        &format!("hl.dsp.focus({{window = \"address:{address}\"}})"),
        &format!("focuswindow address:{address}"),
    );
    if !dispatch(
        &format!("hl.dsp.window.move({{monitor = \"{output}\"}})"),
        &format!("movewindow mon:{output}"),
    ) {
        bail!("Hyprland did not move the window");
    }
    if fill {
        dispatch(
            "hl.dsp.window.fullscreen({mode = \"fullscreen\"})",
            "fullscreen 0",
        );
    }
    Ok(())
}

/// Puts window `w` back on `output` as an ordinary window (out of full screen).
pub fn put_back(w: &ActiveWindow, output: &str) -> Result<()> {
    let address = &w.id;
    dispatch(
        &format!("hl.dsp.focus({{window = \"address:{address}\"}})"),
        &format!("focuswindow address:{address}"),
    );
    dispatch(
        "hl.dsp.window.fullscreen_state({internal = 0, client = 0})",
        "fullscreenstate 0 0",
    );
    move_to(w, output, false)
}

/// Panel outputs (of `panels`) with no window left on them.
pub fn vacated(panels: &[String]) -> Vec<String> {
    if panels.is_empty() {
        return Vec::new();
    }
    let Ok(json) = request("j/clients") else {
        return Vec::new();
    };
    let Ok(Value::Array(clients)) = serde_json::from_str::<Value>(&json) else {
        return Vec::new();
    };
    let monitors = monitors();
    panels
        .iter()
        .filter(|name| {
            let Some(id) = monitors.iter().find(|m| &m.1 == *name).map(|m| m.0) else {
                return true;
            };
            !clients.iter().any(|c| {
                c.get("monitor").and_then(Value::as_i64) == Some(id)
                    && c.get("mapped").and_then(Value::as_bool) != Some(false)
            })
        })
        .cloned()
        .collect()
}

/// (id, name, scale) of every Hyprland output.
fn monitors() -> Vec<(i64, String, f64)> {
    let Ok(json) = request("j/monitors all") else {
        return Vec::new();
    };
    let Ok(Value::Array(all)) = serde_json::from_str::<Value>(&json) else {
        return Vec::new();
    };
    all.iter()
        .filter_map(|m| {
            Some((
                m.get("id")?.as_i64()?,
                m.get("name")?.as_str()?.to_owned(),
                m.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
            ))
        })
        .collect()
}

/// Runs a dispatcher in Hyprland's Lua config dialect, else the classic one.
fn dispatch(lua: &str, classic: &str) -> bool {
    for command in [format!("dispatch {lua}"), format!("dispatch {classic}")] {
        if let Ok(reply) = request(&command)
            && reply.trim() == "ok"
        {
            return true;
        }
    }
    warn!("Hyprland refused {classic:?}");
    false
}
