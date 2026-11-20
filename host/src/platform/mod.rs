//! The door from the core to the operating system: the core (`main.rs`, `displays.rs`,
//! `layout.rs`, `encoder.rs`, `pairing.rs`, `outbox.rs`, `usage.rs`, `i18n.rs`) reaches
//! the desktop, the sound system and the user's files only through this module.
//!
//! This file holds what every platform shares: the types below and two traits. Each
//! operating system has a module of its own (`linux/`, picked with `cfg`) whose public
//! items are re-exported here, so the core writes `platform::windows::active()` and
//! never names the OS.
//!
//! # A new operating system (Windows)
//!
//! Add `platform/windows/mod.rs`, select it below next to `linux`, and provide what
//! `linux/mod.rs` exports, with the same names and signatures:
//!
//! - `Desktop` implementing [`DesktopBackend`] and `Output` implementing
//!   [`OutputBackend`]: creating and removing virtual monitors and the laptop-screen
//!   mirror, their geometry and placement, the primary output, and the frame source
//!   for the encoder ([`crate::encoder::Input`]: GStreamer source elements, or an
//!   `appsrc` named `src` that [`OutputBackend::attach`] feeds).
//! - `windows`: `active`, `cursor`, `all_windows`, `can_move_cursor`, `move_cursor`,
//!   `move_to`, `put_back`, `forget_panel`, `vacated`, `screenshot`, `screenshot_path`.
//! - `focus::ActiveOutput` (`new`, `get`): the output the user works on.
//! - `audio`: `AudioOut` (PC sound to the headset, as a default output device while it
//!   exists), `MicIn` (the headset microphone as the default input: `push`,
//!   `recording`), `restore_after_crash`.
//! - `sound_source::Watcher` (`start`, `output`): the output whose window plays sound.
//! - `notifications::watch`: the PC's desktop notifications, as text.
//! - `tray`: `spawn`, `Handle`, `set_status`, `set_battery`, `set_settings`,
//!   `set_pairing`; the tray menu also turns autostart on and off.
//! - `dialogs`: `ask_code` (the pairing code typed on the PC), `notify` and
//!   `notify_and_wait` (desktop notifications).
//! - System: `config_home`, `single_instance`, `quit_on_signals`, `quit_requested`,
//!   `clean_up_after_crash`, `give_memory_back`, `hostname`, `broadcast_addresses`,
//!   `run` (a program run to the end, stopped if it hangs). The private runtime dir
//!   stays inside the platform.
//!
//! The network belongs to the operating system, not to the desktop: `net.rs` holds
//! it for every desktop, the fake one too, with a part for each operating system.
//!
//! The compiler checks the list: the core does not build until everything is there.
//! `fake/` is the smallest complete example: a desktop that exists only in memory,
//! which the end-to-end tests build in place of the real platform.
//!
//! # A new Linux desktop (GNOME)
//!
//! Linux picks the desktop at run time (`linux/desktop.rs`: KDE Plasma or Hyprland).
//! Add a `linux/gnome/` module next to `linux/kde/` and `linux/hyprland/`, a variant
//! for it in `Desktop` and `Output` (making monitors, streaming them, placing them),
//! and a branch in each function of `linux/windows.rs` and in `linux/focus.rs` (the
//! active window and output, moving windows, the mouse, screenshots). Sound,
//! notifications, the tray, dialogs and the system part are the same on every Linux
//! desktop.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::encoder::{Encoder, Input};

#[cfg(all(target_os = "linux", not(feature = "fake-desktop")))]
mod linux;
#[cfg(all(target_os = "linux", not(feature = "fake-desktop")))]
pub use linux::*;

// A desktop inside the process, for the end-to-end tests only (see `fake/mod.rs`). It
// replaces the whole platform, so a test build never touches the real desktop.
#[cfg(feature = "fake-desktop")]
mod fake;
#[cfg(feature = "fake-desktop")]
pub use fake::*;

// The connection to the headset, the same with the fake desktop as with a real one.
mod net;
pub use net::*;

#[cfg(not(any(target_os = "linux", feature = "fake-desktop")))]
compile_error!("Prostir has no platform layer for this operating system yet (see platform/mod.rs)");

/// A virtual monitor to make: its mode and scale.
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

/// `2560x1440` or `3840x2160@1.5`, as on the command line and in saved files.
impl std::str::FromStr for MonitorSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (size, scale) = match s.split_once('@') {
            Some((a, b)) => (a, b.parse::<f64>().map_err(|e| format!("bad scale: {e}"))?),
            None => (s, 1.0),
        };
        let (w, h) = size.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
        let spec = MonitorSpec {
            width: w.parse().map_err(|e| format!("bad width: {e}"))?,
            height: h.parse().map_err(|e| format!("bad height: {e}"))?,
            scale,
        };
        spec.validate().map_err(|e| e.to_string())?;
        Ok(spec)
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
    #[cfg_attr(
        feature = "fake-desktop",
        expect(dead_code, reason = "the fake desktop has no windows")
    )]
    pub id: String,
    /// Logical size on the desktop.
    pub width: i32,
    pub height: i32,
    /// Scale of the output it is on.
    pub scale: f64,
    /// The output it is on.
    pub output: String,
}

/// What the tray asks the main loop to do.
#[cfg_attr(
    feature = "fake-desktop",
    expect(dead_code, reason = "the fake tray has only some of the items")
)]
pub enum TrayCommand {
    SetBitrate(u32),
    /// Clearer text at about 2 ms more encoding per frame.
    SetSharp(bool),
    /// Count this PC among users (see `usage`).
    SetCounting(bool),
    /// Drop all paired headsets: each must pair again.
    ForgetHeadsets,
    /// Turn the monitors in the headset to face the user.
    Recenter,
    Quit,
}

/// What the tray shows.
#[derive(Clone, Default, PartialEq)]
pub struct TrayStatus {
    /// "Wi-Fi" or "USB" while a headset is connected.
    pub connected: Option<&'static str>,
    pub monitors: usize,
    pub mirror: bool,
    /// Connected, but the headset is not worn: streaming paused.
    pub paused: bool,
}

/// Why there is no pairing code typed on the PC (see `dialogs::ask_code`).
pub enum NoCode {
    #[cfg_attr(
        feature = "fake-desktop",
        expect(dead_code, reason = "the fake pairing dialog is never cancelled")
    )]
    Cancelled,
    /// The PC has no program to ask with.
    NoDialog,
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
    /// An error when the desktop did not take it.
    fn place(
        &self,
        positions: &[(String, i32, i32)],
        primary: Option<&str>,
        current: &HashMap<String, Geometry>,
    ) -> Result<()>;

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

    /// The pixel size another program gave it, when that is no longer the size it was
    /// made with (on KDE, sharing it in Discord or OBS can do that). Starting its
    /// encoder over puts the size back. `None` where the desktop never does that.
    fn resized(&self) -> Option<(u32, u32)>;

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
