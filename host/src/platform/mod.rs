//! The door from the core to the operating system.

use anyhow::{Result, bail};

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
