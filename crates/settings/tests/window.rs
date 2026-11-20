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
    // As step 6.8 brings it: pairing opened from the window.
    let mock = Mock::connected()
        .pairing()
        .edit(|s| s.pairing = None)
        .live();
    let (mut h, handle) = window(mock, Setup::default());
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
    press(&mut h, "125\u{a0}%");
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
    h.get_by_label("Запуск Prostir");
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
    h.get_by_label("Ця версія Prostir старіша за вікно");
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
        code: prostir_settings::api::ApiError::Desktop,
        message: "KWin не відповів.".into(),
    });
    press(&mut h, "2560 × 1440");
    h.get_by_label("Не вдалося: KWin не відповів.");
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.query_by_label("Не вдалося: KWin не відповів.").is_none());
}

#[test]
fn a_service_that_does_not_answer_is_asked_again() {
    let mock = Mock::connected();
    mock.handle().set_silent(true);
    let (mut h, handle) = window(mock, Setup::default());
    assert_eq!(h.get_all_by_label("Prostir не відповідає").count(), 2);
    assert!(h.query_by_label("Запустити Prostir").is_none());
    handle.set_silent(false);
    press(&mut h, "Спробувати ще раз");
    assert!(matches!(h.state().conn, Conn::Live { .. }));
}

#[test]
fn the_first_run_says_when_pairing_is_closed() {
    let (mut h, handle) = window(Mock::no_headsets(), Setup::default());
    h.state_mut().ui.fr_step = 1;
    h.run_steps(3);
    h.get_by_label("Сполучення закрито, тож новий шолом не знайде цей ПК.");
    assert!(h.query_by_label("Пошук шолома в мережі").is_none());
    press(&mut h, "Відкрити знову");
    assert_eq!(handle.calls(), [Request::PairNewHeadset]);
}

#[test]
fn a_new_theme_file_pins_nothing() {
    let dir = std::env::temp_dir().join(format!("prostir-settings-new-{}", std::process::id()));
    let path = dir.join("window-theme.toml");
    let (mut h, _) = window(
        Mock::connected(),
        Setup {
            theme_file: path.clone(),
            ..Setup::default()
        },
    );
    open(&mut h, Page::Look);
    press(&mut h, "Створити файл");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("format = 1"));
    h.run_steps(90);
    // The file is in use, clean, and the palette still changes the colours.
    h.get_by_label("Застосовано. Зміни у файлі видно одразу, перезапуск не потрібен.");
    press(&mut h, "Нейтральна");
    h.run_steps(30);
    let graphite =
        prostir_settings::theme::Theme::builtin(prostir_settings::theme::PaletteName::Graphite);
    assert_eq!(h.state().theme().dark, graphite.dark);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tab_meets_back_before_next_as_seen() {
    let (mut h, _) = window(Mock::no_headsets(), Setup::default());
    h.state_mut().ui.fr_step = 1;
    h.run_steps(3);
    let back = h.get_by_label("Назад").rect();
    let next = h.get_by_label("Далі").rect();
    assert!(back.right() < next.left(), "Back is left of Next");
    h.get_by_label("Назад").focus();
    h.run_steps(1);
    h.key_press(Key::Tab);
    h.run_steps(1);
    assert!(h.get_by_label("Далі").is_focused());
}

#[test]
fn pairing_this_prostir_does_not_open_says_what_to_do_instead() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Headsets);
    press(&mut h, "Сполучити новий шолом");
    assert_eq!(handle.calls(), [Request::PairNewHeadset]);
    // No failure: the button gives way to what to do.
    assert!(h.query_by_label_contains("Не вдалося").is_none());
    assert!(h.query_by_label("Сполучити новий шолом").is_none());
    h.get_by_label_contains("ПК покаже тут код");
}

#[test]
fn a_code_shown_on_the_pc_has_no_countdown_and_never_runs_out() {
    let (mut h, handle) = window(Mock::connected().pc_code("482913"), Setup::default());
    open(&mut h, Page::Headsets);
    h.get_by_label("Введіть цей код у шоломі:");
    h.get_by_label("482 913");
    assert!(h.query_by_label_contains("ще ").is_none(), "no countdown");
    // The code goes away (paired with another headset or closed): no "time is over".
    handle.change(|s| s.pairing = None);
    h.run_steps(3);
    assert!(h.query_by_label_contains("Час сполучення минув").is_none());
}

#[test]
fn monitors_kept_for_the_next_headset() {
    let (mut h, handle) = window(Mock::waiting(), Setup::default());
    open(&mut h, Page::Monitors);
    h.get_by_label("Чекають на шолом");
    // Added without a headset: kept too.
    press(&mut h, "1920 × 1080");
    let added = handle
        .state()
        .displays
        .into_iter()
        .find(|d| d.name == "Prostir-4")
        .unwrap();
    assert_eq!(added.state, prostir_settings::api::DisplayState::Kept);
    press(&mut h, "Прибрати Prostir-1");
    assert_eq!(
        handle.calls().last(),
        Some(&Request::RemoveMonitor { id: 1 })
    );
    open(&mut h, Page::Overview);
    assert!(h.get_all_by_label("Чекає на шолом").count() >= 3);
}

#[test]
fn the_microphone_is_shown_not_switched_and_the_volume_waits() {
    let (mut h, _) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Sound);
    assert!(
        h.query_by_role_and_label(Role::Switch, "Мікрофон шолома на ПК")
            .is_none()
    );
    h.get_by_label("Вмикається й вимикається в шоломі.");
    h.get_by_label("Змінюється кнопками шолома.");
}

#[test]
fn an_older_prostir_is_told_apart_from_none() {
    let mock = Mock::connected();
    mock.handle().set_old(true);
    let (mut h, handle) = window(mock, Setup::default());
    h.get_by_label("Ця версія Prostir старіша за вікно");
    assert!(h.query_by_label("Запустити Prostir").is_none());
    h.get_by_label("Prostir старіший за це вікно");
    handle.set_old(false);
    press(&mut h, "Спробувати ще раз");
    assert!(matches!(h.state().conn, Conn::Live { .. }));
}

#[test]
fn quality_by_name() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Quality);
    press(&mut h, "50");
    press(&mut h, "H.264");
    press_as(&mut h, Some(Role::Switch), "Чіткіший текст");
    press_as(&mut h, Some(Role::Switch), "Кодувати процесором");
    assert_eq!(
        handle.calls(),
        [
            Request::SetQuality { mbit: 50 },
            Request::SetCodec {
                codec: prostir_settings::api::CodecChoice::H264,
                cpu: false
            },
            Request::SetSharp { on: true },
            Request::SetCodec {
                codec: prostir_settings::api::CodecChoice::H264,
                cpu: true
            },
        ]
    );
    let e = handle.state().encoding;
    assert_eq!((e.bitrate, e.sharp, e.cpu), (50, true, true));
}

#[test]
fn what_the_service_does_not_do_is_disabled_quietly() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Quality);
    handle.refuse_next(ClientError::Refused {
        code: prostir_settings::api::ApiError::Unsupported,
        message: "Not on this PC.".into(),
    });
    press_as(&mut h, Some(Role::Switch), "Чіткіший текст");
    assert!(h.query_by_label_contains("Не вдалося").is_none());
    assert!(h.state().ui.unsupported.contains("set_sharp"));
    // Disabled now: a press does nothing.
    press_as(&mut h, Some(Role::Switch), "Чіткіший текст");
    assert_eq!(handle.calls().len(), 1);
}

#[test]
fn a_move_holds_until_the_state_with_it_is_in() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Monitors);
    h.get_by_label("Prostir-3").focus();
    h.run_steps(1);
    h.key_press(Key::ArrowDown);
    h.run_steps(3);
    assert!(matches!(handle.calls().last(), Some(Request::Place { .. })));
    // The answer came with the state that has the move: nothing is held any more.
    assert_eq!(h.state().ui.pending_place, None);
}

/// Three monitors streaming and a monitor of the PC hidden from the headset, with the
/// laptop's panel not connected: the map has all four on the desktop, the hidden one
/// with its size, and no display says a size of 0 × 0.
#[test]
fn the_map_has_every_display_on_the_desktop() {
    use prostir_settings::api::{DeskRect, Display, DisplayKind, DisplayState, Mode};
    let monitor = |id: u32, name: &str, x: i32| Display {
        id,
        kind: DisplayKind::Virtual,
        name: name.into(),
        output: format!("Virtual-{name}"),
        shown: true,
        mode: Mode {
            w: 2560,
            h: 1440,
            scale: 100,
        },
        desk: Some(DeskRect {
            x,
            y: 560,
            w: 2560,
            h: 1440,
        }),
        state: DisplayState::Running,
        ..Display::default()
    };
    let mock = Mock::connected().edit(|s| {
        s.displays = vec![
            monitor(1, "Prostir-4", 0),
            monitor(2, "Prostir-2", 2560),
            monitor(3, "Prostir-1", 5120),
            Display {
                id: 117,
                kind: DisplayKind::Physical,
                name: "HDMI-A-1".into(),
                output: "HDMI-A-1".into(),
                shown: false,
                mode: Mode {
                    w: 1920,
                    h: 1080,
                    scale: 100,
                },
                desk: Some(DeskRect {
                    x: 2560,
                    y: 2000,
                    w: 1920,
                    h: 1080,
                }),
                state: DisplayState::Running,
                ..Display::default()
            },
            Display {
                id: 100,
                kind: DisplayKind::Physical,
                name: "eDP-1".into(),
                output: "eDP-1".into(),
                builtin: true,
                shown: true,
                mode: Mode {
                    w: 0,
                    h: 0,
                    scale: 100,
                },
                state: DisplayState::Absent,
                ..Display::default()
            },
        ];
        s.primary = Some(2);
    });
    let (mut h, _handle) = window(mock, Setup::default());
    open(&mut h, Page::Monitors);
    for name in ["Prostir-4", "Prostir-2", "Prostir-1", "HDMI-A-1"] {
        h.get_by_role_and_label(Role::Button, name);
    }
    assert!(h.query_by_label_contains("0 × 0").is_none());
    h.get_by_role_and_label(Role::Button, "HDMI-A-1").focus();
    h.run_steps(2);
    h.get_by_label_contains("1920 × 1080");
    open(&mut h, Page::Overview);
    assert!(h.query_by_label_contains("0 × 0").is_none());
    assert!(h.query_by_label("Монітор не додано").is_none());
}

/// A monitor that could not be made: the overview names it once, its reason a
/// sentence of its own before the rest.
#[test]
fn a_failed_monitor_is_named_once() {
    let mock = Mock::failed_monitor().edit(|s| {
        let d = s
            .displays
            .iter_mut()
            .find(|d| d.name == "Prostir-4")
            .unwrap();
        d.failure.as_mut().unwrap().text = "2560x1440 більше, ніж шолом декодує в H265".into();
    });
    let (mut h, _handle) = window(mock, Setup::default());
    open(&mut h, Page::Overview);
    h.get_by_label(
        "Prostir-4: 2560x1440 більше, ніж шолом декодує в H265. Решта моніторів працює.",
    );
}

/// "На ПК" keeps the PC's sound on the PC, "У шоломі" sends it to the headset.
#[test]
fn the_sound_goes_where_the_button_says() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    open(&mut h, Page::Sound);
    assert!(handle.state().sound.to_headset);
    press_as(&mut h, Some(Role::RadioButton), "На ПК");
    assert_eq!(
        handle.calls().last(),
        Some(&Request::SoundToHeadset { on: false })
    );
    h.run_steps(2);
    press_as(&mut h, Some(Role::RadioButton), "У шоломі");
    assert_eq!(
        handle.calls().last(),
        Some(&Request::SoundToHeadset { on: true })
    );
    assert_eq!(handle.calls().len(), 2);
}

/// "Вийти з Prostir" quits the whole program as the tray's "Quit" does; it is there
/// only while Prostir runs.
#[test]
fn the_window_quits_prostir() {
    let (mut h, handle) = window(Mock::connected(), Setup::default());
    press(&mut h, "Вийти з Prostir");
    assert_eq!(handle.calls(), [Request::Quit]);
    let (h, _) = window(Mock::not_running(), Setup::default());
    assert!(h.query_by_label("Вийти з Prostir").is_none());
}

/// The first time the window closes while Prostir runs, a note says it goes on in the
/// tray and how to open the window again; never again after that.
#[test]
fn the_first_close_says_prostir_goes_on() {
    use prostir_settings::app::CloseNote;
    use std::sync::{Arc, Mutex};
    let dir = std::env::temp_dir().join(format!("prostir-close-note-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let shown = dir.join("prostir/window-closed-note");
    let said: Arc<Mutex<Vec<String>>> = Arc::default();
    let note = |said: Arc<Mutex<Vec<String>>>| CloseNote {
        shown: shown.clone(),
        notify: Box::new(move |summary, body| {
            said.lock().unwrap().push(format!("{summary}: {body}"));
            true
        }),
    };
    let setup = || Setup {
        close_note: Some(note(said.clone())),
        ..Setup::default()
    };
    let (mut h, _) = window(Mock::not_running(), setup());
    h.state_mut().window_closes();
    assert!(said.lock().unwrap().is_empty(), "nothing goes on");
    let (mut h, _) = window(Mock::connected(), setup());
    h.state_mut().window_closes();
    assert_eq!(
        *said.lock().unwrap(),
        [
            "Prostir працює далі: Prostir працює далі у треї. Відкрити знову: значок Prostir у меню або в треї."
        ]
    );
    assert!(shown.exists());
    h.state_mut().window_closes();
    let (mut h, _) = window(Mock::connected(), setup());
    h.state_mut().window_closes();
    assert_eq!(said.lock().unwrap().len(), 1, "once only");
    let _ = std::fs::remove_dir_all(&dir);
}
