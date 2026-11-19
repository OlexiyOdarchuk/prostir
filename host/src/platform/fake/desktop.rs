//! The fake desktop and its monitors: outputs with a size and a place, whose picture
//! (see `screen.rs`) the encoder reads like a KWin output.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::info;

use super::screen::{self, Picture, Screen};
use crate::encoder::{Encoder, Input};
use crate::platform::{DesktopBackend, Geometry, MonitorSpec, OutputBackend};

/// Name prefix of the fake outputs.
const PREFIX: &str = "Fake-";

/// Where each output is on the fake desktop, shared by the desktop and its outputs.
type Layout = Arc<Mutex<BTreeMap<String, Geometry>>>;

pub struct FakeDesktop {
    layout: Layout,
    /// Picture changes per second on every output (0: still).
    fps: u32,
    picture: Picture,
    /// How long making a monitor takes (KScreen can take seconds).
    slow: Duration,
}

impl DesktopBackend for FakeDesktop {
    fn connect() -> Result<Self> {
        let fps = match std::env::var("PROSTIR_FAKE_FPS") {
            Ok(v) => v.parse().context("PROSTIR_FAKE_FPS: a whole number")?,
            Err(_) => 0,
        };
        let picture = match std::env::var("PROSTIR_FAKE_PICTURE").as_deref() {
            Ok("noise") => Picture::Noise,
            Ok("bands") | Err(_) => Picture::Bands,
            Ok(other) => anyhow::bail!("PROSTIR_FAKE_PICTURE: bands or noise, not {other:?}"),
        };
        let slow = match std::env::var("PROSTIR_FAKE_SLOW_MONITORS_MS") {
            Ok(v) => v
                .parse()
                .context("PROSTIR_FAKE_SLOW_MONITORS_MS: a whole number")?,
            Err(_) => 0,
        };
        info!("desktop: fake, the picture ({picture:?}) changes {fps} times a second");
        Ok(Self {
            layout: Layout::default(),
            fps,
            picture,
            slow: Duration::from_millis(slow),
        })
    }

    fn has_primary(&self) -> bool {
        false
    }

    fn create(&self, number: u32, spec: &MonitorSpec) -> Result<FakeOutput> {
        std::thread::sleep(self.slow);
        let name = format!("{PREFIX}{number}");
        let output = FakeOutput::start(
            name.clone(),
            spec,
            self.fps,
            self.picture,
            self.layout.clone(),
        )?;
        info!("fake desktop: created {name} ({spec})");
        Ok(output)
    }

    fn physical_output(&self) -> Option<String> {
        None
    }

    fn wake_physical_display(&mut self) {}

    fn mirror(&self, _number: u32, name: &str) -> Result<FakeOutput> {
        anyhow::bail!("the fake desktop has no display {name} to mirror")
    }

    fn geometries(&self) -> HashMap<String, Geometry> {
        self.layout
            .lock()
            .unwrap()
            .iter()
            .map(|(name, g)| (name.clone(), *g))
            .collect()
    }

    fn place(
        &self,
        positions: &[(String, i32, i32)],
        _primary: Option<&str>,
        _current: &HashMap<String, Geometry>,
    ) -> Result<()> {
        let mut layout = self.layout.lock().unwrap();
        for (name, x, y) in positions {
            if let Some(g) = layout.get_mut(name) {
                (g.x, g.y) = (*x, *y);
            }
        }
        let placed: Vec<String> = positions
            .iter()
            .map(|(name, x, y)| format!("{name} {x},{y}"))
            .collect();
        info!("fake desktop: placed {}", placed.join(", "));
        Ok(())
    }

    fn keep_primary(&mut self, _primary: &FakeOutput) {}

    fn is_virtual(&self, name: &str) -> bool {
        name.starts_with(PREFIX)
    }

    fn screenshots_one_output(&self) -> bool {
        true
    }

    fn sync_wallpapers(&self) {}
}

/// A fake monitor. Its picture changes `fps` times a second (never with 0) until it
/// is dropped.
pub struct FakeOutput {
    name: String,
    screen: Arc<Screen>,
    stop: Arc<AtomicBool>,
    changer: Option<JoinHandle<()>>,
    layout: Layout,
}

impl FakeOutput {
    fn start(
        name: String,
        spec: &MonitorSpec,
        fps: u32,
        picture: Picture,
        layout: Layout,
    ) -> Result<Self> {
        let screen = Screen::add(&name, spec.width, spec.height, picture)?;
        let stop = Arc::new(AtomicBool::new(false));
        let changer = if fps > 0 {
            let (screen, stop) = (screen.clone(), stop.clone());
            let every = Duration::from_secs(1) / fps;
            Some(
                std::thread::Builder::new()
                    .name(format!("fake-{name}"))
                    .spawn(move || {
                        let mut next = Instant::now();
                        while !stop.load(Ordering::Relaxed) {
                            next += every;
                            std::thread::sleep(next.saturating_duration_since(Instant::now()));
                            screen.change();
                        }
                    })?,
            )
        } else {
            None
        };
        let logical = |px: u32| (f64::from(px) / spec.scale).round() as i32;
        layout.lock().unwrap().insert(
            name.clone(),
            Geometry {
                x: 0,
                y: 0,
                w: logical(spec.width),
                h: logical(spec.height),
                priority: None,
                enabled: true,
            },
        );
        Ok(Self {
            name,
            screen,
            stop,
            changer,
            layout,
        })
    }
}

impl Drop for FakeOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.changer.take() {
            let _ = t.join();
        }
        self.screen.remove();
        self.layout.lock().unwrap().remove(&self.name);
        info!("fake desktop: removed {}", self.name);
    }
}

impl OutputBackend for FakeOutput {
    fn output_name(&self) -> &str {
        &self.name
    }

    fn closed(&self) -> bool {
        false
    }

    fn make_primary(&self) {}

    fn input(&self) -> Input {
        Input {
            source: format!("{} screen={}", screen::ELEMENT, self.name),
            label: self.name.clone(),
        }
    }

    fn detach(&mut self) {}

    /// The encoder reads the picture itself.
    fn attach(&mut self, _encoder: &Encoder, _dmabuf: bool) -> Result<()> {
        Ok(())
    }
}
