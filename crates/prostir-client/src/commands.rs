//! What the headset does for a command from the PC (its tray, later `prostir ctl` and
//! hotkeys): the menu's own actions, so a command does nothing the menu could not. The
//! headset's microphone switch is not among them: only the user changes it, in the
//! headset (kind 9, which once set it, is retired and answered `UNSUPPORTED`).

#![forbid(unsafe_code)]

use prostir_proto::request::{Asked, Command};
use prostir_proto::state::ErrorCode;

use crate::settings::Preset;
use crate::ui::Action;

/// What a command comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Does {
    /// What the menu does for it; answered once done.
    Act(Action),
    /// Switch to profile `index`: answered once the PC answered its monitors.
    Profile(usize),
    /// Closer (below 0) or further.
    Closer(i8),
    /// "No limits".
    NoLimits(bool),
    /// Answered with this failure, and nothing changes.
    Fail(ErrorCode, String),
}

/// What to do for `asked`. A kind this app does not know is unsupported.
pub fn what_to_do(asked: &Asked<Command>) -> Does {
    let Asked::Known(command) = asked else {
        return Does::Fail(ErrorCode::UNSUPPORTED, String::new());
    };
    match command {
        Command::Recenter => Does::Act(Action::Recenter),
        Command::Profile { name } => match profile_named(name) {
            Some(i) => Does::Profile(i),
            None => Does::Fail(ErrorCode::NO_SUCH_PROFILE, name.clone()),
        },
        Command::Cinema { on } => Does::Act(Action::Cinema(*on)),
        Command::FocusMode { on } => Does::Act(Action::Focus(*on)),
        Command::Distance { steps } => Does::Closer(*steps),
        Command::Preset { preset } => match preset {
            0 => Does::Act(Action::Preset(Preset::Arc)),
            1 => Does::Act(Action::Preset(Preset::Wall)),
            2 => Does::Act(Action::Preset(Preset::Overhead)),
            _ => Does::Fail(ErrorCode::BAD_ARGUMENT, preset.to_string()),
        },
        Command::LineUp => Does::Act(Action::Tidy),
        Command::ShowMouse => Does::Act(Action::FindMouse),
        Command::Follow { on } => Does::Act(Action::Follow(*on)),
        // Comes with the arrangement's own step.
        Command::ResetArrangement => Does::Fail(ErrorCode::UNSUPPORTED, String::new()),
        Command::NoLimits { on } => Does::NoLimits(*on),
    }
}

/// The profile slot a command names: by its number from 1, as the menu shows it.
pub fn profile_named(name: &str) -> Option<usize> {
    name.trim()
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=crate::profiles::SLOTS).contains(n))
        .map(|n| n - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prostir_proto::wire::decode;

    /// Whatever the PC sends, the headset's microphone switch stays as the user left
    /// it: no command of any kind, with any fields, comes to the switch, and the kind
    /// that once set it is unsupported.
    #[test]
    fn the_pc_cannot_turn_the_microphone_on() {
        let bodies: [&[u8]; 4] = [&[], &[0], &[1], &[1, 1]];
        for kind in 0..=u8::MAX {
            for body in bodies {
                let mut bytes = vec![kind];
                bytes.extend_from_slice(body);
                let Ok(asked) = decode::<Asked<Command>>(&bytes) else {
                    continue;
                };
                let does = what_to_do(&asked);
                assert!(
                    !matches!(does, Does::Act(Action::SetMic(_) | Action::AskMic)),
                    "kind {kind} {body:?} comes to {does:?}"
                );
            }
        }
        let set_mic = decode::<Asked<Command>>(&[9, 1]).unwrap();
        assert_eq!(set_mic, Asked::Unknown { kind: 9 });
        assert_eq!(
            what_to_do(&set_mic),
            Does::Fail(ErrorCode::UNSUPPORTED, String::new())
        );
    }

    #[test]
    fn commands_are_the_menus_actions() {
        let does = |c: Command| what_to_do(&Asked::Known(c));
        assert_eq!(does(Command::LineUp), Does::Act(Action::Tidy));
        assert_eq!(
            does(Command::Profile { name: " 2 ".into() }),
            Does::Profile(1)
        );
        assert!(matches!(
            does(Command::Profile {
                name: "Cinema".into()
            }),
            Does::Fail(ErrorCode::NO_SUCH_PROFILE, _)
        ));
        assert_eq!(
            does(Command::Profile { name: "3".into() }),
            Does::Profile(2)
        );
        assert!(matches!(
            does(Command::Profile { name: "4".into() }),
            Does::Fail(ErrorCode::NO_SUCH_PROFILE, _)
        ));
        assert!(matches!(
            does(Command::Preset { preset: 7 }),
            Does::Fail(ErrorCode::BAD_ARGUMENT, _)
        ));
        assert_eq!(
            what_to_do(&Asked::Unknown { kind: 200 }),
            Does::Fail(ErrorCode::UNSUPPORTED, String::new())
        );
    }
}
