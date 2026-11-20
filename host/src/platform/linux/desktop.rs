//! The desktop Prostir adds monitors to, picked once when a session starts: KDE Plasma
//! (KWin streams its outputs itself, see `kde/`) or Hyprland (headless outputs
//! captured with wlr-screencopy, see `hyprland/`).

use anyhow::Result;
use log::info;

use super::hyprland::{self, Hyprland};
use super::kde::Kde;
use crate::platform::{Desktop, GpuDevices};

/// Connects to the desktop this session runs in (and clears what a crashed run left
/// on it); its outputs share the graphics devices of `gpu`.
pub fn connect(gpu: &GpuDevices) -> Result<Box<dyn Desktop>> {
    if hyprland::running() {
        info!("desktop: Hyprland");
        Ok(Box::new(Hyprland::connect(gpu.clone())))
    } else {
        Ok(Box::new(Kde::connect(gpu.clone())?))
    }
}
