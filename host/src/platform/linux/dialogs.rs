//! Talking to the person at the PC: a dialog for the pairing code (kdialog, zenity or
//! a dmenu-style launcher) and desktop notifications (notify-send).

use std::process::Command;
use std::time::{Duration, Instant};

use log::warn;

use crate::i18n::t;
use crate::platform::NoCode;

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
        let child = Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            continue; // not installed
        };
        // Nobody at the PC: the dialog goes away rather than holding the PC.
        let until = Instant::now() + Duration::from_secs(120);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        if !status.is_some_and(|s| s.success()) {
            return Err(NoCode::Cancelled); // or timed out
        }
        let mut out = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            use std::io::Read;
            let _ = stdout.read_to_string(&mut out);
        }
        let code: String = out.chars().filter(char::is_ascii_digit).collect();
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
    Command::new("notify-send")
        .args(["--app-name=Prostir", "--icon=prostir", title, body])
        .status()
        .is_ok_and(|s| s.success())
}
