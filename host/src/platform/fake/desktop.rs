//! The fake desktop and its monitors: outputs with a size and a place, whose picture
//! (see `screen.rs`) the encoder reads like a KWin output.
//!
//! Other programs act on the desktop through `$XDG_RUNTIME_DIR/fake-desktop`, read and
//! removed a few times a second, one command a line:
//! - `resize Fake-2 320x240`: another program's stream of the output makes it that
//!   size, as sharing a monitor in Discord does on KDE; starting Prostir's own stream
//!   over puts its size back;
//! - `resize-sticky Fake-2 320x240`: the same, but only making the output anew does;
//! - `unplug eDP-1`, `plug eDP-1`: a display of the PC goes away (the lid closed, the
//!   cable out) and comes back.
//!
//! The PC's own displays are `PROSTIR_FAKE_PHYSICAL`: `CONNECTOR=EDID@WxH` separated
//! by `;` (an empty EDID: the display says nothing), all plugged in at the start.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{info, warn};

use super::screen::{self, Picture, Screen};
use crate::encoder::{Encoder, Input};
use crate::platform::{
    DesktopBackend, Geometry, MonitorSpec, OutputBackend, PhysicalDisplay, is_panel,
};

/// A display of the fake PC itself, and whether it is plugged in.
struct FakePhysical {
    display: PhysicalDisplay,
    plugged: bool,
}

/// The fake PC's own displays, shared with the thread that carries out the test's
/// commands.
static PHYSICAL: Mutex<Vec<FakePhysical>> = Mutex::new(Vec::new());

/// `CONNECTOR=EDID@WxH;…` (see the top of this file).
fn parse_physical(text: &str) -> Result<Vec<FakePhysical>> {
    text.split(';')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| {
            let (connector, rest) = t.split_once('=').context("CONNECTOR=EDID@WxH")?;
            let (edid, size) = rest.rsplit_once('@').context("CONNECTOR=EDID@WxH")?;
            let (w, h) = size.split_once('x').context("a size is WxH")?;
            Ok(FakePhysical {
                display: PhysicalDisplay {
                    connector: connector.to_owned(),
                    edid: (!edid.is_empty()).then(|| edid.to_owned()),
                    builtin: is_panel(connector),
                    size: Some((w.parse()?, h.parse()?)),
                },
                plugged: true,
            })
        })
        .collect()
}

/// Whether display `name` of the fake PC is plugged in.
fn plugged(name: &str) -> bool {
    PHYSICAL
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.plugged && p.display.connector == name)
}

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
    /// Monitors (by number, from 1) that cannot be made.
    broken: Vec<u32>,
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
        let broken = std::env::var("PROSTIR_FAKE_BROKEN_MONITORS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect();
        let physical = parse_physical(&std::env::var("PROSTIR_FAKE_PHYSICAL").unwrap_or_default())
            .context("PROSTIR_FAKE_PHYSICAL")?;
        info!(
            "desktop: fake, the picture ({picture:?}) changes {fps} times a second; {} display(s) of its own",
            physical.len()
        );
        *PHYSICAL.lock().unwrap() = physical;
        static WATCHED: Once = Once::new();
        WATCHED.call_once(watch_commands);
        Ok(Self {
            layout: Layout::default(),
            fps,
            picture,
            slow: Duration::from_millis(slow),
            broken,
        })
    }

    fn name(&self) -> &'static str {
        "fake"
    }

    fn has_primary(&self) -> bool {
        false
    }

    fn create(&self, number: u32, spec: &MonitorSpec) -> Result<FakeOutput> {
        std::thread::sleep(self.slow);
        anyhow::ensure!(
            !self.broken.contains(&number),
            "fake monitor {number} is broken"
        );
        let name = format!("{PREFIX}{number}");
        let output = FakeOutput::start(
            name.clone(),
            spec,
            self.fps,
            self.picture,
            self.layout.clone(),
            true,
        )?;
        info!("fake desktop: created {name} ({spec})");
        Ok(output)
    }

    /// An external display before the laptop's panel, as on Hyprland.
    fn physical_output(&self) -> Option<String> {
        let mut found = self.physical_displays();
        found.sort_by_key(|d| (d.builtin, d.connector.clone()));
        found.first().map(|d| d.connector.clone())
    }

    fn physical_displays(&self) -> Vec<PhysicalDisplay> {
        PHYSICAL
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.plugged)
            .map(|p| p.display.clone())
            .collect()
    }

    fn wake_physical_display(&mut self) {}

    fn mirror(&self, _number: u32, name: &str) -> Result<FakeOutput> {
        let size = PHYSICAL
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.plugged && p.display.connector == name)
            .and_then(|p| p.display.size)
            .with_context(|| format!("the fake desktop has no display {name}"))?;
        let spec = MonitorSpec {
            width: size.0,
            height: size.1,
            scale: 1.0,
        };
        let output = FakeOutput::start(
            name.to_owned(),
            &spec,
            self.fps,
            self.picture,
            self.layout.clone(),
            false,
        )?;
        info!("fake desktop: mirroring {name}");
        Ok(output)
    }

    fn geometries(&self) -> HashMap<String, Geometry> {
        let mut layout = self.layout.lock().unwrap();
        // The PC's own displays are on the desktop while plugged in.
        for p in PHYSICAL.lock().unwrap().iter() {
            let name = &p.display.connector;
            match (p.plugged, layout.contains_key(name)) {
                (true, false) => {
                    let (w, h) = p.display.size.unwrap_or((1920, 1080));
                    let g = Geometry {
                        x: 0,
                        y: 0,
                        w: w as i32,
                        h: h as i32,
                        priority: None,
                        enabled: true,
                    };
                    layout.insert(name.clone(), g);
                }
                (false, true) => {
                    layout.remove(name);
                }
                _ => {}
            }
        }
        layout
            .iter()
            .map(|(name, g)| {
                let mut g = *g;
                // Resized by another program: at the same scale, the size it has now.
                if let Some(screen) = Screen::find(name)
                    && let Some((w, h)) = screen.resized()
                {
                    let (made_w, made_h) = screen.size();
                    let scaled = |now: u32, made: u32, logical: i32| {
                        (f64::from(now) * f64::from(logical) / f64::from(made.max(1))).round()
                            as i32
                    };
                    (g.w, g.h) = (scaled(w, made_w, g.w), scaled(h, made_h, g.h));
                }
                (name.clone(), g)
            })
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
    /// Made here (and gone with it), not a display of the PC streamed as it is.
    owned: bool,
}

impl FakeOutput {
    fn start(
        name: String,
        spec: &MonitorSpec,
        fps: u32,
        picture: Picture,
        layout: Layout,
        owned: bool,
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
        if owned {
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
        }
        Ok(Self {
            name,
            screen,
            stop,
            changer,
            layout,
            owned,
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
        if self.owned {
            self.layout.lock().unwrap().remove(&self.name);
            info!("fake desktop: removed {}", self.name);
        } else {
            info!("fake desktop: stopped mirroring {}", self.name);
        }
    }
}

impl OutputBackend for FakeOutput {
    fn output_name(&self) -> &str {
        &self.name
    }

    /// A display of the PC is closed once it is unplugged.
    fn closed(&self) -> bool {
        !self.owned && !plugged(&self.name)
    }

    fn resized(&self) -> Option<(u32, u32)> {
        self.screen.resized()
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

/// Carries out what other programs do on the desktop, as the test writes it to
/// `$XDG_RUNTIME_DIR/fake-desktop` (see the top of this file).
fn watch_commands() {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let path = PathBuf::from(dir).join("fake-desktop");
    let watched = std::thread::Builder::new()
        .name("fake-desktop".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let _ = std::fs::remove_file(&path);
                for line in text.lines() {
                    if let Err(e) = command(line.trim()) {
                        warn!("fake desktop: {line:?}: {e:#}");
                    }
                }
            }
        });
    if let Err(e) = watched {
        warn!("fake desktop: no commands: {e}");
    }
}

/// One command from the file.
fn command(line: &str) -> Result<()> {
    let words: Vec<&str> = line.split_whitespace().collect();
    if let [verb @ ("plug" | "unplug"), name] = words[..] {
        let mut physical = PHYSICAL.lock().unwrap();
        let p = physical
            .iter_mut()
            .find(|p| p.display.connector == name)
            .with_context(|| format!("no display {name}"))?;
        p.plugged = verb == "plug";
        info!("fake desktop: {name} {verb}ged");
        return Ok(());
    }
    let [verb @ ("resize" | "resize-sticky"), name, size] = words[..] else {
        anyhow::bail!("not a command");
    };
    let (w, h) = size.split_once('x').context("a size is WxH")?;
    let size = (w.parse()?, h.parse()?);
    let screen = Screen::find(name).with_context(|| format!("no output {name}"))?;
    screen.resize(size, verb == "resize-sticky");
    info!("fake desktop: another program resized {name} to {w}x{h}");
    Ok(())
}
