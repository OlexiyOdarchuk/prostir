//! The PC's windows: the active one's size, moving it to an output (and making it fill
//! it, for a window panel), noticing when it is gone, and screenshots.
//!
//! Hyprland answers over its IPC socket. KWin has no such query, so on KDE a small
//! KWin script does the work and reports back over D-Bus to `dev.prostir.Host`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{info, warn};
use serde_json::Value;

use crate::hyprland;

/// The window that has the keyboard, as found when asked.
#[derive(Debug, Clone)]
pub struct ActiveWindow {
    /// Hyprland address or KWin internal id.
    pub id: String,
    /// Logical size on the desktop.
    pub width: i32,
    pub height: i32,
    /// Scale of the output it is on.
    pub scale: f64,
}

/// What the KWin scripts report.
enum Report {
    Window(Option<ActiveWindow>),
    /// A window on a panel output closed or left it: the output name.
    Left(String),
}

/// Smallest window panel, in logical pixels.
const MIN_SIZE: (i32, i32) = (320, 200);

pub fn active() -> Option<ActiveWindow> {
    if hyprland::running() {
        let json = hyprland::request("j/activewindow").ok()?;
        let v: Value = serde_json::from_str(&json).ok()?;
        let size = v.get("size")?.as_array()?;
        let monitor = v.get("monitor").and_then(Value::as_i64);
        let scale = hyprland_monitors()
            .into_iter()
            .find(|(id, _, _)| Some(*id) == monitor)
            .map_or(1.0, |m| m.2);
        return Some(ActiveWindow {
            id: v.get("address")?.as_str()?.to_owned(),
            width: size.first()?.as_i64()? as i32,
            height: size.get(1)?.as_i64()? as i32,
            scale,
        });
    }
    kde_active()
}

/// Output pixel size for a panel showing `w`: even numbers, at least [`MIN_SIZE`].
pub fn panel_size(w: &ActiveWindow) -> (u32, u32) {
    let px = |logical: i32, min: i32| {
        let p = (f64::from(logical.max(min)) * w.scale).round() as u32;
        (p + 1) & !1
    };
    (px(w.width, MIN_SIZE.0), px(w.height, MIN_SIZE.1))
}

/// Moves window `w` to `output`; `fill`: full screen there (a window panel).
pub fn move_to(w: &ActiveWindow, output: &str, fill: bool) -> Result<()> {
    if hyprland::running() {
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
        return Ok(());
    }
    let id = js_string(&w.id);
    let output_js = js_string(output);
    let fill_js = if fill {
        // Report when the window closes or leaves, so the panel goes away with it.
        format!(
            "win.fullScreen = true;
             const report = function () {{
               callDBus('{BUS}', '{PATH}', '{BUS}', 'Left', {output_js});
             }};
             win.closed.connect(report);
             win.outputChanged.connect(function () {{
               if (!win.output || win.output.name !== {output_js}) report();
             }});"
        )
    } else {
        String::new()
    };
    let script = format!(
        "const out = workspace.screens.find(function (s) {{ return s.name === {output_js}; }});
         const win = workspace.windowList().find(function (w) {{
           return w.internalId.toString() === {id};
         }});
         if (out && win) {{
           workspace.sendClientToScreen(win, out);
           {fill_js}
         }}"
    );
    // A panel's script stays loaded to watch its window; others are done at once.
    let name = if fill {
        format!("prostir-panel-{output}")
    } else {
        "prostir-move".to_owned()
    };
    kwin_run(&name, &script, !fill)
}

/// Stops watching the window of panel `output` (KDE).
pub fn forget_panel(output: &str) {
    if !hyprland::running() {
        kwin_unload(&format!("prostir-panel-{output}"));
    }
}

/// Panel outputs whose window closed or left since the last call. `panels`: the
/// outputs to check (Hyprland is asked directly; KWin reports by itself).
pub fn vacated(panels: &[String]) -> Vec<String> {
    if hyprland::running() {
        if panels.is_empty() {
            return Vec::new();
        }
        let Ok(json) = hyprland::request("j/clients") else {
            return Vec::new();
        };
        let Ok(Value::Array(clients)) = serde_json::from_str::<Value>(&json) else {
            return Vec::new();
        };
        let monitors = hyprland_monitors();
        return panels
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
            .collect();
    }
    let Some(rx) = reports() else {
        return Vec::new();
    };
    let rx = rx.lock().unwrap();
    let mut out = std::mem::take(&mut *STASH.lock().unwrap());
    while let Ok(r) = rx.try_recv() {
        if let Report::Left(name) = r
            && !out.contains(&name)
        {
            out.push(name);
        }
    }
    out
}

/// Saves a picture of `output` (or of the whole desktop) to the Pictures folder.
pub fn screenshot(output: Option<&str>) -> Result<PathBuf> {
    let dir = pictures_dir().join("Prostir");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {dir:?}"))?;
    let path = dir.join(format!("{}.png", timestamp()));
    let status = if hyprland::running() {
        let mut cmd = std::process::Command::new("grim");
        if let Some(o) = output {
            cmd.args(["-o", o]);
        }
        cmd.arg(&path).status()
    } else {
        std::process::Command::new("spectacle")
            .args(["--background", "--nonotify", "--fullscreen", "--output"])
            .arg(&path)
            .status()
    }
    .context(if hyprland::running() {
        "grim is not installed"
    } else {
        "spectacle is not installed"
    })?;
    if !status.success() || !path.exists() {
        bail!("the screenshot tool failed ({status})");
    }
    info!("screenshot saved to {path:?}");
    Ok(path)
}

fn pictures_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    std::process::Command::new("xdg-user-dir")
        .arg("PICTURES")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.is_absolute() && *p != home)
        .unwrap_or_else(|| home.join("Pictures"))
}

/// Local time as `2026-11-18 14-05-09`.
fn timestamp() -> String {
    // SAFETY: plain libc time calls on a zeroed `tm`.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// (id, name, scale) of every Hyprland output.
fn hyprland_monitors() -> Vec<(i64, String, f64)> {
    let Ok(json) = hyprland::request("j/monitors all") else {
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
        if let Ok(reply) = hyprland::request(&command)
            && reply.trim() == "ok"
        {
            return true;
        }
    }
    warn!("Hyprland refused {classic:?}");
    false
}

// --- KDE: KWin scripts reporting back over D-Bus -----------------------------------

const BUS: &str = "dev.prostir.Host";
const PATH: &str = "/dev/prostir/Host";

struct Host(SyncSender<Report>);

#[zbus::interface(name = "dev.prostir.Host")]
impl Host {
    /// The active window: KWin id (empty: none), logical size, output scale.
    fn window(&self, id: String, width: i32, height: i32, scale: f64) {
        let w = (!id.is_empty()).then_some(ActiveWindow {
            id,
            width,
            height,
            scale,
        });
        let _ = self.0.try_send(Report::Window(w));
    }

    /// The window of panel `output` closed or moved away.
    fn left(&self, output: String) {
        let _ = self.0.try_send(Report::Left(output));
    }
}

/// The D-Bus service KWin scripts call, started on first use.
fn reports() -> Option<&'static Mutex<Receiver<Report>>> {
    static SERVICE: OnceLock<Option<(zbus::blocking::Connection, Mutex<Receiver<Report>>)>> =
        OnceLock::new();
    SERVICE
        .get_or_init(|| {
            let (tx, rx) = sync_channel(16);
            let conn = zbus::blocking::connection::Builder::session()
                .and_then(|b| b.name(BUS))
                .and_then(|b| b.serve_at(PATH, Host(tx)))
                .and_then(|b| b.build())
                .inspect_err(|e| warn!("cannot offer {BUS} on D-Bus: {e}"))
                .ok()?;
            Some((conn, Mutex::new(rx)))
        })
        .as_ref()
        .map(|(_, rx)| rx)
}

fn kde_active() -> Option<ActiveWindow> {
    let rx = reports()?;
    // Drop anything stale before asking.
    let rx = rx.lock().unwrap();
    let mut keep = Vec::new();
    while let Ok(r) = rx.try_recv() {
        keep.push(r);
    }
    let script = format!(
        "const w = workspace.activeWindow;
         if (w && w.normalWindow) {{
           const g = w.frameGeometry;
           callDBus('{BUS}', '{PATH}', '{BUS}', 'Window', w.internalId.toString(),
                    Math.round(g.width), Math.round(g.height),
                    w.output ? w.output.devicePixelRatio : 1.0);
         }} else {{
           callDBus('{BUS}', '{PATH}', '{BUS}', 'Window', '', 0, 0, 1.0);
         }}"
    );
    if let Err(e) = kwin_run("prostir-active", &script, true) {
        warn!("KWin script: {e:#}");
        return None;
    }
    let mut found = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(Report::Window(w)) => {
                found = w;
                break;
            }
            Ok(other) => keep.push(other),
            Err(_) => break,
        }
    }
    drop(rx);
    // Panel reports that arrived meanwhile are not lost.
    for r in keep {
        if let Report::Left(name) = r {
            STASH.lock().unwrap().push(name);
        }
    }
    found
}

/// Panel reports read while waiting for something else.
static STASH: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Loads and runs a KWin script; `once`: unloaded right after it ran.
fn kwin_run(name: &str, source: &str, once: bool) -> Result<()> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("{name}.js"));
    std::fs::write(&path, source)?;
    let bus = zbus::blocking::Connection::session()?;
    kwin_unload(name);
    let reply = bus.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "loadScript",
        &(path.to_string_lossy().as_ref(), name),
    )?;
    let id: i32 = reply.body().deserialize()?;
    if id < 0 {
        bail!("KWin did not load the script");
    }
    bus.call_method(
        Some("org.kde.KWin"),
        format!("/Scripting/Script{id}").as_str(),
        Some("org.kde.kwin.Script"),
        "run",
        &(),
    )?;
    if once {
        // Give it a moment to run before it is unloaded.
        std::thread::sleep(Duration::from_millis(150));
        kwin_unload(name);
    }
    Ok(())
}

fn kwin_unload(name: &str) {
    if let Ok(bus) = zbus::blocking::Connection::session() {
        let _ = bus.call_method(
            Some("org.kde.KWin"),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "unloadScript",
            &(name,),
        );
    }
}

/// A JavaScript string literal.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_size_is_even_and_scaled() {
        let w = ActiveWindow {
            id: String::new(),
            width: 801,
            height: 100,
            scale: 1.5,
        };
        assert_eq!(panel_size(&w), (1202, 300));
    }

    #[test]
    fn js_strings_are_quoted() {
        assert_eq!(js_string("a'b\"c"), "\"a'b\\\"c\"");
    }
}
