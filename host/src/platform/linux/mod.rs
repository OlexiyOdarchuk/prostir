//! Linux: KDE Plasma or Hyprland, picked at run time.

pub mod audio;
mod autostart;
mod desktop;
pub mod dialogs;
pub mod focus;
mod hyprland;
mod kde;
pub mod notifications;
pub mod sound_source;
mod system;
pub mod tray;
pub mod windows;

pub use desktop::{Desktop, Output};
pub use system::{
    broadcast_addresses, clean_up_after_crash, config_home, give_memory_back, hostname, run,
    single_instance,
};
