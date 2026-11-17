//! Renders the headset menu to a PNG for checking the design on the desktop.
//! cargo run -p qmon-client --example panel_preview -- panel.png [hover_x hover_y]

use questmon::settings::Settings;
use questmon::ui::{MonitorEntry, Panel, UiState};

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "panel.png".into());
    let hover = match (args.next(), args.next()) {
        (Some(x), Some(y)) => Some((x.parse().unwrap(), y.parse().unwrap())),
        _ => None,
    };
    let state = UiState {
        host: Some("iShawyha-Laptop".into()),
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
        mirror_active: true,
        settings: Settings::default(),
        room_available: true,
        panorama_available: true,
        custom_available: false,
        curve_available: true,
        error: Some("Максимум 6 моніторів".into()),
        confirm_remove: Some(2),
    };
    let mut panel = Panel::new();
    panel.set_state(&state);
    panel.hover(hover);
    let rgba = panel.render_if_dirty().unwrap().to_vec();
    let pixmap = tiny_skia::Pixmap::from_vec(
        rgba,
        tiny_skia::IntSize::from_wh(questmon::ui::WIDTH, questmon::ui::HEIGHT).unwrap(),
    )
    .unwrap();
    // Composite over a dark grey so translucent parts are visible in the preview.
    let mut bg = tiny_skia::Pixmap::new(questmon::ui::WIDTH, questmon::ui::HEIGHT).unwrap();
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
