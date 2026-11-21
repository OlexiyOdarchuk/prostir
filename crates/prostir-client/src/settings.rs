//! Headset-side preferences chosen in the menu, kept between launches.

use std::path::Path;

pub use crate::background::Sky;
use crate::i18n::{Lang, Language};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Around the user: wherever a monitor is put, it turns to face them.
    Arc,
    /// A flat wall: all monitors in one plane, facing the same way, for walking along.
    Wall,
    /// Above the head, for lying down.
    Overhead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    Flat,
    Gentle,
    Strong,
}

impl Curve {
    /// Cylinder radius as a multiple of the monitor's distance from the user; `None` is flat.
    pub fn radius_factor(self) -> Option<f32> {
        match self {
            Curve::Flat => None,
            Curve::Gentle => Some(2.0),
            Curve::Strong => Some(1.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    Black,
    /// Passthrough cameras.
    Room,
    /// The room, darker: the monitors stand out more.
    RoomDim,
    /// A built-in panorama.
    Sky(Sky),
    /// The user's own `background.jpg/png`.
    Custom,
}

impl Background {
    /// Shows the room through the cameras.
    pub fn is_room(self) -> bool {
        matches!(self, Background::Room | Background::RoomDim)
    }
}

/// How the date on the palm menu is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateStyle {
    /// In the menu's language.
    #[default]
    LikeMenu,
    /// `Ср, 18 лист.`
    Ukrainian,
    /// `Wed, Nov 18`
    English,
    /// `18.11.2026`
    Numbers,
}

/// What tracked hands do in the headset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandMode {
    /// Not drawn and not tracked by the app (saves rendering every frame).
    Hidden,
    /// Drawn, e.g. to see them over the keyboard, but they do not click or drag.
    Visible,
    /// Drawn and point, click and drag. Off by default: hands over a keyboard
    /// would otherwise click things while typing.
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub preset: Preset,
    pub curve: Curve,
    pub background: Background,
    pub hands: HandMode,
    /// Dim the monitors the mouse is not on.
    pub focus: bool,
    /// One big screen further away; the other monitors are hidden.
    pub cinema: bool,
    /// Position in the row of the monitor shown in cinema mode; `None`: the largest
    /// landscape one.
    pub cinema_screen: Option<u8>,
    /// The first-run tips were shown and dismissed.
    pub intro_seen: bool,
    /// A passthrough window onto the real keyboard.
    pub keyboard_window: bool,
    /// The headset microphone may be used by the PC. Decided here, on the headset:
    /// the PC can only ask.
    pub mic: bool,
    /// The menu tab shown last (see `app::tab_code`).
    pub last_tab: u8,
    /// The monitors come along when the user turns or moves away.
    pub follow: bool,
    /// A monitor let go almost against another sticks to it.
    pub magnet: bool,
    /// Dragging one monitor moves the whole arrangement.
    pub move_all: bool,
    /// Sound placed on the side of the monitor whose window plays it.
    pub sound_from_screen: bool,
    /// The clock in 12-hour time (2:05 PM) rather than 24-hour (14:05).
    pub clock_12h: bool,
    /// How the date is written (see [`DateStyle`]).
    pub date_style: DateStyle,
    /// The round ☰ button under the main monitor.
    pub menu_button: bool,
    /// How bright the monitors are, percent (40 to 100).
    pub brightness: u8,
    /// The mouse goes to the monitor the user looks at (where the PC allows it).
    pub gaze_mouse: bool,
    /// A bigger menu, easier to read and hit.
    pub large_menu: bool,
    /// Left-handed: the wrist menu on the right wrist, touched with the left hand.
    pub left_handed: bool,
    /// The headset display at 72 Hz instead of 90 (the video is 60 frames a second).
    pub battery_saver: bool,
    /// The PC's desktop notifications shown in the headset.
    pub notifications: bool,
    /// "No limits": more monitors and displays than the safe budgets allow (the PC
    /// reads it and puts its own hard limits on top).
    pub no_limits: bool,
    /// Rest reminders: minutes of wearing between them (0: none).
    pub rest_every: u16,
    pub language: Language,
    /// The language of the PC last connected to (for "like the PC" before connecting).
    pub pc_language: Option<Lang>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preset: Preset::Arc,
            curve: Curve::Flat,
            background: Background::Black,
            hands: HandMode::Visible,
            focus: false,
            cinema: false,
            cinema_screen: None,
            intro_seen: false,
            keyboard_window: false,
            mic: true,
            last_tab: 0,
            follow: false,
            magnet: true,
            move_all: false,
            sound_from_screen: false,
            clock_12h: false,
            date_style: DateStyle::LikeMenu,
            menu_button: true,
            brightness: 100,
            gaze_mouse: false,
            large_menu: false,
            left_handed: false,
            battery_saver: false,
            notifications: false,
            no_limits: false,
            rest_every: 0,
            language: Language::LikePc,
            pc_language: None,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Reads `key value` lines; unknown keys are ignored and missing ones default.
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("preset", v)) => {
                    s.preset = match v {
                        "wall" => Preset::Wall,
                        "overhead" => Preset::Overhead,
                        _ => Preset::Arc,
                    }
                }
                Some(("curve", v)) => {
                    s.curve = match v {
                        "gentle" => Curve::Gentle,
                        "strong" => Curve::Strong,
                        _ => Curve::Flat,
                    }
                }
                Some(("focus", v)) => s.focus = v == "on",
                Some(("cinema", v)) => s.cinema = v == "on",
                Some(("cinema-screen", v)) => s.cinema_screen = v.parse().ok(),
                Some(("intro", v)) => s.intro_seen = v == "seen",
                Some(("keyboard-window", v)) => s.keyboard_window = v == "on",
                Some(("mic", v)) => s.mic = v != "off",
                Some(("tab", v)) => s.last_tab = v.trim().parse().unwrap_or(0),
                Some(("follow", v)) => s.follow = v == "on",
                Some(("magnet", v)) => s.magnet = v != "off",
                Some(("move-all", v)) => s.move_all = v == "on",
                Some(("sound-from-screen", v)) => s.sound_from_screen = v == "on",
                Some(("clock", v)) => s.clock_12h = v == "12h",
                Some(("date", v)) => {
                    s.date_style = match v {
                        "uk" => DateStyle::Ukrainian,
                        "en" => DateStyle::English,
                        "numbers" => DateStyle::Numbers,
                        _ => DateStyle::LikeMenu,
                    }
                }
                Some(("menu-button", v)) => s.menu_button = v != "off",
                Some(("brightness", v)) => {
                    s.brightness = v.trim().parse::<u8>().unwrap_or(100).clamp(40, 100);
                }
                Some(("gaze-mouse", v)) => s.gaze_mouse = v == "on",
                Some(("large-menu", v)) => s.large_menu = v == "on",
                Some(("left-handed", v)) => s.left_handed = v == "on",
                Some(("battery-saver", v)) => s.battery_saver = v == "on",
                Some(("notifications", v)) => s.notifications = v == "on",
                Some(("no-limits", v)) => s.no_limits = v == "on",
                Some(("rest-every", v)) => s.rest_every = v.trim().parse().unwrap_or(0),
                Some(("language", v)) => {
                    s.language = match v {
                        "uk" => Language::Fixed(Lang::Uk),
                        "en" => Language::Fixed(Lang::En),
                        _ => Language::LikePc,
                    }
                }
                Some(("pc-language", v)) => s.pc_language = Lang::from_code(v),
                Some(("hands", v)) => {
                    s.hands = match v {
                        "hidden" => HandMode::Hidden,
                        "on" | "control" => HandMode::Control,
                        _ => HandMode::Visible,
                    }
                }
                Some(("background", v)) => {
                    s.background = match v {
                        "room" => Background::Room,
                        "room-dim" => Background::RoomDim,
                        "custom" => Background::Custom,
                        "panorama" | "sky-night" => Background::Sky(Sky::Night),
                        "sky-sunset" => Background::Sky(Sky::Sunset),
                        "sky-space" => Background::Sky(Sky::Space),
                        "sky-aurora" => Background::Sky(Sky::Aurora),
                        _ => Background::Black,
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self, path: &Path) {
        if let Err(e) = prostir_proto::secure::write_atomic(path, self.to_text().as_bytes()) {
            log::warn!("cannot save settings to {}: {e}", path.display());
        }
    }

    pub fn to_text(&self) -> String {
        let preset = match self.preset {
            Preset::Arc => "arc",
            Preset::Wall => "wall",
            Preset::Overhead => "overhead",
        };
        let curve = match self.curve {
            Curve::Flat => "flat",
            Curve::Gentle => "gentle",
            Curve::Strong => "strong",
        };
        let background = match self.background {
            Background::Black => "black",
            Background::Room => "room",
            Background::RoomDim => "room-dim",
            Background::Custom => "custom",
            Background::Sky(Sky::Night) => "sky-night",
            Background::Sky(Sky::Sunset) => "sky-sunset",
            Background::Sky(Sky::Space) => "sky-space",
            Background::Sky(Sky::Aurora) => "sky-aurora",
        };
        let hands = match self.hands {
            HandMode::Hidden => "hidden",
            HandMode::Visible => "visible",
            HandMode::Control => "control",
        };
        let on = |b: bool| if b { "on" } else { "off" };
        format!(
            "preset {preset}\ncurve {curve}\nbackground {background}\nhands {hands}\n\
             focus {}\ncinema {}\ncinema-screen {}\nlanguage {}\npc-language {}\nintro {}\nkeyboard-window {}\nmic {}\ntab {}\nfollow {}\n\
             gaze-mouse {}\nlarge-menu {}\nleft-handed {}\nbattery-saver {}\nnotifications {}\nrest-every {}\nmagnet {}\nmove-all {}\nsound-from-screen {}\nclock {}\ndate {}\nmenu-button {}\nbrightness {}\nno-limits {}\n",
            on(self.focus),
            on(self.cinema),
            self.cinema_screen
                .map_or_else(|| "auto".to_owned(), |i| i.to_string()),
            match self.language {
                Language::LikePc => "like-pc",
                Language::Fixed(l) => l.code(),
            },
            self.pc_language.map_or("", Lang::code),
            if self.intro_seen { "seen" } else { "new" },
            on(self.keyboard_window),
            on(self.mic),
            self.last_tab,
            on(self.follow),
            on(self.gaze_mouse),
            on(self.large_menu),
            on(self.left_handed),
            on(self.battery_saver),
            on(self.notifications),
            self.rest_every,
            on(self.magnet),
            on(self.move_all),
            on(self.sound_from_screen),
            if self.clock_12h { "12h" } else { "24h" },
            match self.date_style {
                DateStyle::LikeMenu => "like-menu",
                DateStyle::Ukrainian => "uk",
                DateStyle::English => "en",
                DateStyle::Numbers => "numbers",
            },
            on(self.menu_button),
            self.brightness,
            on(self.no_limits)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("prostir-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.txt");
        let s = Settings {
            preset: Preset::Overhead,
            curve: Curve::Gentle,
            background: Background::Sky(Sky::Aurora),
            hands: HandMode::Control,
            focus: true,
            cinema: true,
            cinema_screen: Some(2),
            intro_seen: true,
            keyboard_window: true,
            mic: false,
            last_tab: 2,
            follow: true,
            magnet: false,
            move_all: true,
            sound_from_screen: true,
            clock_12h: true,
            date_style: DateStyle::Numbers,
            menu_button: false,
            brightness: 60,
            gaze_mouse: true,
            large_menu: true,
            left_handed: true,
            battery_saver: true,
            notifications: true,
            no_limits: true,
            rest_every: 20,
            language: Language::Fixed(Lang::En),
            pc_language: Some(Lang::Uk),
        };
        s.save(&path);
        assert_eq!(Settings::load(&path), s);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
