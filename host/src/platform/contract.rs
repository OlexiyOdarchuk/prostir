//! The platform contract: what the core asks of a desktop, of the outputs it streams,
//! of its windows and of its mouse. A desktop is picked once, when a session starts
//! (`platform::connect`), and the core holds it as a `Box<dyn Desktop>`: it never asks
//! which desktop it is, and never sees a desktop's own names for its outputs.
//!
//! What differs between desktops and operating systems is said as data where one
//! answer fits all (the capabilities in [`DesktopCaps`], the kind of memory frames live
//! in, the state of an output), not as more traits.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::encoder::{Encoder, Input};
use crate::errors::UserError;
use crate::screen::ScreenId;

/// A desktop: KDE Plasma, Hyprland or the fake one of the tests.
pub trait Desktop: Send {
    /// What it can do, found when it was connected.
    fn caps(&self) -> &DesktopCaps;

    /// Every output on the desktop now, the PC's own displays and the virtual ones
    /// (ours and other programs'), by key. Empty when the desktop does not answer.
    fn outputs(&self) -> Outputs;

    /// The displays of the PC itself connected and on now (the panel of a closed laptop
    /// is not); [`PhysicalDisplay::default`] marks the one to show in the headset when
    /// nothing else was chosen.
    fn physical(&self) -> Vec<PhysicalDisplay>;

    /// A new virtual monitor. Dropping the output removes it from the desktop.
    fn make(&mut self, spec: &VirtualSpec) -> Result<Box<dyn Output>>;

    /// Streams display `key` of the PC as it is. Dropping the output stops the stream;
    /// the display stays.
    fn show(&mut self, key: &OutputKey) -> Result<Box<dyn Output>>;

    /// Moves outputs where `plan` says and makes its primary the primary output, where
    /// the desktop has one. Only what differs from `current` (as [`Desktop::outputs`]
    /// said a moment ago) is sent. An error when the desktop did not take it.
    fn place(&mut self, plan: &DeskPlan, current: &Outputs) -> Result<()>;

    /// Checks now and then that `primary` is still the primary output, and makes it so
    /// again if the desktop moved it elsewhere.
    fn keep_primary(&mut self, _primary: &OutputKey) {}

    /// No display of the PC is on although one should be (a laptop's panel left off
    /// after its lid was opened): switch it on, where the desktop needs that.
    fn wake_physical(&mut self) {}

    /// Gives new monitors the wallpaper of the others, where the desktop needs that.
    fn sync_wallpapers(&mut self) {}

    /// Its windows; none where the desktop has no window panels.
    fn windows(&mut self) -> Option<&mut dyn Windows>;

    /// Its mouse; none where the mouse is neither found nor moved.
    fn pointer(&mut self) -> Option<&mut dyn Pointer>;
}

/// An output streamed to the headset: a virtual monitor, a window panel, or a display of
/// the PC shown as it is.
pub trait Output: Send {
    /// Its stable key (not a name the desktop may give to another output later).
    fn key(&self) -> &OutputKey;

    /// How it is doing now.
    fn health(&self) -> OutputHealth;

    /// Where its frames come from, to build the encoder on.
    fn source(&self) -> FrameSource;

    /// Starts feeding `encoder`, for sources whose frames the platform pushes itself.
    fn attach(&mut self, encoder: &Encoder) -> Result<()>;

    /// Stops feeding the current encoder (before it is replaced).
    fn detach(&mut self);

    /// Makes it the primary output, where the desktop has one.
    fn make_primary(&self);
}

/// The PC's windows: the active one, moving it to an output (and making it fill one, for
/// a window panel), noticing when it is gone, and pictures of the screen.
pub trait Windows {
    /// The window that has the keyboard.
    fn active(&mut self) -> Option<ActiveWindow>;

    /// The output the user works on (the one with the mouse, or with the active window,
    /// as the desktop counts it).
    fn active_output(&mut self) -> Option<OutputKey>;

    /// Moves window `w` to `output`; `fill`: full screen there (a window panel).
    fn move_to(&mut self, w: &ActiveWindow, output: &OutputKey, fill: bool) -> Result<()>;

    /// Puts window `w` back on `output` as an ordinary window (out of full screen): a
    /// window panel being closed.
    fn put_back(&mut self, w: &ActiveWindow, output: &OutputKey) -> Result<()>;

    /// Stops watching the window of panel `output`.
    fn forget_panel(&mut self, _output: &OutputKey) {}

    /// Panel outputs (of `panels`) whose window closed or left since the last call.
    fn vacated(&mut self, panels: &[OutputKey]) -> Vec<OutputKey>;

    /// Saves a picture of `output` (or of the whole desktop, where it cannot take one
    /// output; see [`DesktopCaps::screenshot_one_output`]) to the Pictures folder.
    fn screenshot(&mut self, output: Option<&OutputKey>) -> Result<PathBuf>;

    /// Every window, for a thread of its own (the one that looks where sound comes
    /// from). The desktop keeps it: such a thread holds it weakly, and stops once the
    /// desktop is gone.
    fn list(&self) -> Arc<dyn WindowList>;
}

/// Every normal window of the desktop, asked from any thread.
pub trait WindowList: Send + Sync {
    fn all(&self) -> Vec<AppWindow>;
}

/// A window, as [`WindowList::all`] lists it.
#[derive(Debug, Clone)]
pub struct AppWindow {
    /// The process that shows it.
    pub pid: u32,
    /// Its program, as the desktop calls it (Wayland app id or X11 class).
    pub class: String,
    /// The output it is on.
    pub output: OutputKey,
}

/// The mouse.
pub trait Pointer {
    /// Where it is on the desktop, in the desktop's units ([`DesktopCaps::units`]).
    fn position(&mut self) -> Option<(i32, i32)>;

    /// Puts it at `x`,`y`; whether that worked (never, where
    /// [`DesktopCaps::moves_pointer`] is false).
    fn move_to(&mut self, x: i32, y: i32) -> bool;
}

/// The key of an output, stable while it exists: on Linux the connector name the
/// desktop lists (`Virtual-Prostir-1`, `Prostir-2`, `eDP-1`), which does not change.
/// A desktop whose names do change makes its own key and keeps it to itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputKey(String);

impl OutputKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for OutputKey {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OutputKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How an output is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputHealth {
    /// Streams as made.
    Live,
    /// Exists, but sends no frames for a while, and comes back by itself: nothing to
    /// start over and nothing to lay out again.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no Linux desktop pauses an output")
    )]
    Paused { why: String },
    /// Another program gave it this pixel size, which is not the size it was made with
    /// (on KDE, sharing it in Discord or OBS does that). Starting its encoder over puts
    /// the size back.
    Resized { w: u32, h: u32 },
    /// The desktop closed it.
    Closed,
}

/// Where an output's frames come from.
#[derive(Debug, Clone)]
pub struct FrameSource {
    /// The start of the encoder's pipeline.
    pub input: Input,
    /// Who makes the frames.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the encoder is picked per output later")
    )]
    pub origin: Origin,
    /// Where the frames can be handed over without a copy: an encoder that takes this
    /// keeps them on the graphics card.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the encoder is picked per output later")
    )]
    pub memory: Memory,
    /// The graphics card they live on, where there can be more than one that matters.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the encoder is picked per output later")
    )]
    pub adapter: Option<AdapterId>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the encoder is picked per output later")
    )]
    pub cursor: Cursor,
}

/// Who makes an output's frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The virtual monitor's own maker hands them over: KWin's stream of a virtual
    /// output, the swap chain of our display driver on Windows.
    Monitor,
    /// They are copied off a screen: wlr-screencopy, KWin's stream of a real display,
    /// Desktop Duplication.
    Capture,
}

/// Where frames live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    /// Plain memory.
    #[cfg_attr(
        all(not(test), not(feature = "fake-desktop")),
        expect(
            dead_code,
            reason = "only the fake desktop's frames are in plain memory alone"
        )
    )]
    System,
    /// Linux GPU buffers; also plain memory for an encoder that takes no others.
    #[cfg_attr(
        all(not(test), feature = "fake-desktop"),
        expect(dead_code, reason = "the fake desktop's frames are in plain memory")
    )]
    DmaBuf,
    /// Direct3D 11 textures on one graphics card.
    #[cfg_attr(not(test), expect(dead_code, reason = "Windows only"))]
    D3D11 { adapter: AdapterId },
}

/// A graphics card, as the system tells them apart (on Windows its LUID, which
/// changes between boots, so it is never saved).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdapterId(pub u64);

/// Where the mouse pointer is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// Into the frames.
    InFrame,
    /// Apart from them (Desktop Duplication, a driver's hardware cursor).
    #[cfg_attr(not(test), expect(dead_code, reason = "every source draws it in"))]
    Separate,
}

/// What a desktop can do.
#[derive(Debug, Clone)]
pub struct DesktopCaps {
    /// For people and the headset: `KDE Plasma`, `Hyprland`.
    pub name: &'static str,
    /// Whether virtual monitors can be made, and how; or why not (the display driver is
    /// not installed). The PC's own displays work either way.
    pub virtual_monitors: Result<AddModel, Unavailable>,
    /// It has a primary output (where the panel and new windows go).
    pub has_primary: bool,
    /// What its positions and sizes are in.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "every Linux desktop is logical")
    )]
    pub units: Units,
    /// The primary output is always at (0, 0): the whole desktop moves with it.
    #[cfg_attr(not(test), expect(dead_code, reason = "Windows only"))]
    pub origin_is_primary: bool,
    /// The scales it gives an output, in percent.
    pub scales: Scales,
    /// Refresh rates it makes virtual monitors with, in Hz.
    pub refresh: Vec<u16>,
    /// [`Windows::screenshot`] can take a single output; if not, a monitor's picture is
    /// decoded from its own stream.
    pub screenshot_one_output: bool,
    /// [`Pointer::move_to`] works.
    pub moves_pointer: bool,
    /// Its virtual monitors go by themselves when Prostir is gone (KWin does that; a
    /// driver's watchdog would), so nothing is left to clean up after a crash.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the clean-up at start does not ask yet")
    )]
    pub cleans_up_after_crash: bool,
}

impl DesktopCaps {
    /// The refresh rate a new virtual monitor gets: 60 Hz where the desktop makes that,
    /// else the first it makes.
    pub fn refresh_for_new(&self) -> u16 {
        if self.refresh.contains(&60) {
            60
        } else {
            self.refresh.first().copied().unwrap_or(60)
        }
    }
}

/// How adding a virtual monitor goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddModel {
    /// One at a time: the others stay as they are.
    OneByOne,
    /// Adding or removing one makes every virtual monitor start over (a driver that
    /// restarts its adapter): all their streams break at once, which is no failure of
    /// each.
    #[cfg_attr(not(test), expect(dead_code, reason = "Windows only, as a fallback"))]
    RestartsAll,
}

/// Why something the desktop would do cannot be done here.
#[derive(Debug, Clone)]
pub struct Unavailable {
    /// What the headset is told.
    pub error: UserError,
    /// What the person at the PC is told.
    pub why: String,
}

/// What a desktop's positions and sizes are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Units {
    /// Logical pixels: an output's size is its pixels divided by its scale.
    Logical,
    /// Pixels.
    #[cfg_attr(not(test), expect(dead_code, reason = "Windows only"))]
    Physical,
}

/// The scales a desktop gives an output, in percent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scales {
    /// Any from `min` to `max`.
    Any { min: u16, max: u16 },
    /// Only these.
    Steps(Vec<u16>),
}

impl Scales {
    pub fn allows(&self, scale: u16) -> bool {
        match self {
            Self::Any { min, max } => (*min..=*max).contains(&scale),
            Self::Steps(steps) => steps.contains(&scale),
        }
    }
}

/// A virtual monitor to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualSpec {
    /// The screen it is for, the same in every run: a driver can make it the
    /// monitor's serial number, so the system remembers its place and scale.
    pub id: ScreenId,
    /// The lowest number free when it was made: its name on the desktop (`Prostir-N`),
    /// so the desktop finds what it remembers for that name.
    pub number: u32,
    /// Its picture in pixels.
    pub w: u32,
    pub h: u32,
    /// Its scale in percent (as `Mode.scale` in the protocol); 100 keeps what the
    /// desktop remembers.
    pub scale: u16,
    /// Hz, one of [`DesktopCaps::refresh`].
    pub refresh: u16,
}

impl VirtualSpec {
    /// The scale as a factor (1.25).
    pub fn factor(&self) -> f64 {
        f64::from(self.scale) / 100.0
    }
}

/// `2560x1440`, or `3840x2160@1.5` with a scale.
impl std::fmt::Display for VirtualSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.w, self.h)?;
        if self.scale != 100 {
            write!(f, "@{}", self.factor())?;
        }
        Ok(())
    }
}

/// Where outputs go on the desktop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeskPlan {
    /// Each output's top-left corner, in the desktop's units.
    pub positions: Vec<(OutputKey, i32, i32)>,
    /// The output to make primary, where the desktop has one.
    pub primary: Option<OutputKey>,
}

/// Every output on the desktop, by key.
pub type Outputs = HashMap<OutputKey, Geometry>;

/// Where an output is on the desktop: position and size in the desktop's units, and its
/// priority (1 = primary; only KDE has priorities).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub priority: Option<u32>,
    /// `false` for an output the desktop lists as disabled (a closed laptop lid).
    pub enabled: bool,
    /// Virtual (ours or another program's), not a display of the PC.
    pub is_virtual: bool,
}

/// A display of the PC itself, connected now.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalDisplay {
    /// Its key; on Linux its connector (`eDP-1`, `HDMI-A-1`), also shown to people.
    pub key: OutputKey,
    /// Maker, model and serial number as the display says them (its EDID), as far as
    /// the desktop tells; none when it says nothing.
    pub edid: Option<String>,
    /// The laptop's own panel.
    pub builtin: bool,
    /// Its picture in pixels, when known.
    pub size: Option<(u32, u32)>,
    /// The one to show in the headset when nothing else was chosen: an external
    /// monitor before the laptop's panel, never the panel of a closed laptop.
    pub default: bool,
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
    /// Size on the desktop, in its units.
    pub width: i32,
    pub height: i32,
    /// Scale of the output it is on.
    pub scale: f64,
    /// The output it is on, when the desktop says.
    pub output: Option<OutputKey>,
}

/// What a Windows desktop would report, built here so the contract keeps room for it:
/// these values must stay expressible (and every field readable) without a Windows
/// build.
#[cfg(test)]
mod windows_shape {
    use super::*;

    /// A Windows PC with our display driver: monitors one at a time, physical pixels with
    /// the primary at (0, 0), scales in steps, refresh rates the driver offers.
    fn with_driver() -> DesktopCaps {
        DesktopCaps {
            name: "Windows",
            virtual_monitors: Ok(AddModel::OneByOne),
            has_primary: true,
            units: Units::Physical,
            origin_is_primary: true,
            scales: Scales::Steps(vec![100, 125, 150, 175, 200]),
            refresh: vec![60, 72, 90, 120],
            screenshot_one_output: true,
            moves_pointer: true,
            cleans_up_after_crash: true,
        }
    }

    #[test]
    fn a_windows_desktop_fits_the_contract() {
        let caps = with_driver();
        assert_eq!(
            caps.virtual_monitors.as_ref().ok(),
            Some(&AddModel::OneByOne)
        );
        assert!(caps.has_primary && caps.origin_is_primary && caps.cleans_up_after_crash);
        assert_eq!(caps.units, Units::Physical);
        assert!(caps.scales.allows(125) && !caps.scales.allows(130));
        assert_eq!(caps.refresh_for_new(), 60);
        assert!(caps.screenshot_one_output && caps.moves_pointer);

        // No driver: the PC's own displays still work, adding a monitor says why not.
        let without = DesktopCaps {
            virtual_monitors: Err(Unavailable {
                error: UserError::Unsupported,
                why: "The Prostir display driver is not installed".into(),
            }),
            cleans_up_after_crash: false,
            ..with_driver()
        };
        let no = without.virtual_monitors.unwrap_err();
        assert_eq!(no.error, UserError::Unsupported);
        assert!(!no.why.is_empty());

        // A driver that restarts its adapter for every change.
        assert_ne!(AddModel::RestartsAll, AddModel::OneByOne);
    }

    #[test]
    fn windows_frames_fit_the_contract() {
        let gpu = AdapterId(0x0001_2345);
        // Our monitor's frames straight from the driver's swap chain, on its card.
        let driver = FrameSource {
            input: Input {
                source: "appsrc name=src".into(),
                label: "Prostir-1".into(),
            },
            origin: Origin::Monitor,
            memory: Memory::D3D11 { adapter: gpu },
            adapter: Some(gpu),
            cursor: Cursor::Separate,
        };
        // A display of the PC through Desktop Duplication.
        let duplication = FrameSource {
            input: Input {
                source: "d3d11screencapturesrc capture-api=dxgi".into(),
                label: "DISPLAY1".into(),
            },
            origin: Origin::Capture,
            memory: Memory::D3D11 { adapter: gpu },
            adapter: Some(gpu),
            cursor: Cursor::InFrame,
        };
        for source in [&driver, &duplication] {
            assert_eq!(source.adapter, Some(gpu));
            assert!(matches!(source.memory, Memory::D3D11 { adapter } if adapter == gpu));
            assert!(!source.input.source.is_empty());
        }
        assert_ne!(driver.origin, duplication.origin);
        assert_ne!(driver.cursor, duplication.cursor);
        assert_ne!(Memory::System, Memory::DmaBuf);

        // The lock screen or a UAC prompt: no frames for a while, nothing broken.
        let locked = OutputHealth::Paused {
            why: "the lock screen is shown".into(),
        };
        assert!(matches!(&locked, OutputHealth::Paused { why } if !why.is_empty()));
        assert_ne!(locked, OutputHealth::Closed);
    }

    #[test]
    fn a_windows_monitor_spec_fits_the_contract() {
        let spec = VirtualSpec {
            id: ScreenId::saved(7),
            number: 1,
            w: 2560,
            h: 1440,
            scale: 125,
            refresh: 120,
        };
        assert_eq!(spec.id, ScreenId::saved(7));
        assert_eq!(spec.to_string(), "2560x1440@1.25");
        assert!(with_driver().refresh.contains(&spec.refresh));
    }
}
