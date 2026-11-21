//! Renders the built-in panoramas to PNGs:
//! cargo run --release -p prostir-client --example sky_preview -- DIR

#![expect(
    clippy::disallowed_methods,
    reason = "a desktop tool, not the core: it times how long each panorama takes"
)]

use prostir::background::{self, Sky};

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    for (sky, name) in [
        (Sky::Night, "night"),
        (Sky::Sunset, "sunset"),
        (Sky::Space, "space"),
        (Sky::Aurora, "aurora"),
    ] {
        let t = std::time::Instant::now();
        let px = background::generate(sky);
        let size = tiny_skia::IntSize::from_wh(background::WIDTH, background::HEIGHT).unwrap();
        let path = dir.join(format!("{name}.png"));
        tiny_skia::Pixmap::from_vec(px, size)
            .unwrap()
            .save_png(&path)
            .unwrap();
        println!("{} in {:?}", path.display(), t.elapsed());
    }
}
