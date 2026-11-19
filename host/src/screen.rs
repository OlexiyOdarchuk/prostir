//! A screen of the PC shown in the headset: a virtual monitor, a window panel or a real
//! display, and the one key the PC knows it by.

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

/// A screen: what it is, its output on the desktop and the encoder streaming it.
pub struct Screen {
    pub id: ScreenId,
    pub kind: ScreenKind,
    // Field order matters: the encoder stops before the output goes away.
    pub stream: Option<Encoder>,
    pub output: Option<Output>,
}

impl Screen {
    /// A screen streaming `output` with `stream`.
    pub fn running(id: ScreenId, kind: ScreenKind, output: Output, stream: Encoder) -> Self {
        Self {
            id,
            kind,
            stream: Some(stream),
            output: Some(output),
        }
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
}
