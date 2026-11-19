//! The Plasma shell, through `qdbus` and its scripting interface.

use std::time::Duration;

use log::warn;

use super::kscreen;
use crate::platform::run;

/// Plasma runs a script in well under a second.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `script` in the Plasma shell through whichever qdbus this system has (Arch:
/// qdbus6; Fedora: qdbus-qt6; older setups: qdbus). `None`: no qdbus found.
fn evaluate(script: &str) -> Option<std::io::Result<std::process::Output>> {
    ["qdbus6", "qdbus-qt6", "qdbus"].iter().find_map(|qdbus| {
        let ran = run(
            std::process::Command::new(qdbus).args([
                "org.kde.plasmashell",
                "/PlasmaShell",
                "org.kde.PlasmaShell.evaluateScript",
                script,
            ]),
            TIMEOUT,
        );
        match ran {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            ran => Some(ran),
        }
    })
}

/// The geometry Plasma gives its first screen (the one with the panel):
/// `(x, y, width, height)`.
pub fn first_screen() -> Option<(i32, i32, i32, i32)> {
    const SCRIPT: &str =
        "var g = screenGeometry(0); print(g.x + ' ' + g.y + ' ' + g.width + ' ' + g.height);";
    let out = evaluate(SCRIPT)?.ok().filter(|o| o.status.success())?;
    let nums: Vec<i32> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    match nums[..] {
        [x, y, w, h] => Some((x, y, w, h)),
        _ => None,
    }
}

/// Plasma sometimes keeps its old screen order after outputs come and go quickly,
/// leaving the panel on a former primary. Making another output primary for a moment
/// and then `output` again makes it re-read the order. Runs in the background.
pub fn nudge_primary(output: String, other: String) {
    let _ = std::thread::Builder::new()
        .name("primary".into())
        .spawn(move || {
            kscreen::set(&other, "priority.1");
            std::thread::sleep(Duration::from_millis(800));
            kscreen::set(&output, "priority.1");
        });
}

/// Gives every screen the wallpaper of the primary desktop. Plasma configures
/// wallpapers per screen, so new monitors would otherwise show the default one.
/// Runs a little later because Plasma sets up desktops for new screens asynchronously.
pub fn sync_wallpapers() {
    const SCRIPT: &str = r#"
        var src = desktopForScreen(0);
        if (src) {
            src.currentConfigGroup = ["Wallpaper", src.wallpaperPlugin, "General"];
            var image = src.readConfig("Image");
            desktops().forEach(function (d) {
                if (d.screen < 0 || d.screen == 0) return;
                d.wallpaperPlugin = src.wallpaperPlugin;
                d.currentConfigGroup = ["Wallpaper", src.wallpaperPlugin, "General"];
                if (d.readConfig("Image") != image) d.writeConfig("Image", image);
            });
        }"#;
    let _ = std::thread::Builder::new()
        .name("wallpapers".into())
        .spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            match evaluate(SCRIPT) {
                Some(Ok(out)) if out.status.success() => {}
                Some(Ok(out)) => warn!(
                    "wallpaper sync failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Some(Err(e)) => warn!("wallpaper sync failed: {e}"),
                None => warn!("qdbus not found, wallpapers stay per screen"),
            }
        });
}
