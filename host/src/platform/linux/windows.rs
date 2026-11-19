//! The PC's windows: the active one's size, moving it to an output (and making it fill
//! it, for a window panel), noticing when it is gone, and screenshots.
//!
//! Hyprland answers over its IPC socket (see `hyprland/windows.rs`); on KDE, KWin
//! scripts do the work (see `kde/windows.rs`).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::info;

use crate::platform::linux::hyprland;
use crate::platform::linux::kde;
use crate::platform::{ActiveWindow, run};

pub fn active() -> Option<ActiveWindow> {
    if hyprland::running() {
        return hyprland::windows::active();
    }
    kde::windows::active()
}

/// Where the mouse is on the desktop (logical pixels).
pub fn cursor() -> Option<(i32, i32)> {
    if hyprland::running() {
        return hyprland::windows::cursor();
    }
    kde::windows::cursor()
}

/// Every normal window: (process id, class, output name).
pub fn all_windows() -> Vec<(u32, String, String)> {
    if hyprland::running() {
        return hyprland::windows::all_windows();
    }
    kde::windows::all_windows()
}

/// Whether the mouse can be moved from here (Hyprland; KWin scripts cannot).
pub fn can_move_cursor() -> bool {
    hyprland::running()
}

/// Puts the mouse at `x`,`y` (logical desktop pixels).
pub fn move_cursor(x: i32, y: i32) -> bool {
    hyprland::running() && hyprland::windows::move_cursor(x, y)
}

/// Moves window `w` to `output`; `fill`: full screen there (a window panel).
pub fn move_to(w: &ActiveWindow, output: &str, fill: bool) -> Result<()> {
    if hyprland::running() {
        return hyprland::windows::move_to(w, output, fill);
    }
    kde::windows::move_to(w, output, fill)
}

/// Puts window `w` back on `output` as an ordinary window (out of full screen): a
/// window panel being closed.
pub fn put_back(w: &ActiveWindow, output: &str) -> Result<()> {
    if hyprland::running() {
        return hyprland::windows::put_back(w, output);
    }
    kde::windows::put_back(w, output)
}

/// Stops watching the window of panel `output` (KDE).
pub fn forget_panel(output: &str) {
    if !hyprland::running() {
        kde::windows::forget_panel(output);
    }
}

/// Panel outputs whose window closed or left since the last call. `panels`: the
/// outputs to check (Hyprland is asked directly; KWin reports by itself).
pub fn vacated(panels: &[String]) -> Vec<String> {
    if hyprland::running() {
        return hyprland::windows::vacated(panels);
    }
    kde::windows::vacated()
}

/// Saves a picture of `output` (or of the whole desktop) to the Pictures folder.
pub fn screenshot(output: Option<&str>) -> Result<PathBuf> {
    let path = screenshot_path()?;
    let (tool, mut cmd) = if hyprland::running() {
        let mut cmd = std::process::Command::new("grim");
        if let Some(o) = output {
            cmd.args(["-o", o]);
        }
        ("grim", cmd)
    } else {
        let mut cmd = std::process::Command::new("spectacle");
        cmd.args(["--background", "--nonotify", "--fullscreen", "--output"]);
        ("spectacle", cmd)
    };
    // Saving a picture of several large screens takes a few seconds.
    let status = match run(cmd.arg(&path), Duration::from_secs(20)) {
        Ok(out) => out.status,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{tool} is not installed"),
        Err(e) => return Err(e).context(tool),
    };
    if !status.success() || !path.exists() {
        bail!("the screenshot tool failed ({status})");
    }
    info!("screenshot saved to {path:?}");
    Ok(path)
}

/// A new file for a screenshot: `Prostir/<date and time>.png` in the Pictures folder.
pub fn screenshot_path() -> Result<PathBuf> {
    let dir = pictures_dir().join("Prostir");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {dir:?}"))?;
    Ok(dir.join(format!("{}.png", timestamp())))
}

fn pictures_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    run(
        std::process::Command::new("xdg-user-dir").arg("PICTURES"),
        Duration::from_secs(2),
    )
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
