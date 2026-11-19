//! Talking to the person at the PC: a dialog for the pairing code (kdialog, zenity or
//! a dmenu-style launcher) and desktop notifications (notify-send).

use std::io::ErrorKind;
use std::process::Command;
use std::time::Duration;

use log::warn;

use crate::i18n::t;
use crate::platform::{NoCode, run};

/// Asks for the code the headset shows, in a small dialog on the PC.
pub fn ask_code() -> Result<String, NoCode> {
    let text = t(
        "Шолом хоче підключитися.\nВведіть код, який він показує:",
        "A headset wants to connect.\nType the code it shows:",
    );
    // Launchers in dmenu mode (common on Hyprland and Sway) print what was typed; they
    // show one line, so the prompt is shorter.
    let prompt = t("Код із шолома: ", "Code from the headset: ");
    let attempts: [(&str, Vec<&str>); 5] = [
        ("kdialog", vec!["--title", "Prostir", "--inputbox", text]),
        ("zenity", vec!["--entry", "--title=Prostir", "--text", text]),
        ("fuzzel", vec!["--dmenu", "--lines=0", "--prompt", prompt]),
        ("wofi", vec!["--dmenu", "--lines=1", "--prompt", prompt]),
        ("rofi", vec!["-dmenu", "-l", "0", "-p", prompt]),
    ];
    for (program, args) in attempts {
        // Nobody at the PC: the dialog goes away rather than holding the PC.
        let out = match run(Command::new(program).args(&args), Duration::from_secs(120)) {
            Ok(out) => out,
            Err(e) if e.kind() == ErrorKind::NotFound => continue, // not installed
            Err(_) => return Err(NoCode::Cancelled),               // timed out
        };
        if !out.status.success() {
            return Err(NoCode::Cancelled);
        }
        let code: String = String::from_utf8_lossy(&out.stdout)
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
        return if code.is_empty() {
            Err(NoCode::Cancelled)
        } else {
            Ok(code)
        };
    }
    warn!(
        "no dialog program (kdialog, zenity, fuzzel, wofi, rofi): cannot ask for the pairing code"
    );
    Err(NoCode::NoDialog)
}

/// Desktop notification, best effort.
pub fn notify(title: &str, body: &str) {
    if cfg!(test) {
        return;
    }
    let child = Command::new("notify-send")
        .args(["--app-name=Prostir", "--icon=prostir", title, body])
        .spawn();
    // Reaped in the background, so finished notify-send processes do not pile up.
    if let Ok(mut child) = child {
        let _ = std::thread::Builder::new()
            .name("notify".into())
            .spawn(move || child.wait());
    }
}

/// Desktop notification, waiting until it is handed over; `true` when it was shown.
pub fn notify_and_wait(title: &str, body: &str) -> bool {
    run(
        Command::new("notify-send").args(["--app-name=Prostir", "--icon=prostir", title, body]),
        Duration::from_secs(10),
    )
    .is_ok_and(|out| out.status.success())
}
