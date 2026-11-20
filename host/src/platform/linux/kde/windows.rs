//! KDE's windows. KWin has no query for them, so a small KWin script does the work
//! and reports back over D-Bus to `dev.prostir.Host`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, bail};
use log::warn;

use crate::platform::linux::screenshot;
use crate::platform::{ActiveWindow, AppWindow, OutputKey, Pointer, WindowList, Windows};

/// KDE's windows and mouse, for the core.
pub struct KdeWindows {
    /// The session bus, to ask KWin for its active output.
    bus: Option<zbus::blocking::Connection>,
}

impl KdeWindows {
    pub fn new() -> Self {
        Self {
            bus: zbus::blocking::Connection::session().ok(),
        }
    }
}

impl Windows for KdeWindows {
    fn active(&mut self) -> Option<ActiveWindow> {
        active()
    }

    /// KWin's active output: the one with the mouse, or with the focused window,
    /// depending on the KDE setting.
    fn active_output(&mut self) -> Option<OutputKey> {
        let reply = self
            .bus
            .as_ref()?
            .call_method(
                Some("org.kde.KWin"),
                "/KWin",
                Some("org.kde.KWin"),
                "activeOutputName",
                &(),
            )
            .ok()?;
        reply
            .body()
            .deserialize::<String>()
            .ok()
            .map(OutputKey::new)
    }

    fn move_to(&mut self, w: &ActiveWindow, output: &OutputKey, fill: bool) -> Result<()> {
        move_to(w, output.as_str(), fill)
    }

    fn put_back(&mut self, w: &ActiveWindow, output: &OutputKey) -> Result<()> {
        put_back(w, output.as_str())
    }

    fn forget_panel(&mut self, output: &OutputKey) {
        kwin_unload(&format!("prostir-panel-{output}"));
    }

    /// KWin reports by itself which panels lost their window.
    fn vacated(&mut self, _panels: &[OutputKey]) -> Vec<OutputKey> {
        vacated().into_iter().map(OutputKey::new).collect()
    }

    /// Spectacle takes the whole desktop: it cannot take one output by name.
    fn screenshot(&mut self, _output: Option<&OutputKey>) -> Result<PathBuf> {
        let mut cmd = std::process::Command::new("spectacle");
        cmd.args(["--background", "--nonotify", "--fullscreen", "--output"]);
        screenshot::take("spectacle", cmd)
    }

    fn list(&self) -> Arc<dyn WindowList> {
        Arc::new(KdeWindowList)
    }
}

impl Pointer for KdeWindows {
    fn position(&mut self) -> Option<(i32, i32)> {
        cursor()
    }

    /// KWin scripts cannot move the mouse.
    fn move_to(&mut self, _x: i32, _y: i32) -> bool {
        false
    }
}

/// Every window, asked of KWin from any thread.
struct KdeWindowList;

impl WindowList for KdeWindowList {
    fn all(&self) -> Vec<AppWindow> {
        all_windows()
    }
}

/// What the KWin scripts report.
enum Report {
    Window(Option<ActiveWindow>),
    /// The mouse position, logical desktop pixels.
    Cursor(i32, i32),
    /// A window on a panel output closed or left it: the output name.
    Left(String),
    /// Every window: process id, class and output, one per line, tab-separated.
    Windows(String),
}

fn active() -> Option<ActiveWindow> {
    let script = format!(
        "const w = workspace.activeWindow;
         if (w && w.normalWindow) {{
           const g = w.frameGeometry;
           callDBus('{BUS}', '{PATH}', '{BUS}', 'Window', w.internalId.toString(),
                    Math.round(g.width), Math.round(g.height),
                    Math.round((w.output ? w.output.devicePixelRatio : 1) * 1000),
                    w.output ? w.output.name : '');
         }} else {{
           callDBus('{BUS}', '{PATH}', '{BUS}', 'Window', '', 0, 0, 1000, '');
         }}"
    );
    ask_kwin(&script, |r| match r {
        Report::Window(w) => Some(w.clone()),
        _ => None,
    })
    .flatten()
}

/// Where the mouse is on the desktop (logical pixels).
fn cursor() -> Option<(i32, i32)> {
    let script = format!(
        "callDBus('{BUS}', '{PATH}', '{BUS}', 'Cursor',
                  Math.round(workspace.cursorPos.x), Math.round(workspace.cursorPos.y));"
    );
    ask_kwin(&script, |r| match r {
        Report::Cursor(x, y) => Some((*x, *y)),
        _ => None,
    })
}

/// Every normal window.
fn all_windows() -> Vec<AppWindow> {
    let script = format!(
        "var lines = [];
         workspace.windowList().forEach(function (w) {{
           if (w.normalWindow && w.output) {{
             lines.push(w.pid + '\\t' + w.resourceClass + '\\t' + w.output.name);
           }}
         }});
         callDBus('{BUS}', '{PATH}', '{BUS}', 'Windows', lines.join('\\n'));"
    );
    ask_kwin(&script, |r| match r {
        Report::Windows(list) => Some(list.clone()),
        _ => None,
    })
    .map(|list| {
        list.lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                Some(AppWindow {
                    pid: f.next()?.parse().ok()?,
                    class: f.next()?.to_owned(),
                    output: OutputKey::new(f.next()?),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Moves window `w` to `output`; `fill`: full screen there (a window panel).
fn move_to(w: &ActiveWindow, output: &str, fill: bool) -> Result<()> {
    let script = move_script(&w.id, output, fill);
    // A panel's script stays loaded to watch its window; others are done at once.
    let name = if fill {
        format!("prostir-panel-{output}")
    } else {
        "prostir-move".to_owned()
    };
    kwin_run(&name, &script, !fill)
}

/// The KWin script that moves window `id` to `output` (and, with `fill`, makes it
/// full screen there and reports when it closes or leaves).
fn move_script(id: &str, output: &str, fill: bool) -> String {
    let id = js_string(id);
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
    format!(
        "const win = workspace.windowList().find(function (w) {{
           return w.internalId.toString() === {id};
         }});
         function place() {{
           const out = workspace.screens.find(function (s) {{ return s.name === {output_js}; }});
           if (!out || !win) return false;
           workspace.sendClientToScreen(win, out);
           {fill_js}
           return true;
         }}
         // A new output can appear a moment later: try again when it does.
         if (!place()) {{
           const retry = function () {{
             if (place()) workspace.screensChanged.disconnect(retry);
           }};
           workspace.screensChanged.connect(retry);
         }}"
    )
}

/// Puts window `w` back on `output` as an ordinary window (out of full screen).
fn put_back(w: &ActiveWindow, output: &str) -> Result<()> {
    let script = format!(
        "const win = workspace.windowList().find(function (w) {{
           return w.internalId.toString() === {id};
         }});
         const out = workspace.screens.find(function (s) {{ return s.name === {output}; }});
         if (win) {{
           win.fullScreen = false;
           if (out) workspace.sendClientToScreen(win, out);
         }}",
        id = js_string(&w.id),
        output = js_string(output),
    );
    kwin_run("prostir-put-back", &script, true)
}

/// Panel outputs whose window closed or left since the last call (KWin reports by
/// itself).
fn vacated() -> Vec<String> {
    let Some(rx) = reports() else {
        return Vec::new();
    };
    // Busy asking KWin something (the sound watcher, say): next time. This runs on the
    // session loop, which must not wait for it.
    let Ok(rx) = rx.try_lock() else {
        return Vec::new();
    };
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

const BUS: &str = "dev.prostir.Host";
const PATH: &str = "/dev/prostir/Host";

struct Host(SyncSender<Report>);

#[zbus::interface(name = "dev.prostir.Host")]
impl Host {
    /// The active window: KWin id (empty: none), logical size, the output's scale in
    /// thousandths (a whole number: KWin sends a scale of 1 as an integer, which
    /// would not match a `double`), output name.
    fn window(&self, id: String, width: i32, height: i32, scale_milli: i32, output: String) {
        let w = (!id.is_empty()).then_some(ActiveWindow {
            id,
            width,
            height,
            scale: f64::from(scale_milli.max(250)) / 1000.0,
            output: (!output.is_empty()).then(|| OutputKey::new(output)),
        });
        let _ = self.0.try_send(Report::Window(w));
    }

    /// Every window, one per line: pid, class, output (tab-separated).
    fn windows(&self, list: String) {
        let _ = self.0.try_send(Report::Windows(list));
    }

    /// The mouse position.
    fn cursor(&self, x: i32, y: i32) {
        let _ = self.0.try_send(Report::Cursor(x, y));
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

/// Runs a KWin script that reports back, and waits (up to 2 s) for the report
/// `pick` accepts. Panel reports that arrive meanwhile are kept for [`vacated`].
fn ask_kwin<T>(script: &str, pick: impl Fn(&Report) -> Option<T>) -> Option<T> {
    let rx = reports()?;
    let rx = rx.lock().unwrap();
    let mut keep = Vec::new();
    // Drop anything stale before asking.
    while let Ok(r) = rx.try_recv() {
        keep.push(r);
    }
    keep.retain(|r| matches!(r, Report::Left(_)));
    if let Err(e) = kwin_run("prostir-ask", script, true) {
        warn!("KWin script: {e:#}");
        return None;
    }
    let mut found = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(r) => match pick(&r) {
                Some(v) => {
                    found = Some(v);
                    break;
                }
                None => keep.push(r),
            },
            Err(_) => break,
        }
    }
    drop(rx);
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
    let path = crate::platform::linux::system::runtime_dir().join(format!("{name}.js"));
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

    /// The generated KWin scripts are valid JavaScript (checked with Node, if present).
    #[test]
    fn kwin_scripts_parse() {
        let dir = std::env::temp_dir().join(format!("prostir-js-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, fill) in [false, true].into_iter().enumerate() {
            let script = move_script("{1234-abcd}", "Virtual-Prostir-11", fill);
            // KWin's globals, for the syntax check only.
            let path = dir.join(format!("move{i}.js"));
            std::fs::write(&path, &script).unwrap();
            match std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .status()
            {
                Ok(status) => assert!(status.success(), "invalid JavaScript:\n{script}"),
                Err(_) => return, // no Node: nothing to check with
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn js_strings_are_quoted() {
        assert_eq!(js_string("a'b\"c"), "\"a'b\\\"c\"");
    }
}
