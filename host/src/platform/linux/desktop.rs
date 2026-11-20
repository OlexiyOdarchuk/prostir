//! The desktop Prostir adds monitors to: KDE Plasma (KWin streams its outputs itself)
//! or Hyprland (headless outputs captured with wlr-screencopy).

use std::collections::HashMap;

use anyhow::Result;

use crate::encoder::{Encoder, Input};
use crate::platform::linux::hyprland;
use crate::platform::linux::hyprland::screencopy::{self, Capture};
use crate::platform::linux::kde::virtual_output::{Screencast, VirtualOutput};
use crate::platform::linux::kde::{Kde, kscreen, plasma};
use crate::platform::{DesktopBackend, Geometry, MonitorSpec, OutputBackend};

pub enum Desktop {
    Kde(Kde),
    Hyprland,
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // Real displays moved for the layout go back where they were.
        if let Self::Hyprland = self {
            hyprland::restore_real_outputs();
        }
    }
}

impl DesktopBackend for Desktop {
    fn connect() -> Result<Self> {
        if hyprland::running() {
            log::info!("desktop: Hyprland");
            hyprland::remove_leftovers();
            hyprland::keep_rules_after_reloads();
            Ok(Self::Hyprland)
        } else {
            Ok(Self::Kde(Kde::connect()?))
        }
    }

    fn has_primary(&self) -> bool {
        matches!(self, Self::Kde(_))
    }

    fn create(&self, number: u32, spec: &MonitorSpec) -> Result<Output> {
        let kind = match self {
            Self::Kde(k) => Kind::Kde(k.screencast.create(number, spec)?),
            Self::Hyprland => Kind::Hyprland(HyprOutput {
                name: hyprland::create(number, spec)?,
                capture: None,
                owned: true,
            }),
        };
        Ok(Output {
            kind,
            _connection: None,
        })
    }

    fn physical_output(&self) -> Option<String> {
        match self {
            Self::Kde(k) => k.screencast.physical_output(),
            Self::Hyprland => hyprland::physical_output(),
        }
    }

    fn wake_physical_display(&mut self) {
        if let Self::Kde(k) = self {
            k.wake_physical_display();
        }
    }

    fn mirror(&self, number: u32, name: &str) -> Result<Output> {
        match self {
            // KWin streams a given display only once per connection: each mirror
            // gets a connection of its own, kept as long as the output.
            Self::Kde(_) => {
                let connection = Screencast::connect()?;
                let output = connection.mirror(number, name)?;
                Ok(Output {
                    kind: Kind::Kde(output),
                    _connection: Some(connection),
                })
            }
            Self::Hyprland => Ok(Output {
                kind: Kind::Hyprland(HyprOutput {
                    name: name.to_owned(),
                    capture: None,
                    owned: false,
                }),
                _connection: None,
            }),
        }
    }

    fn geometries(&self) -> HashMap<String, Geometry> {
        match self {
            Self::Kde(_) => kscreen::geometries(),
            Self::Hyprland => hyprland::geometries(),
        }
    }

    fn place(
        &self,
        positions: &[(String, i32, i32)],
        primary: Option<&str>,
        current: &HashMap<String, Geometry>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Kde(k) => k.place(positions, primary, current),
            Self::Hyprland => {
                anyhow::ensure!(hyprland::place(positions), "Hyprland refused the layout");
                Ok(())
            }
        }
    }

    fn keep_primary(&mut self, primary: &Output) {
        if let (Self::Kde(k), Kind::Kde(o)) = (self, &primary.kind) {
            k.keep_primary(o);
        }
    }

    fn is_virtual(&self, name: &str) -> bool {
        name.starts_with("Virtual-") || name.starts_with(hyprland::PREFIX)
    }

    fn screenshots_one_output(&self) -> bool {
        // KDE's screenshot tool cannot take one output by name.
        !matches!(self, Self::Kde(_))
    }

    fn sync_wallpapers(&self) {
        if let Self::Kde(_) = self {
            plasma::sync_wallpapers();
        }
    }
}

/// A streamed output: one of ours, or a mirrored real display.
pub struct Output {
    // Field order matters: the stream closes before its own KWin connection.
    kind: Kind,
    /// A KDE mirror's own connection (see [`Desktop::mirror`]).
    _connection: Option<Screencast>,
}

enum Kind {
    Kde(VirtualOutput),
    Hyprland(HyprOutput),
}

pub struct HyprOutput {
    name: String,
    capture: Option<Capture>,
    /// Ours (removed when dropped), not a mirrored real display.
    owned: bool,
}

impl Drop for HyprOutput {
    fn drop(&mut self) {
        // Stop capturing before the output goes away.
        self.capture.take();
        if self.owned {
            hyprland::remove(&self.name);
        }
    }
}

impl OutputBackend for Output {
    fn output_name(&self) -> &str {
        match &self.kind {
            Kind::Kde(o) => o.output_name(),
            Kind::Hyprland(o) => &o.name,
        }
    }

    fn closed(&self) -> bool {
        match &self.kind {
            Kind::Kde(o) => o.closed(),
            Kind::Hyprland(o) => o.capture.as_ref().is_some_and(Capture::closed),
        }
    }

    fn resized(&self) -> Option<(u32, u32)> {
        match &self.kind {
            Kind::Kde(o) => o.resized(),
            // Its outputs change size only through monitor rules, not through capture.
            Kind::Hyprland(_) => None,
        }
    }

    fn make_primary(&self) {
        // Hyprland has no such thing.
        if let Kind::Kde(o) = &self.kind {
            o.make_primary();
        }
    }

    fn input(&self) -> Input {
        match &self.kind {
            Kind::Kde(o) => o.input(),
            Kind::Hyprland(o) => screencopy::input(&o.name),
        }
    }

    fn detach(&mut self) {
        if let Kind::Hyprland(o) = &mut self.kind {
            o.capture = None;
        }
    }

    fn attach(&mut self, encoder: &Encoder, dmabuf: bool) -> Result<()> {
        // KWin feeds the encoder through PipeWire by itself.
        if let Kind::Hyprland(o) = &mut self.kind {
            let appsrc = encoder
                .appsrc()
                .ok_or_else(|| anyhow::anyhow!("the encoder takes no pushed frames"))?;
            o.capture = Some(Capture::start(
                o.name.clone(),
                appsrc,
                encoder.paused(),
                dmabuf,
            )?);
        }
        Ok(())
    }
}
