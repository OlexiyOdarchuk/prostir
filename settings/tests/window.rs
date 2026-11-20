//! The window drawn without a screen (kittest with wgpu): a picture of every page in
//! light and dark with both palettes, the states each page has, and the window driven
//! by name and by keyboard the way a screen reader or a keyboard user would.
//!
//! Pictures live in tests/snapshots; `UPDATE_SNAPSHOTS=1 cargo test -p prostir-settings`
//! writes them anew after a deliberate change. Before every picture kittest checks that
//! each control has an accessible name.

use eframe::egui::{self, Key, Role};
use egui_kittest::kittest::Queryable;
use egui_kittest::{Harness, SnapshotOptions, SnapshotResults};
use prostir_settings::api::{ClientError, Pairing, Request, ThemeMode};
use prostir_settings::app::{Conn, Page, Start};
use prostir_settings::mock::{Mock, MockHandle};
use prostir_settings::portal::SystemScheme;
use prostir_settings::{App, Options};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The window's smallest size: everything has to fit in it.
const SIZE: egui::Vec2 = egui::vec2(960.0, 640.0);

/// A theme file path that does not exist, the same on every machine.
fn no_file() -> PathBuf {
    PathBuf::from("/nonexistent/.config/prostir/window-theme.toml")
}

struct Setup {
    language: &'static str,
    theme_file: PathBuf,
    starts: Arc<Mutex<u32>>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            language: "uk",
            theme_file: no_file(),
            starts: Arc::default(),
        }
    }
}

fn window(mock: Mock, setup: Setup) -> (Harness<'static, App>, MockHandle) {
    let handle = mock.handle();
    let starts = setup.starts.clone();
    let mut h = Harness::builder()
        .with_size(SIZE)
        .with_pixels_per_point(1.0)
        .with_render_cursor(false)
        .wgpu()
        .build_eframe(move |cc| {
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
                        Ok(())
                    }),
                    presented: None,
                },
            )
        });
    h.run_steps(2);
    (h, handle)
}

fn open(h: &mut Harness<'static, App>, page: Page) {
    h.state_mut().ui.page = page;
    // Rows centre on the size their controls had the frame before.
    h.run_steps(4);
}

/// Clicks the control named `label`, scrolled into view first, as a person would.
fn press(h: &mut Harness<'static, App>, label: &str) {
    press_as(h, None, label);
}

/// The same for the control of `role` named `label`, where a text has the same name.
fn press_as(h: &mut Harness<'static, App>, role: Option<Role>, label: &str) {
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

/// Pictures of several windows in one test, checked together at its end.
type Pictures = SnapshotResults;

fn picture(h: &mut Harness<'static, App>, all: &mut Pictures, name: &str) {
    // Text is drawn slightly differently by different GPUs and lavapipe: a few
    // pixels may differ, a changed layout may not.
    h.snapshot_options(
        name,
        &SnapshotOptions::new().threshold(1.5).max_failed_pixels(400),
    );
    all.extend_harness(h);
}

const PAGES: [(Page, &str); 6] = [
    (Page::Overview, "overview"),
    (Page::Monitors, "monitors"),
    (Page::Headsets, "headsets"),
    (Page::Sound, "sound"),
    (Page::Look, "look"),
    (Page::FirstRun, "first-run"),
];

#[test]
fn every_page_in_both_themes_and_palettes() {
    let mut all = Pictures::default();
    for (page, name) in PAGES {
        for (theme, t) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
            for palette in ["ink", "graphite"] {
                let mock = if page == Page::FirstRun {
                    Mock::no_headsets()
                } else {
                    Mock::connected()
                };
                let (mut h, _) = window(mock.look(theme, palette), Setup::default());
                if page == Page::Monitors {
                    h.state_mut().ui.selected = Some(2);
                }
                open(&mut h, page);
                picture(&mut h, &mut all, &format!("{name}-{t}-{palette}"));
            }
        }
    }
    all.unwrap();
}

#[test]
fn each_state_of_the_overview() {
    let mut all = Pictures::default();
    let dark = |m: Mock| m.look(ThemeMode::Dark, "ink");
    for (mock, name) in [
        (dark(Mock::connecting()), "overview-connecting"),
        (dark(Mock::waiting()), "overview-waiting"),
        (dark(Mock::failed_monitor()), "overview-failed"),
        (dark(Mock::not_running()), "overview-not-running"),
        (
            dark(Mock::connected().edit(|s| {
                let session = s.headsets[0].session.as_mut().unwrap();
                session.worn = false;
                s.problems.push(prostir_settings::api::Problem {
                    code: "Desktop".into(),
                    text: "KWin не відповідає: нові монітори чекають на нього.".into(),
                });
            })),
            "overview-taken-off",
        ),
    ] {
        let (mut h, _) = window(mock, Setup::default());
        open(&mut h, Page::Overview);
        picture(&mut h, &mut all, name);
    }
    // No headset paired: the window opens on the first run.
    let (mut h, _) = window(dark(Mock::no_headsets()), Setup::default());
    h.run_steps(2);
    assert_eq!(h.state().ui.page, Page::FirstRun);
    open(&mut h, Page::Overview);
    picture(&mut h, &mut all, "overview-no-headsets");
    // In English.
    let (mut h, _) = window(
        Mock::connected().look(ThemeMode::Light, "ink"),
        Setup {
            language: "en",
            ..Setup::default()
        },
    );
    open(&mut h, Page::Overview);
    picture(&mut h, &mut all, "overview-en");
    all.unwrap();
}

#[test]
fn each_state_of_the_other_pages() {
    let mut all = Pictures::default();
    let dark = |m: Mock| m.look(ThemeMode::Dark, "ink");
    // Monitors around you, and with no headset (only the PC's screen).
    let (mut h, _) = window(dark(Mock::connected()), Setup::default());
    h.state_mut().ui.arc_view = true;
    h.state_mut().ui.selected = Some(2);
    open(&mut h, Page::Monitors);
    picture(&mut h, &mut all, "monitors-arc");
    let (mut h, _) = window(dark(Mock::waiting()), Setup::default());
    open(&mut h, Page::Monitors);
    picture(&mut h, &mut all, "monitors-offline");

    // Pairing: a headset shows a code; then a wrong code; a code from the PC; waiting.
    let (mut h, _) = window(dark(Mock::connected().pairing()), Setup::default());
    open(&mut h, Page::Headsets);
    picture(&mut h, &mut all, "headsets-pairing");
    h.get_by_label("Код із шолома").focus();
    h.run_steps(1);
    h.get_by_label("Код із шолома")
        .type_text(prostir_settings::mock::WRONG_CODE);
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(4);
    picture(&mut h, &mut all, "headsets-pairing-wrong");
    let pc_code = |code: Option<&'static str>| {
        move |s: &mut prostir_settings::api::State| {
            s.pairing = Some(Pairing {
                seconds_left: 95,
                seconds_total: 180,
                pc_code: code.map(Into::into),
                ..Default::default()
            });
        }
    };
    let (mut h, _) = window(
        dark(Mock::connected().edit(pc_code(Some("482913")))),
        Setup::default(),
    );
    open(&mut h, Page::Headsets);
    picture(&mut h, &mut all, "headsets-pc-code");
    let (mut h, _) = window(
        dark(Mock::connected().edit(pc_code(None))),
        Setup::default(),
    );
    open(&mut h, Page::Headsets);
    picture(&mut h, &mut all, "headsets-pairing-waiting");
    let (mut h, _) = window(dark(Mock::no_headsets()), Setup::default());
    open(&mut h, Page::Headsets);
    picture(&mut h, &mut all, "headsets-empty");

    // Sound with no headset.
    let (mut h, _) = window(dark(Mock::waiting()), Setup::default());
    open(&mut h, Page::Sound);
    picture(&mut h, &mut all, "sound-offline");

    // A theme file with mistakes: what was not used and what is hard to read.
    let dir = std::env::temp_dir().join(format!("prostir-settings-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("window-theme.toml");
    std::fs::write(
        &path,
        "format = 1\n[dark]\ntext = \"white\"\ntext_muted = \"#3A4560\"\nglow = \"#FFFFFF\"\n[shape]\nradius = 4\n",
    )
    .unwrap();
    let (mut h, _) = window(
        dark(Mock::connected()),
        Setup {
            theme_file: path.clone(),
            ..Setup::default()
        },
    );
    open(&mut h, Page::Look);
    if let Some(last) = h.get_all_by_label_contains("text_muted").last() {
        last.scroll_to_me();
    }
    h.run_steps(3);
    let folder = prostir_lang::short_path(&dir);
    let r = h.get_by_label(&folder).rect();
    h.mask(r);
    picture(&mut h, &mut all, "look-bad-theme");
    std::fs::remove_dir_all(&dir).unwrap();

    // The look chosen without the service holds for the window.
    let (mut h, _) = window(dark(Mock::not_running()), Setup::default());
    open(&mut h, Page::Look);
    picture(&mut h, &mut all, "look-not-running");
    all.unwrap();
}

#[test]
fn first_run_steps() {
    let mut all = Pictures::default();
    let (mut h, handle) = window(
        Mock::no_headsets().look(ThemeMode::Dark, "ink"),
        Setup::default(),
    );
    assert_eq!(h.state().ui.page, Page::FirstRun);
    press(&mut h, "Далі");
    // Going on to "open Prostir in the headset" opens pairing, so the headset finds
    // the PC.
    assert_eq!(handle.calls(), [Request::PairNewHeadset]);
    picture(&mut h, &mut all, "first-run-2");
    h.state_mut().ui.fr_step = 2;
    h.run_steps(3);
    picture(&mut h, &mut all, "first-run-3");
    h.state_mut().ui.fr_step = 3;
    h.run_steps(3);
    picture(&mut h, &mut all, "first-run-4");
    all.unwrap();
}

#[test]
fn pairing_by_name_and_keyboard() {
    let (mut h, handle) = window(Mock::connected().live(), Setup::default());
    open(&mut h, Page::Headsets);
    press(&mut h, "Сполучити новий шолом");
    assert_eq!(handle.calls(), [Request::PairNewHeadset]);
    assert!(h.query_by_label("Зупинити сполучення").is_some());
    let left = h.state().ui.pair.ends - h.ctx.input(|i| i.time);
    assert!((170.0..=180.0).contains(&left), "{left}");

    // The headset shows a code; it is typed here and sent with Enter.
    let (mut h, handle) = window(Mock::connected().pairing(), Setup::default());
    open(&mut h, Page::Headsets);
    h.get_by_label("Код із шолома").focus();
    h.run_steps(1);
    h.get_by_label("Код із шолома").type_text("48a2 913");
    h.run_steps(2);
    assert_eq!(h.state().ui.code, "482913");
    h.key_press(Key::Enter);
    h.run_steps(4);
    assert_eq!(
        handle.calls(),
        [Request::PairingCode {
            code: "482913".into()
        }]
    );
    // Pairing closed with a new headset: the window says who was paired.
    assert!(handle.state().pairing.is_none());
    h.get_by_label("Quest 3S (2) сполучено. Далі він підключатиметься сам.");
    assert_eq!(handle.state().headsets.len(), 3);
}

#[test]
fn forgetting_a_headset_asks_first() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Headsets);
    press(&mut h, "Забути Quest 3S");
    assert!(handle.calls().is_empty());
    h.get_by_label("Скасувати");
    press(&mut h, "Забути Quest 3S");
    assert_eq!(
        handle.calls(),
        [Request::ForgetHeadset {
            headset: "c81e07".into()
        }]
    );
}

#[test]
fn monitors_by_name_and_keyboard() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Monitors);
    press(&mut h, "1920 × 1080");
    assert!(matches!(
        handle.calls()[..],
        [Request::AddMonitor {
            w: 1920,
            h: 1080,
            ..
        }]
    ));
    // Select a monitor with the keyboard and move it with an arrow key.
    h.get_by_label("Prostir-3").focus();
    h.run_steps(1);
    h.key_press(Key::ArrowDown);
    h.run_steps(3);
    let calls = handle.calls();
    assert!(
        matches!(calls.last(), Some(Request::Place { id: 3, x: 5120, y }) if *y > 0),
        "{calls:?}"
    );
    assert_eq!(h.state().ui.selected, Some(3));
    // The selected monitor's controls.
    press(&mut h, "125 %");
    assert_eq!(
        handle.calls().last(),
        Some(&Request::SetScale { id: 3, scale: 125 })
    );
    press_as(&mut h, Some(Role::Switch), "Головний монітор");
    assert_eq!(handle.calls().last(), Some(&Request::SetPrimary { id: 3 }));
    press(&mut h, "Прибрати монітор");
    assert_eq!(
        handle.calls().last(),
        Some(&Request::RemoveMonitor { id: 3 })
    );
    assert!(h.state().ui.selected.is_none());
}

#[test]
fn the_pages_are_reached_with_the_keyboard() {
    let (mut h, _) = window(Mock::connected(), Setup::default());
    // Tab from nothing lands on the first page in the sidebar, then the next.
    h.key_press(Key::Tab);
    h.run_steps(1);
    assert!(h.get_by_role_and_label(Role::Tab, "Огляд").is_focused());
    h.key_press(Key::Tab);
    h.run_steps(1);
    assert!(h.get_by_role_and_label(Role::Tab, "Монітори").is_focused());
    h.key_press(Key::Enter);
    h.run_steps(2);
    assert_eq!(h.state().ui.page, Page::Monitors);
    h.key_press(Key::Tab);
    h.key_press(Key::Tab);
    h.run_steps(1);
    assert!(h.get_by_role_and_label(Role::Tab, "Звук").is_focused());
    h.key_press(Key::Space);
    h.run_steps(2);
    assert_eq!(h.state().ui.page, Page::Sound);
}

#[test]
fn the_look_is_chosen_and_kept_by_the_service() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Look);
    press(&mut h, "Світла");
    press(&mut h, "Нейтральна");
    press(&mut h, "English");
    let window = handle.state().window;
    assert_eq!(window.theme, ThemeMode::Light);
    assert_eq!(window.palette, "graphite");
    assert_eq!(window.language.as_deref(), Some("en"));
    // The window follows: the texts are English now.
    h.get_by_label("Theme");
}

#[test]
fn starting_prostir_from_the_window() {
    let setup = Setup::default();
    let starts = setup.starts.clone();
    let (mut h, handle) = window(Mock::not_running(), setup);
    assert!(matches!(h.state().conn, Conn::Absent { .. }));
    press(&mut h, "Запустити Prostir");
    assert_eq!(*starts.lock().unwrap(), 1);
    assert!(matches!(
        h.state().conn,
        Conn::Absent {
            start: Start::Waiting { .. },
            ..
        }
    ));
    h.get_by_label("Запускаю Prostir");
    handle.set_running(true);
    h.run_steps(2);
    assert!(matches!(h.state().conn, Conn::Live { .. }));
    // And it going away again.
    handle.set_running(false);
    h.run_steps(2);
    // In the sidebar and on the page.
    assert_eq!(h.get_all_by_label("Prostir не запущено").count(), 2);
}

#[test]
fn a_failed_request_says_why() {
    let mut all = Pictures::default();
    let (mut h, handle) = window(
        Mock::connected().look(ThemeMode::Dark, "ink"),
        Setup::default(),
    );
    open(&mut h, Page::Monitors);
    handle.refuse_next(ClientError::Refused {
        code: "Desktop".into(),
        message: "KWin не відповів.".into(),
    });
    press(&mut h, "2560 × 1440");
    h.get_by_label("Не вдалося: KWin не відповів.");
    picture(&mut h, &mut all, "monitors-refused");
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.query_by_label("Не вдалося: KWin не відповів.").is_none());
    all.unwrap();
}
