//! The door from the core to the operating system.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::encoder::{Encoder, Input};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorSpec {
    /// Size in pixels of the encoded stream.
    pub width: u32,
    pub height: u32,
    /// KWin output scale (UI scaling on that monitor); 1.0 keeps what KScreen remembers.
    pub scale: f64,
}

impl MonitorSpec {
    pub fn validate(&self) -> Result<()> {
        if !(320..=7680).contains(&self.width)
            || !(240..=4320).contains(&self.height)
            || !(0.5..=4.0).contains(&self.scale)
        {
            bail!(
                "monitor {}x{}@{} is out of range",
                self.width,
                self.height,
                self.scale
            );
        }
        Ok(())
    }
}

impl std::fmt::Display for MonitorSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)?;
        if (self.scale - 1.0).abs() > f64::EPSILON {
            write!(f, "@{}", self.scale)?;
        }
        Ok(())
    }
}

/// The window that has the keyboard, as found when asked.
#[derive(Debug, Clone)]
pub struct ActiveWindow {
    /// The desktop's id for it (Hyprland address, KWin internal id).
    pub id: String,
    /// Logical size on the desktop.
    pub width: i32,
    pub height: i32,
    /// Scale of the output it is on.
    pub scale: f64,
    /// The output it is on.
    pub output: String,
}

/// Where an output is on the desktop: logical position and size, and its priority
/// (1 = primary; only KDE has priorities).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub priority: Option<u32>,
    /// `false` for an output the desktop lists as disabled (a closed laptop lid).
    pub enabled: bool,
}

/// The desktop the monitors are added to, picked when a session starts. Implemented
/// by the platform's `Desktop`; its outputs are the platform's `Output`.
pub trait DesktopBackend: Sized {
    /// Connects to the desktop (and clears what a crashed run left on it).
    fn connect() -> Result<Self>;

    /// Whether the desktop has a primary output (where the panel and new windows go).
    fn has_primary(&self) -> bool;

    /// A new virtual monitor, number `number` (1-based).
    fn create(&self, number: u32, spec: &MonitorSpec) -> Result<Output>;

    /// The real display to mirror (the laptop panel), if one is on.
    fn physical_output(&self) -> Option<String>;

    /// No real display to mirror although one should be there (a laptop panel left
    /// off after the lid was opened): switch it on, if the desktop needs that.
    fn wake_physical_display(&mut self);

    /// Streams an existing display, number `number`, named `name`.
    fn mirror(&self, number: u32, name: &str) -> Result<Output>;

    /// Logical geometry of every output.
    fn geometries(&self) -> HashMap<String, Geometry>;

    /// Moves outputs to `positions` (logical, top-left); also makes `primary` the
    /// primary output where there is one. Only what differs from `current` is sent.
    fn place(
        &self,
        positions: &[(String, i32, i32)],
        primary: Option<&str>,
        current: &HashMap<String, Geometry>,
    );

    /// Checks now and then that `primary` is still the primary output, and makes it
    /// so again if the desktop moved it elsewhere.
    fn keep_primary(&mut self, primary: &Output);

    /// Whether output `name` is virtual (ours or another program's), not a real display.
    fn is_virtual(&self, name: &str) -> bool;

    /// Whether [`windows::screenshot`] can take a single output; if not, a monitor's
    /// screenshot is decoded from its own stream.
    fn screenshots_one_output(&self) -> bool;

    /// Gives new monitors the wallpaper of the others, where the desktop needs that.
    fn sync_wallpapers(&self);
}

/// A streamed output: a virtual monitor, a window panel, or the mirrored real display.
/// Dropping it removes it from the desktop (a mirrored display stays).
pub trait OutputBackend {
    /// The desktop's name for it, e.g. `Virtual-Prostir-1`, `Prostir-1` or `eDP-1`.
    fn output_name(&self) -> &str;

    /// Whether the desktop closed it.
    fn closed(&self) -> bool;

    /// Makes it the primary output (where the desktop has one).
    fn make_primary(&self);

    /// What to build the encoder on.
    fn input(&self) -> Input;

    /// Stops feeding the current encoder (before it is replaced).
    fn detach(&mut self);

    /// Starts feeding `encoder`, for inputs whose frames the platform pushes itself.
    /// `dmabuf`: the encoder takes GPU buffers.
    fn attach(&mut self, encoder: &Encoder, dmabuf: bool) -> Result<()>;
}
