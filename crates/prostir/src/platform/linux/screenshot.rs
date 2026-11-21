//! Pictures of the screen, saved to the Pictures folder by the desktop's own tool
//! (Spectacle on KDE, grim on Hyprland).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::info;

use super::run;

/// Saves a picture with `tool`, run as `cmd` with the path to write to added. The
/// tool writes a file of its own, not there before, which then takes the place of
/// the name [`screenshot_path`] made: a file that was there already, or that the tool
/// left empty, is no picture.
pub fn take(tool: &str, mut cmd: Command) -> Result<PathBuf> {
    let path = screenshot_path()?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let fresh = path.with_file_name(format!(".{name}.part.png"));
    let _ = std::fs::remove_file(&fresh);
    let saved = (|| {
        // Saving a picture of several large screens takes a few seconds.
        let out = match run(cmd.arg(&fresh), Duration::from_secs(20)) {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{tool} is not installed"),
            Err(e) => return Err(e).context(tool.to_owned()),
        };
        if !out.status.success() || !has_picture(&fresh) {
            let said = String::from_utf8_lossy(&out.stderr);
            bail!("{tool} failed ({}): {}", out.status, said.trim());
        }
        std::fs::rename(&fresh, &path).with_context(|| format!("{fresh:?} to {path:?}"))
    })();
    if let Err(e) = saved {
        let _ = std::fs::remove_file(&fresh);
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    info!("screenshot saved to {path:?}");
    Ok(path)
}

/// Whether `path` is a file with something in it.
pub fn has_picture(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// Where screenshots go: `Prostir` in the Pictures folder.
pub fn screenshot_dir() -> PathBuf {
    pictures_dir().join("Prostir")
}

/// A new, empty file for a screenshot, made here so no other picture has its name:
/// `Prostir/<date and time>.<milliseconds>.png` in the Pictures folder, with `-2`,
/// `-3` and so on before `.png` where that name is taken.
pub fn screenshot_path() -> Result<PathBuf> {
    let dir = screenshot_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {dir:?}"))?;
    new_file(&dir, &timestamp())
}

/// Makes a file of a name no file there has, `<stem>.png` or `<stem>-<n>.png`.
pub fn new_file(dir: &Path, stem: &str) -> Result<PathBuf> {
    // Made with `create_new`: of two pictures in the same millisecond, the second
    // gets the next number.
    for n in 0..100 {
        let name = if n == 0 {
            format!("{stem}.png")
        } else {
            format!("{stem}-{}.png", n + 1)
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {path:?}")),
        }
    }
    bail!("no free name for a screenshot in {dir:?}")
}

fn pictures_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    run(
        Command::new("xdg-user-dir").arg("PICTURES"),
        Duration::from_secs(2),
    )
    .ok()
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .map(|s| PathBuf::from(s.trim()))
    .filter(|p| p.is_absolute() && *p != home)
    .unwrap_or_else(|| home.join("Pictures"))
}

/// Local time as `2026-11-18 14-05-09.042` (to the millisecond).
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = libc::time_t::try_from(now.as_secs()).unwrap_or_default();
    // SAFETY: plain libc time calls on a zeroed `tm`.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const secs, &raw mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}.{:03}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            now.subsec_millis()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each new file has a name of its own, also within one millisecond, and a file
    /// that is there already is never taken.
    #[test]
    fn every_screenshot_gets_a_name_of_its_own() {
        let dir = std::env::temp_dir().join(format!("prostir-shots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("same.png"), b"old").unwrap();
        let mut names = std::collections::BTreeSet::new();
        for _ in 0..5 {
            let path = new_file(&dir, "same").unwrap();
            assert!(names.insert(path.clone()), "{path:?} twice");
            assert!(!has_picture(&path), "made empty");
        }
        assert_eq!(std::fs::read(dir.join("same.png")).unwrap(), b"old");
        assert!(timestamp().contains('.'), "{}", timestamp());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
