//! Starting with the session: an XDG autostart entry.

use std::path::PathBuf;

use log::warn;

use super::system::config_home;

fn file() -> Option<PathBuf> {
    let base = config_home()?;
    Some(base.join("autostart").join("prostir.desktop"))
}

/// Whether this PC can start Prostir with the session from here (Hyprland without
/// uwsm ignores XDG autostart: its own config starts it).
pub fn available() -> bool {
    file().is_some() && !super::hyprland::running()
}

/// Whether Prostir starts with the session.
pub fn enabled() -> bool {
    file().is_some_and(|p| p.exists())
}

/// Turns starting with the session on or off.
pub fn set(enabled: bool) {
    let Some(path) = file() else { return };
    let result = if enabled {
        // A wrapper (Nix) names the program to start; else this very file.
        let exe = std::env::var("PROSTIR_EXE")
            .ok()
            .filter(|e| !e.is_empty())
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(|| "prostir".into());
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Prostir\n\
             Comment=Virtual monitors for Meta Quest\nExec={exe}\nIcon=prostir\n\
             Terminal=false\nX-KDE-autostart-phase=2\n"
        );
        path.parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, entry))
    } else {
        std::fs::remove_file(&path)
    };
    if let Err(e) = result {
        warn!("cannot change autostart ({}): {e}", path.display());
    }
}
