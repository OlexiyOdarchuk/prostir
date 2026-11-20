//! `prostir-settings`: the settings window of the Prostir PC part.
//!
//! `PROSTIR_SETTINGS_MOCK=1` (or `--mock`) shows a made-up PC with a headset instead of
//! talking to the Prostir running in the background, to try the window without one.

use std::time::Instant;

use eframe::egui;
use prostir_settings::api::Client;
use prostir_settings::app::CloseNote;
use prostir_settings::app::start_prostir;
use prostir_settings::instance::{self, Start};
use prostir_settings::mock::Mock;
use prostir_settings::portal::SystemScheme;
use prostir_settings::{App, NoBus, Options, dbus, theme};

/// The window with the time it took to show, for the log.
struct Timed {
    app: App,
    started: Instant,
    shown: bool,
}

impl eframe::App for Timed {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.show(ui);
        if !self.shown {
            self.shown = true;
            log::info!("first frame {:?} after start", self.started.elapsed());
        }
    }
}

fn main() -> eframe::Result {
    if prostir_settings::asks_version(std::env::args().skip(1)) {
        println!("{}", prostir_settings::version_line());
        return Ok(());
    }
    let started = Instant::now();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let mock = std::env::args().any(|a| a == "--mock")
        || std::env::var_os("PROSTIR_SETTINGS_MOCK").is_some_and(|v| v == "1");

    let bus = dbus::session();
    let mut presented = None;
    if let Ok(conn) = &bus {
        let name = if mock {
            instance::MOCK_NAME
        } else {
            instance::NAME
        };
        // A first window that has its name but not yet its object answers Present
        // with an error: ask again a few times before opening without the name.
        for attempt in 1..=5 {
            match instance::claim(conn, name) {
                Ok(Start::Second) => {
                    log::info!("the settings window is open already: brought it forward");
                    return Ok(());
                }
                Ok(Start::First(p)) => {
                    presented = Some(p);
                    break;
                }
                Err(e) if attempt == 5 => log::warn!("could not claim {name}: {e}"),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
    }
    let system = SystemScheme::default();
    // The note the first time the window closes while Prostir runs on, as a desktop
    // notification; remembered next to the window's theme file.
    let close_note = match (&bus, mock) {
        (Ok(conn), false) => {
            let conn = conn.clone();
            let shown = theme::user_path().with_file_name("window-closed-note");
            Some(CloseNote {
                shown,
                notify: Box::new(move |summary, body| {
                    dbus::notify(&conn, summary, body)
                        .inspect_err(|e| log::warn!("no desktop notification: {e}"))
                        .is_ok()
                }),
            })
        }
        _ => None,
    };
    let (client, start): (Box<dyn Client>, prostir_settings::app::Starter) = if mock {
        let m = Mock::connected().live();
        let handle = m.handle();
        (
            Box::new(m),
            Box::new(move || {
                handle.set_running(true);
                Ok(prostir_settings::launch::Started::Launched)
            }),
        )
    } else {
        let client: Box<dyn Client> = match &bus {
            Ok(conn) => match dbus::DbusClient::new(conn.clone()) {
                Ok(c) => Box::new(c),
                Err(e) => Box::new(NoBus(e.to_string())),
            },
            Err(e) => Box::new(NoBus(e.to_string())),
        };
        (client, Box::new(start_prostir))
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 800.0])
            .with_min_inner_size([960.0, 640.0])
            .with_title("Prostir")
            // The same name as the .desktop file, so the desktop shows its icon.
            .with_app_id(instance::NAME),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Prostir",
        options,
        Box::new(move |cc| {
            if let Ok(conn) = bus {
                let ctx = cc.egui_ctx.clone();
                system.follow(conn, move || ctx.request_repaint());
            }
            let app = App::new(
                &cc.egui_ctx,
                Options {
                    client,
                    threaded: true,
                    theme_file: Some(theme::user_path()),
                    lang_dirs: None,
                    language: None,
                    system,
                    start,
                    presented,
                    close_note,
                },
            );
            Ok(Box::new(Timed {
                app,
                started,
                shown: false,
            }))
        }),
    )
}
