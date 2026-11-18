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

use crate::platform::hyprland;

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
    /// The output it is on.
    pub output: String,
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

/// Smallest window panel, in logical pixels.
const MIN_SIZE: (i32, i32) = (320, 200);

pub fn active() -> Option<ActiveWindow> {
    if hyprland::running() {
        let json = hyprland::request("j/activewindow").ok()?;
        let v: Value = serde_json::from_str(&json).ok()?;
        let size = v.get("size")?.as_array()?;
        let monitor = v.get("monitor").and_then(Value::as_i64);
        let on = hyprland_monitors()
            .into_iter()
            .find(|(id, _, _)| Some(*id) == monitor);
        return Some(ActiveWindow {
            id: v.get("address")?.as_str()?.to_owned(),
            width: size.first()?.as_i64()? as i32,
            height: size.get(1)?.as_i64()? as i32,
            scale: on.as_ref().map_or(1.0, |m| m.2),
            output: on.map(|m| m.1).unwrap_or_default(),
        });
    }
    kde_active()
}

/// Where the mouse is on the desktop (logical pixels).
pub fn cursor() -> Option<(i32, i32)> {
    if hyprland::running() {
        let json = hyprland::request("j/cursorpos").ok()?;
        let v: Value = serde_json::from_str(&json).ok()?;
        return Some((v.get("x")?.as_i64()? as i32, v.get("y")?.as_i64()? as i32));
    }
    let script = format!(
        "callDBus('{BUS}', '{PATH}', '{BUS}', 'Cursor',
                  Math.round(workspace.cursorPos.x), Math.round(workspace.cursorPos.y));"
    );
    ask_kwin(&script, |r| match r {
        Report::Cursor(x, y) => Some((*x, *y)),
        _ => None,
    })
}

/// Every normal window: (process id, class, output name).
pub fn all_windows() -> Vec<(u32, String, String)> {
    if hyprland::running() {
        let monitors = hyprland_monitors();
        let Ok(json) = hyprland::request("j/clients") else {
            return Vec::new();
        };
        let Ok(Value::Array(clients)) = serde_json::from_str::<Value>(&json) else {
            return Vec::new();
        };
        return clients
            .iter()
            .filter_map(|c| {
                let pid = c.get("pid")?.as_i64()? as u32;
                let class = c.get("class")?.as_str()?.to_owned();
                let monitor = c.get("monitor")?.as_i64()?;
                let output = monitors.iter().find(|m| m.0 == monitor)?.1.clone();
                Some((pid, class, output))
            })
            .collect();
    }
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
                Some((
                    f.next()?.parse().ok()?,
                    f.next()?.to_owned(),
                    f.next()?.to_owned(),
                ))
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Whether the mouse can be moved from here (Hyprland; KWin scripts cannot).
pub fn can_move_cursor() -> bool {
    hyprland::running()
}

/// Puts the mouse at `x`,`y` (logical desktop pixels).
pub fn move_cursor(x: i32, y: i32) -> bool {
    hyprland::running()
        && dispatch(
            &format!("hl.dsp.cursor.move({{x = {x}, y = {y}}})"),
            &format!("movecursor {x} {y}"),
        )
}

/// Output pixel size for a panel showing `w`: even numbers, at least [`MIN_SIZE`].
pub fn panel_size(w: &ActiveWindow) -> (u32, u32) {
    // At most what an output may be (an odd report from the compositor or a spoofed
    // one must not overflow).
    let px = |logical: i32, min: i32| {
        let p = (f64::from(logical.clamp(min, 8192)) * w.scale.clamp(0.25, 4.0)).round() as u32;
        (p.min(8192) + 1) & !1
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
    let script = kde_move_script(&w.id, output, fill);
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
fn kde_move_script(id: &str, output: &str, fill: bool) -> String {
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

/// Puts window `w` back on `output` as an ordinary window (out of full screen): a
/// window panel being closed.
pub fn put_back(w: &ActiveWindow, output: &str) -> Result<()> {
    if hyprland::running() {
        let address = &w.id;
        dispatch(
            &format!("hl.dsp.focus({{window = \"address:{address}\"}})"),
            &format!("focuswindow address:{address}"),
        );
        dispatch(
            "hl.dsp.window.fullscreen_state({internal = 0, client = 0})",
            "fullscreenstate 0 0",
        );
        return move_to(w, output, false);
    }
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

/// Decodes one keyframe of a monitor's stream into a PNG in the Pictures folder (a
/// screenshot of just that monitor where the desktop's tool cannot take one output).
pub fn keyframe_to_png(data: &[u8], codec: prostir_proto::Codec) -> Result<PathBuf> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    let dir = pictures_dir().join("Prostir");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {dir:?}"))?;
    let path = dir.join(format!("{}.png", timestamp()));
    let (caps, parse) = match codec {
        prostir_proto::Codec::H264 => ("video/x-h264", "h264parse"),
        prostir_proto::Codec::H265 => ("video/x-h265", "h265parse"),
    };
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=src caps={caps},stream-format=byte-stream,alignment=au ! {parse} ! \
         decodebin ! videoconvert ! pngenc snapshot=true ! filesink name=sink"
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let sink = pipeline.by_name("sink").context("no filesink")?;
    sink.set_property("location", path.to_string_lossy().as_ref());
    let src = pipeline
        .by_name("src")
        .context("no appsrc")?
        .downcast::<gstreamer_app::AppSrc>()
        .map_err(|_| anyhow::anyhow!("not an appsrc"))?;
    pipeline.set_state(gst::State::Playing)?;
    let pushed = src
        .push_buffer(gst::Buffer::from_slice(data.to_vec()))
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .and_then(|_| src.end_of_stream().map_err(|e| anyhow::anyhow!("{e:?}")));
    let bus = pipeline.bus().context("no bus")?;
    let ended = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    pushed?;
    match ended.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) if path.exists() => {
            info!("screenshot saved to {path:?}");
            Ok(path)
        }
        Some(gst::MessageView::Error(e)) => bail!("{}", e.error()),
        _ => bail!("the frame was not decoded in time"),
    }
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
    /// The active window: KWin id (empty: none), logical size, the output's scale in
    /// thousandths (a whole number: KWin sends a scale of 1 as an integer, which
    /// would not match a `double`), output name.
    fn window(&self, id: String, width: i32, height: i32, scale_milli: i32, output: String) {
        let w = (!id.is_empty()).then_some(ActiveWindow {
            id,
            width,
            height,
            scale: f64::from(scale_milli.max(250)) / 1000.0,
            output,
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

fn kde_active() -> Option<ActiveWindow> {
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
    let path = crate::runtime_dir().join(format!("{name}.js"));
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
            output: String::new(),
        };
        assert_eq!(panel_size(&w), (1202, 300));
    }

    /// The generated KWin scripts are valid JavaScript (checked with Node, if present).
    #[test]
    fn kwin_scripts_parse() {
        let dir = std::env::temp_dir().join(format!("prostir-js-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, fill) in [false, true].into_iter().enumerate() {
            let script = kde_move_script("{1234-abcd}", "Virtual-Prostir-11", fill);
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

#[cfg(test)]
mod live {
    /// A keyframe from the graphics card's encoder becomes a PNG (run by hand:
    /// `cargo test -- --ignored keyframe`; needs the VA plugin and a GPU).
    #[test]
    #[ignore]
    fn keyframe_becomes_png() {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        gst::init().unwrap();
        let pipeline = gst::parse::launch(
            "videotestsrc num-buffers=1 ! video/x-raw,width=640,height=360 ! vapostproc ! \
             vah265enc ! h265parse config-interval=-1 ! \
             video/x-h265,stream-format=byte-stream,alignment=au ! appsink name=out",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let out = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let sample = out.pull_sample().unwrap();
        let data = sample.buffer().unwrap().map_readable().unwrap().to_vec();
        pipeline.set_state(gst::State::Null).unwrap();
        let path = super::keyframe_to_png(&data, prostir_proto::Codec::H265).unwrap();
        let png = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        eprintln!("decoded {} bytes into a {} byte PNG", data.len(), png.len());
    }
}
