//! The window driven by name and by keyboard, the way a screen reader or a keyboard
//! user would, on the mock service. Nothing is drawn, so no graphics card is needed.

mod common;

use common::{Setup, open, press, press_as, window};
use eframe::egui::{self, Key, Role};
use egui_kittest::kittest::Queryable;
use prostir_settings::api::{ClientError, Request, ThemeMode};
use prostir_settings::app::{Conn, Page, Start};
use prostir_settings::mock::Mock;

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
fn starting_gives_up_after_ten_seconds() {
    let (mut h, _) = window(
        Mock::not_running(),
        Setup {
            step: 0.5,
            ..Setup::default()
        },
    );
    press(&mut h, "Запустити Prostir");
    h.run_steps(18);
    assert!(matches!(
        h.state().conn,
        Conn::Absent {
            start: Start::Waiting { .. },
            ..
        }
    ));
    h.run_steps(4);
    assert!(matches!(
        h.state().conn,
        Conn::Absent {
            start: Start::TimedOut,
            ..
        }
    ));
    h.get_by_label("Prostir не запустився");
}

#[test]
fn a_prostir_that_runs_already_is_not_started_again() {
    let setup = Setup {
        started: prostir_settings::app::Started::AlreadyRunning,
        ..Setup::default()
    };
    let (mut h, handle) = window(Mock::not_running(), setup);
    press(&mut h, "Запустити Prostir");
    h.get_by_label("Prostir уже працює");
    assert!(h.query_by_label("Запустити Prostir").is_none());
    // Once it answers, "Try again" finds it.
    handle.set_running_quietly(true);
    press(&mut h, "Спробувати ще раз");
    assert!(matches!(h.state().conn, Conn::Live { .. }));
}

#[test]
fn the_code_can_be_pasted() {
    let (mut h, handle) = window(Mock::connected().pairing(), Setup::default());
    open(&mut h, Page::Headsets);
    h.get_by_label("Код із шолома").focus();
    h.run_steps(1);
    h.event(egui::Event::Paste("482 913\n".into()));
    h.run_steps(2);
    assert_eq!(h.state().ui.code, "482913");
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert_eq!(
        handle.calls(),
        [Request::PairingCode {
            code: "482913".into()
        }]
    );
}

#[test]
fn a_theme_font_that_is_not_a_font_does_not_stop_the_window() {
    let dir = std::env::temp_dir().join(format!("prostir-settings-font-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let text = dir.join("words.ttf");
    std::fs::write(&text, "not a font at all").unwrap();
    let path = dir.join("window-theme.toml");
    std::fs::write(
        &path,
        format!(
            "[type]\nfont = \"{}\"\nfont_medium = \"/dev/zero\"\n",
            text.display()
        ),
    )
    .unwrap();
    let (mut h, _) = window(
        Mock::connected(),
        Setup {
            theme_file: path.clone(),
            ..Setup::default()
        },
    );
    open(&mut h, Page::Look);
    assert_eq!(h.get_all_by_label_contains("шрифт не читається").count(), 2);
    // The same when the file changes while the window is open.
    std::fs::write(&path, "[type]\nfont = \"/dev/zero\"\n").unwrap();
    h.state_mut().ui.page = Page::Overview;
    h.run_steps(90);
    open(&mut h, Page::Look);
    assert_eq!(h.get_all_by_label_contains("шрифт не читається").count(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn going_on_in_the_first_run_opens_pairing() {
    let (mut h, handle) = window(Mock::no_headsets(), Setup::default());
    assert_eq!(h.state().ui.page, Page::FirstRun);
    press(&mut h, "Далі");
    assert_eq!(handle.calls(), [Request::PairNewHeadset]);
}

#[test]
fn a_refused_request_says_why_until_closed() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Monitors);
    handle.refuse_next(ClientError::Refused {
        code: "Desktop".into(),
        message: "KWin не відповів.".into(),
    });
    press(&mut h, "2560 × 1440");
    h.get_by_label("Не вдалося: KWin не відповів.");
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.query_by_label("Не вдалося: KWin не відповів.").is_none());
}
