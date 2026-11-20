//! A screen of the PC shown in the headset: a virtual monitor, a window panel or a real
//! display, and the one key the PC knows it by.

use std::time::{Duration, Instant};

use prostir_proto::{MIRROR_ID, WINDOW_IDS};

use crate::encoder::Encoder;
use crate::platform::{ActiveWindow, MonitorSpec, Output, OutputBackend};

/// The PC's key for a screen: in its set, its rows and its saved files. A virtual
/// monitor keeps its number between runs, so the headset finds it where it was.
///
/// The protocol still numbers streams with a `u8` (virtual monitors `0..`, window
/// panels `10..13`, the laptop screen `100`), and the numbers here are the same, so
/// [`ScreenId::wire`] and [`ScreenId::from_wire`] are the only places they cross.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScreenId(u32);

impl ScreenId {
    /// The laptop screen (the physical display streamed as it is).
    pub const BUILTIN: Self = Self(MIRROR_ID as u32);

    /// A virtual monitor's number as saved (`0..`).
    pub fn saved(n: u32) -> Self {
        Self(n)
    }

    /// The number saved for it.
    pub fn number(self) -> u32 {
        self.0
    }

    /// The lowest number for a new virtual monitor that none of `taken` has.
    pub fn first_free(taken: impl Fn(Self) -> bool) -> Self {
        (0..)
            .map(Self)
            .find(|id| !taken(*id) && !Self::panels().any(|p| p == *id) && *id != Self::BUILTIN)
            .expect("fewer screens than numbers")
    }

    /// Numbers of window panels, given out for one session.
    pub fn panels() -> impl Iterator<Item = Self> {
        WINDOW_IDS.map(|id| Self(u32::from(id)))
    }

    /// The stream id in protocol messages.
    pub fn wire(self) -> u8 {
        u8::try_from(self.0).expect("screen numbers fit the protocol's stream ids")
    }

    /// The screen a protocol message names.
    pub fn from_wire(id: u8) -> Self {
        Self(u32::from(id))
    }

    /// The name the screen's output and stream go by: `Prostir-N` from 1.
    pub fn name(self) -> String {
        if self == Self::BUILTIN {
            "mirror".into()
        } else {
            format!("Prostir-{}", self.0 + 1)
        }
    }
}

impl std::fmt::Display for ScreenId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}

/// Stream ids of `ids`, for a protocol message.
pub fn wire_ids(ids: &[ScreenId]) -> Vec<u8> {
    ids.iter().map(|id| id.wire()).collect()
}

/// What a screen shows.
#[derive(Debug, Clone)]
pub enum ScreenKind {
    /// A virtual monitor made for the headset.
    Virtual { spec: MonitorSpec },
    /// An output made for one window, shown as a floating panel; `origin` is the window
    /// as it was before, to put it back.
    Window {
        origin: ActiveWindow,
        spec: MonitorSpec,
    },
    /// A real display streamed as it is (`builtin`: the laptop's own panel).
    Physical { connector: String, builtin: bool },
}

impl ScreenKind {
    /// The mode it was made with (none for a real display).
    pub fn spec(&self) -> Option<&MonitorSpec> {
        match self {
            Self::Virtual { spec } | Self::Window { spec, .. } => Some(spec),
            Self::Physical { .. } => None,
        }
    }
}

/// How a screen is doing.
#[derive(Debug, Clone)]
pub enum ScreenState {
    /// Being made: no output or encoder yet.
    Starting,
    Running,
    /// Its output or encoder could not start: it keeps its place in the set and the
    /// rows, and is tried again at the next session.
    Failed {
        why: String,
    },
}

/// A screen: what it is, its output on the desktop and the encoder streaming it.
pub struct Screen {
    pub id: ScreenId,
    pub kind: ScreenKind,
    // Field order matters: the encoder stops before the output goes away.
    pub stream: Option<Encoder>,
    pub output: Option<Output>,
    pub state: ScreenState,
    /// Puts the output's size back when another program changes it.
    pub size_guard: SizeGuard,
}

impl Screen {
    /// A screen streaming `output` with `stream`.
    pub fn streaming(id: ScreenId, kind: ScreenKind, output: Output, stream: Encoder) -> Self {
        Self {
            id,
            kind,
            stream: Some(stream),
            output: Some(output),
            state: ScreenState::Running,
            size_guard: SizeGuard::default(),
        }
    }

    /// A screen still to be made.
    pub fn starting(id: ScreenId, kind: ScreenKind) -> Self {
        Self {
            id,
            kind,
            stream: None,
            output: None,
            state: ScreenState::Starting,
            size_guard: SizeGuard::default(),
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.state, ScreenState::Running)
    }

    /// The desktop's name for its output, while it has one.
    pub fn output_name(&self) -> Option<&str> {
        self.output.as_ref().map(OutputBackend::output_name)
    }

    pub fn is_virtual(&self) -> bool {
        matches!(self.kind, ScreenKind::Virtual { .. })
    }

    pub fn is_window(&self) -> bool {
        matches!(self.kind, ScreenKind::Window { .. })
    }

    pub fn is_builtin(&self) -> bool {
        matches!(self.kind, ScreenKind::Physical { builtin: true, .. })
    }
}

/// Restores of one monitor's size allowed in [`RESTORE_WINDOW`].
const RESTORES_ALLOWED: usize = 3;
const RESTORE_WINDOW: Duration = Duration::from_secs(60);
/// How long a restart has to put the size back before the output is made anew.
const RESTORE_WAIT: Duration = Duration::from_secs(2);

/// What to do about a monitor's size (see [`SizeGuard::check`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeAction {
    /// The size is right, or the last step still has time to work.
    Wait,
    /// The size is right again: lay the desktop out again for it.
    Restored,
    /// Start its encoder over: the desktop gives the output back its size.
    Restart,
    /// The restart did not help: make the output anew with the same number and spec.
    Recreate,
    /// Restored too often lately: say so once and leave the size as it is.
    GiveUp,
}

/// Puts a monitor's size back after another program changed it, without fighting
/// that program forever.
#[derive(Default)]
pub struct SizeGuard {
    /// When the size was restored (restarts and recreations), within the window.
    restores: Vec<Instant>,
    /// The last step taken and when, while the size is still wrong.
    last: Option<(SizeAction, Instant)>,
    /// Gave up (and said so): no more restores until the size is right again with
    /// room in the window.
    gave_up: bool,
    /// The size was wrong at the last check.
    wrong: bool,
}

impl SizeGuard {
    /// What to do at `now`, given the size another program gave the monitor
    /// (`None` while it has its own size). Records the step it returns.
    pub fn check(&mut self, resized: Option<(u32, u32)>, now: Instant) -> SizeAction {
        self.restores
            .retain(|&at| now.duration_since(at) < RESTORE_WINDOW);
        let full = self.restores.len() >= RESTORES_ALLOWED;
        if resized.is_none() {
            self.last = None;
            // A give-up ends once the window has room again, so it is said once.
            self.gave_up &= full;
            return if std::mem::take(&mut self.wrong) {
                SizeAction::Restored
            } else {
                SizeAction::Wait
            };
        }
        self.wrong = true;
        if self.gave_up {
            return SizeAction::Wait;
        }
        let step = match self.last {
            Some((_, at)) if now.duration_since(at) < RESTORE_WAIT => return SizeAction::Wait,
            Some((SizeAction::Restart, _)) => SizeAction::Recreate,
            _ => SizeAction::Restart,
        };
        if full {
            self.last = None;
            self.gave_up = true;
            return SizeAction::GiveUp;
        }
        self.restores.push(now);
        self.last = Some((step, now));
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers are the protocol's: virtual monitors from 0, never a panel's or the
    /// laptop screen's.
    #[test]
    fn numbers_are_the_protocols() {
        assert_eq!(ScreenId::BUILTIN.wire(), MIRROR_ID);
        assert_eq!(ScreenId::from_wire(3), ScreenId::from_wire(3));
        let taken = [0, 1, 3].map(ScreenId::from_wire);
        assert_eq!(
            ScreenId::first_free(|id| taken.contains(&id)),
            ScreenId::from_wire(2)
        );
        let ten: Vec<ScreenId> = (0..10).map(ScreenId::from_wire).collect();
        assert_eq!(
            ScreenId::first_free(|id| ten.contains(&id)),
            ScreenId::from_wire(13),
            "past the window panels"
        );
        assert_eq!(ScreenId::from_wire(0).name(), "Prostir-1");
    }

    const SHRUNK: Option<(u32, u32)> = Some((320, 240));

    fn secs(start: Instant, s: f64) -> Instant {
        start + Duration::from_secs_f64(s)
    }

    #[test]
    fn a_resized_monitor_is_restarted_once_and_then_left_to_settle() {
        let start = Instant::now();
        let mut guard = SizeGuard::default();
        assert_eq!(guard.check(None, start), SizeAction::Wait);
        assert_eq!(guard.check(SHRUNK, secs(start, 1.0)), SizeAction::Restart);
        // The size comes back a moment after the restart.
        assert_eq!(guard.check(SHRUNK, secs(start, 1.5)), SizeAction::Wait);
        assert_eq!(guard.check(None, secs(start, 2.0)), SizeAction::Restored);
        assert_eq!(guard.check(None, secs(start, 2.5)), SizeAction::Wait);
        assert_eq!(guard.restores.len(), 1);
    }

    #[test]
    fn a_size_that_does_not_come_back_makes_the_output_anew() {
        let start = Instant::now();
        let mut guard = SizeGuard::default();
        assert_eq!(guard.check(SHRUNK, start), SizeAction::Restart);
        assert_eq!(guard.check(SHRUNK, secs(start, 1.9)), SizeAction::Wait);
        assert_eq!(guard.check(SHRUNK, secs(start, 2.0)), SizeAction::Recreate);
        // The new output gets its time too; still wrong after that, a restart again.
        assert_eq!(guard.check(SHRUNK, secs(start, 3.0)), SizeAction::Wait);
        assert_eq!(guard.check(SHRUNK, secs(start, 4.0)), SizeAction::Restart);
        assert_eq!(guard.check(None, secs(start, 4.5)), SizeAction::Restored);
    }

    #[test]
    fn a_program_that_keeps_resizing_is_not_fought_forever() {
        let start = Instant::now();
        let mut guard = SizeGuard::default();
        let mut actions = Vec::new();
        // Resized again every 3 s, each time after the size came back.
        for i in 0..5 {
            let at = f64::from(i) * 3.0;
            actions.push(guard.check(SHRUNK, secs(start, at)));
            guard.check(None, secs(start, at + 0.5));
        }
        use SizeAction::*;
        assert_eq!(actions, [Restart, Restart, Restart, GiveUp, Wait]);
        // Still shrunk: no more restores.
        assert_eq!(guard.check(SHRUNK, secs(start, 20.0)), Wait);
        assert_eq!(guard.check(SHRUNK, secs(start, 90.0)), Wait);
        // Back by other means (the headset taken off and put on): laid out again.
        assert_eq!(guard.check(None, secs(start, 91.0)), Restored);
        // After a quiet minute the guard starts over.
        assert_eq!(guard.check(SHRUNK, secs(start, 92.0)), Restart);
    }
}
