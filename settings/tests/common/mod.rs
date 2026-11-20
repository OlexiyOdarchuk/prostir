//! What the window tests share: a window on the mock service, and clicking its
//! controls the way a person would.

#![allow(dead_code)]

use eframe::egui::{self, Role};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use prostir_settings::app::{CloseNote, Page, Started};
use prostir_settings::mock::{Mock, MockHandle};
use prostir_settings::portal::SystemScheme;
use prostir_settings::{App, Options};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The window's smallest size: everything has to fit in it.
pub const SIZE: egui::Vec2 = egui::vec2(960.0, 640.0);

/// A theme file path that does not exist, the same on every machine.
pub fn no_file() -> PathBuf {
    PathBuf::from("/nonexistent/.config/prostir/window-theme.toml")
}

pub struct Setup {
    pub language: &'static str,
    pub theme_file: PathBuf,
    /// How many times "Start Prostir" ran.
    pub starts: Arc<Mutex<u32>>,
    /// What starting it finds.
    pub started: Started,
    /// Drawn with wgpu, for pictures; without, the window runs all the same.
    pub pictures: bool,
    /// Seconds between frames.
    pub step: f32,
    /// The note the first time the window closes.
    pub close_note: Option<CloseNote>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            language: "uk",
            theme_file: no_file(),
            starts: Arc::default(),
            started: Started::Launched,
            pictures: false,
            step: 1.0 / 60.0,
            close_note: None,
        }
    }
}

pub fn window(mock: Mock, setup: Setup) -> (Harness<'static, App>, MockHandle) {
    let handle = mock.handle();
    let starts = setup.starts.clone();
    let started = setup.started;
    let mut builder = Harness::builder()
        .with_size(SIZE)
        .with_pixels_per_point(1.0)
        .with_render_cursor(false)
        .with_step_dt(setup.step);
    if setup.pictures {
        builder = builder.wgpu();
    }
    let mut h = builder.build_eframe(move |cc| {
        App::new(
            &cc.egui_ctx,
            Options {
                client: Box::new(mock),
                threaded: false,
                theme_file: Some(setup.theme_file),
                lang_dirs: Some(Vec::new()),
                language: Some(setup.language.into()),
                system: SystemScheme::default(),
                start: Box::new(move || {
                    *starts.lock().unwrap() += 1;
                    Ok(started)
                }),
                presented: None,
                close_note: setup.close_note,
            },
        )
    });
    h.run_steps(2);
    (h, handle)
}

pub fn open(h: &mut Harness<'static, App>, page: Page) {
    h.state_mut().ui.page = page;
    // Rows centre on the size their controls had the frame before.
    h.run_steps(4);
}

/// Clicks the control named `label`, scrolled into view first, as a person would.
pub fn press(h: &mut Harness<'static, App>, label: &str) {
    press_as(h, None, label);
}

/// The same for the control of `role` named `label`, where a text has the same name.
pub fn press_as(h: &mut Harness<'static, App>, role: Option<Role>, label: &str) {
    let find = |h: &Harness<'static, App>| match role {
        Some(r) => h.get_by_role_and_label(r, label).rect(),
        None => h.get_by_label(label).rect(),
    };
    match role {
        Some(r) => h.get_by_role_and_label(r, label).scroll_to_me(),
        None => h.get_by_label(label).scroll_to_me(),
    }
    h.run_steps(2);
    // egui finds what is under the pointer from where it was the frame before.
    let at = find(h).center();
    h.hover_at(at);
    h.run_steps(2);
    let at = find(h).center();
    h.event(egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    h.event(egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(3);
}
