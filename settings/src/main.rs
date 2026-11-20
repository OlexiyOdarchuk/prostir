//! `prostir-settings`: the settings window of the Prostir PC part.
//!
//! `PROSTIR_SETTINGS_MOCK=1` (or `--mock`) shows a made-up PC with a headset instead of
//! talking to the Prostir running in the background, to try the window without one.

use std::time::Instant;

use eframe::egui;
use prostir_settings::api::Client;
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
    let started = Instant::now();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let mock = std::env::args().any(|a| a == "--mock")
        || std::env::var_os("PROSTIR_SETTINGS_MOCK").is_some_and(|v| v == "1");

    let bus = dbus::session();
    let mut presented = None;
    if let Ok(conn) = &bus {
        match instance::claim(conn) {
            Ok(Start::Second) => {
                log::info!("the settings window is open already: brought it forward");
                return Ok(());
            }
            Ok(Start::First(p)) => presented = Some(p),
            Err(e) => log::warn!("could not claim {}: {e}", instance::NAME),
        }
    }
    let system = SystemScheme::default();
    let (client, start): (Box<dyn Client>, prostir_settings::app::Starter) = if mock {
        let m = Mock::connected().live();
        let handle = m.handle();
        (
            Box::new(m),
            Box::new(move || {
                handle.set_running(true);
                Ok(())
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
