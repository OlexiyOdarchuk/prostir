//! Pictures of the screen, saved to the Pictures folder by the desktop's own tool
//! (Spectacle on KDE, grim on Hyprland).

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::info;

use super::run;

/// Saves a picture with `tool`, run as `cmd` with the new file's path added.
pub fn take(tool: &str, mut cmd: Command) -> Result<PathBuf> {
    let path = screenshot_path()?;
    // Saving a picture of several large screens takes a few seconds.
    let out = match run(cmd.arg(&path), Duration::from_secs(20)) {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{tool} is not installed"),
        Err(e) => return Err(e).context(tool.to_owned()),
    };
    if !out.status.success() || !path.exists() {
        let said = String::from_utf8_lossy(&out.stderr);
        bail!("{tool} failed ({}): {}", out.status, said.trim());
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
