//! Renders the headset menu to a PNG for checking the design on the desktop.
//! cargo run -p prostir-client --example panel_preview -- panel.png [hover_x hover_y]
//! Environment: TAB=look|sound, LANG_UI=en, PAIR=show|type|failed, OFFLINE=1, KEYPAD=…

use prostir::i18n::Lang;
use prostir::settings::Settings;
use prostir::ui::{MonitorEntry, PairingView, Panel, Tab, UiState};

fn questmon_pairing(mode: &str) -> PairingView {
    PairingView {
        host: "workstation".into(),
        code: (mode == "show").then(|| "482 913".into()),
        typed: if mode == "show" {
            String::new()
        } else {
            "4829".into()
        },
        failed: mode == "failed",
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "panel.png".into());
    let hover = match (args.next(), args.next()) {
        (Some(x), Some(y)) => Some((x.parse().unwrap(), y.parse().unwrap())),
        _ => None,
    };
    let state = UiState {
        mic_denied: std::env::var("MIC_DENIED").is_ok(),
        confirm_save: None,
        unreachable: std::env::var("BLOCKED").ok(),
        pair_offer: std::env::var("OFFER")
            .ok()
            .map(|v| ("workstation 192.168.1.10".into(), v == "failed")),
        pairing: std::env::var("PAIR")
            .ok()
            .map(|mode| questmon_pairing(&mode)),
        host: std::env::var("OFFLINE")
            .is_err()
            .then(|| "workstation".into()),
        found: vec![
            ("workstation".into(), "192.168.1.10".into()),
            ("media-pc".into(), "192.168.1.20".into()),
        ],
        ip_input: std::env::var("KEYPAD").ok(),
        monitors: vec![
            MonitorEntry {
                id: 0,
                width: 2560,
                height: 1440,
            },
            MonitorEntry {
                id: 1,
                width: 2560,
                height: 1440,
            },
            MonitorEntry {
                id: 2,
                width: 1440,
                height: 2560,
            },
        ],
        primary: Some(1),
        mirror_wanted: true,
        audio_wanted: true,
        mic_wanted: true,
        mirror_active: true,
        settings: Settings::default(),
        room_available: true,
        panorama_available: true,
        custom_available: false,
        curve_available: true,
        keyboard_available: true,
        error: None,
        confirm_remove: std::env::var("CONFIRM").is_ok().then_some(2),
        intro: std::env::var("INTRO").is_ok(),
        update: std::env::var("UPDATE")
            .ok()
            .map(|v| (v == "pc", "0.2.0".into())),
        tab: match std::env::var("TAB").as_deref() {
            Ok("look") => Tab::Look,
            Ok("profiles") => Tab::Profiles,
            Ok("sound") => Tab::Sound,
            _ => Tab::Monitors,
        },
        profiles: (0..prostir::profiles::SLOTS)
            .map(prostir::profiles::Profile::default_for)
            .collect(),
        active_profile: Some(0),
        lang: if std::env::var("LANG_UI").as_deref() == Ok("en") {
            Lang::En
        } else {
            Lang::Uk
        },
    };
    let mut panel = Panel::new();
    panel.set_state(&state);
    panel.hover(hover);
    let rgba = panel.render_if_dirty().unwrap().to_vec();
    let pixmap = tiny_skia::Pixmap::from_vec(
        rgba,
        tiny_skia::IntSize::from_wh(prostir::ui::WIDTH, prostir::ui::HEIGHT).unwrap(),
    )
    .unwrap();
    // Composite over a dark grey so translucent parts are visible in the preview.
    let mut bg = tiny_skia::Pixmap::new(prostir::ui::WIDTH, prostir::ui::HEIGHT).unwrap();
    bg.fill(tiny_skia::Color::from_rgba8(40, 44, 52, 255));
    bg.draw_pixmap(
        0,
        0,
        pixmap.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );
    bg.save_png(&out).unwrap();
    println!("{out}");
}
