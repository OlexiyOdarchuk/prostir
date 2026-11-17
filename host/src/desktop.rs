//! The desktop Prostir adds monitors to: KDE Plasma (KWin streams its outputs itself)
//! or Hyprland (headless outputs captured with wlr-screencopy).

use std::collections::HashMap;

use anyhow::Result;

use crate::encoder::{Encoder, Input};
use crate::hyprland;
use crate::screencopy::Capture;
use crate::virtual_output::{self, Geometry, MonitorSpec, Screencast, VirtualOutput};

pub enum Desktop {
    Kde(Screencast),
    Hyprland,
}

impl Desktop {
    pub fn connect() -> Result<Self> {
        if hyprland::running() {
            log::info!("desktop: Hyprland");
            hyprland::remove_leftovers();
            hyprland::keep_rules_after_reloads();
            Ok(Self::Hyprland)
        } else {
            Ok(Self::Kde(Screencast::connect()?))
        }
    }

    pub fn is_kde(&self) -> bool {
        matches!(self, Self::Kde(_))
    }

    /// A new virtual monitor, number `number` (1-based).
    pub fn create(&self, number: u32, spec: &MonitorSpec) -> Result<Output> {
        match self {
            Self::Kde(s) => Ok(Output::Kde(s.create(number, spec)?)),
            Self::Hyprland => Ok(Output::Hyprland(HyprOutput {
                name: hyprland::create(number, spec)?,
                capture: None,
                owned: true,
            })),
        }
    }

    /// The real display to mirror (the laptop panel), if one is on.
    pub fn physical_output(&self) -> Option<String> {
        match self {
            Self::Kde(s) => s.physical_output(),
            Self::Hyprland => hyprland::physical_output(),
        }
    }

    /// Streams an existing display. KWin needs a connection of its own for that,
    /// returned alongside to be kept as long as the output.
    pub fn mirror(&self, number: u32, name: &str) -> Result<(Output, Option<Screencast>)> {
        match self {
            Self::Kde(_) => {
                let connection = Screencast::connect()?;
                let output = connection.mirror(number, name)?;
                Ok((Output::Kde(output), Some(connection)))
            }
            Self::Hyprland => Ok((
                Output::Hyprland(HyprOutput {
                    name: name.to_owned(),
                    capture: None,
                    owned: false,
                }),
                None,
            )),
        }
    }

    /// Logical geometry of every output.
    pub fn geometries(&self) -> HashMap<String, Geometry> {
        match self {
            Self::Kde(_) => virtual_output::output_geometries(),
            Self::Hyprland => hyprland::geometries(),
        }
    }

    /// Moves outputs to `positions` (logical, top-left); on KDE also makes `primary`
    /// the primary output. Only what differs from `current` is sent.
    pub fn place(
        &self,
        positions: &[(String, i32, i32)],
        primary: Option<&str>,
        current: &HashMap<String, Geometry>,
    ) {
        match self {
            Self::Kde(_) => {
                let mut settings: Vec<String> = positions
                    .iter()
                    .filter(|(name, x, y)| current.get(name).is_none_or(|g| (g.x, g.y) != (*x, *y)))
                    .map(|(name, x, y)| format!("output.{name}.position.{x},{y}"))
                    .collect();
                if let Some(p) = primary
                    && current.get(p).is_none_or(|g| g.priority != Some(1))
                {
                    settings.push(format!("output.{p}.priority.1"));
                }
                virtual_output::kscreen_apply(&settings);
            }
            Self::Hyprland => hyprland::place(positions),
        }
    }
}

/// A streamed output: one of ours, or a mirrored real display.
pub enum Output {
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

impl Output {
    /// The compositor's name for it, e.g. `Virtual-Prostir-1`, `Prostir-1` or `eDP-1`.
    pub fn output_name(&self) -> &str {
        match self {
            Self::Kde(o) => o.output_name(),
            Self::Hyprland(o) => &o.name,
        }
    }

    pub fn closed(&self) -> bool {
        match self {
            Self::Kde(o) => o.closed(),
            Self::Hyprland(o) => o.capture.as_ref().is_some_and(Capture::closed),
        }
    }

    /// KDE's primary output (Hyprland has no such thing).
    pub fn make_primary(&self) {
        if let Self::Kde(o) = self {
            o.make_primary();
        }
    }

    /// What to build the encoder on.
    pub fn input(&self) -> Input {
        match self {
            Self::Kde(o) => Input::PipeWire(o.node),
            Self::Hyprland(o) => Input::Pushed(o.name.clone()),
        }
    }

    /// Starts feeding `encoder` (Hyprland; KWin feeds it through PipeWire by itself).
    pub fn attach(&mut self, encoder: &Encoder, software: bool) -> Result<()> {
        if let Self::Hyprland(o) = self {
            let appsrc = encoder
                .appsrc()
                .ok_or_else(|| anyhow::anyhow!("the encoder takes no pushed frames"))?;
            o.capture = Some(Capture::start(
                o.name.clone(),
                appsrc,
                encoder.paused(),
                !software,
            )?);
        }
        Ok(())
    }
}
