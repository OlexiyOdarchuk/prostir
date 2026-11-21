//! Starting Prostir: the settings window is its own window and the tray icon the
//! program minimized, so starting it again by any way in (the menu's plain `prostir`,
//! `prostir serve` of an autostart line or a terminal) opens or brings forward the
//! window of the one running and ends well; a click on the icon does the same; the
//! very first start opens it; `--version` says the version, on every command.

use std::process::Command;

use crate::pc::{Options, Pc, TestDir};
use crate::{SOON, TWO_MONITORS};

/// What the fake desktop logs where a real one opens the settings window.
const WINDOW: &str = "fake desktop: the settings window opens";

/// `-v` and `--version` print the version as the log says it, and nothing starts.
#[test]
fn the_version_from_every_way_in() {
    let expected = format!(
        "Prostir {}\n",
        prostir_proto::app_version(env!("CARGO_PKG_VERSION"))
    );
    for args in [
        &["--version"][..],
        &["-v"],
        &["serve", "--version"],
        &["ctl", "--version"],
        &["window", "-v"],
        &["window", "--version"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_prostir"))
            .args(args)
            .env_remove("PROSTIR_FAKE_DESKTOP")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{args:?}");
    }
}

/// While Prostir runs, starting it again by any way in opens its settings window and
/// ends with 0, never "already running"; so does a click on the tray icon.
#[test]
fn starting_again_opens_the_settings_window() {
    let dir = TestDir::new("start-again");
    dir.save_monitors(TWO_MONITORS);
    let mut pc = Pc::start(&dir, &Options::default());
    if pc.bus.is_none() {
        return;
    }
    assert_eq!(
        pc.count_log(WINDOW),
        0,
        "not at a start that is not the first"
    );
    for args in [&[][..], &["serve"], &["serve", "--stay"]] {
        let before = pc.count_log(WINDOW);
        let again = pc.again(args);
        assert_eq!(again.code, 0, "prostir {args:?}: {again:?}");
        assert!(!again.err.contains("already running"), "{again:?}");
        pc.wait_log_after(SOON, WINDOW, before);
    }
    let before = pc.count_log(WINDOW);
    pc.tray("activate");
    pc.wait_log_after(SOON, WINDOW, before);
    pc.wait_log(SOON, "fake tray: activate");
}

/// Asleep in its sockets' watcher, `prostir` (the menu) wakes it for the window.
#[test]
fn starting_again_wakes_the_watcher_for_the_window() {
    let dir = TestDir::new("start-again-asleep");
    dir.save_monitors(TWO_MONITORS);
    let options = Options {
        watcher: true,
        ..Options::default()
    };
    let mut pc = Pc::start(&dir, &options);
    if pc.bus.is_none() {
        return;
    }
    let again = pc.again(&[]);
    assert_eq!(again.code, 0, "{again:?}");
    pc.wait_log(SOON, WINDOW);
}

/// The very first start opens the settings window; the next does not.
#[test]
fn the_first_start_opens_the_settings_window() {
    let dir = TestDir::new("first-start");
    let mut pc = Pc::start(&dir, &Options::default());
    pc.wait_log(SOON, "the first start: the settings window opens");
    pc.wait_log(SOON, WINDOW);
    pc.quit();
    let pc = Pc::start(&dir, &Options::default());
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(pc.count_log(WINDOW), 0);
}
