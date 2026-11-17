//! Renders the generated panorama to a PNG: cargo run -p qmon-client --example sky_preview -- sky.png
fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "sky.png".into());
    let px = questmon::background::night_sky();
    let size =
        tiny_skia::IntSize::from_wh(questmon::background::WIDTH, questmon::background::HEIGHT)
            .unwrap();
    tiny_skia::Pixmap::from_vec(px, size)
        .unwrap()
        .save_png(&out)
        .unwrap();
    println!("{out}");
}
