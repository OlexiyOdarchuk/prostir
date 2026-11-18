//! Linux: KDE Plasma or Hyprland, picked at run time.

pub mod audio;
pub mod desktop;
pub mod focus;
pub mod hyprland;
pub mod kde;
pub mod notifications;
pub mod sound_source;
pub mod system;
pub mod tray;
pub mod windows;

pub use desktop::{Desktop, Output};
