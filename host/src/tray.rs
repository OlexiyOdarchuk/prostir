//! System tray icon (StatusNotifierItem): connection status, video quality,
//! start with the session, quit.

use crate::i18n::t;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{MenuItem, ToolTip};
use log::warn;

/// What the tray asks the main loop to do.
pub enum Command {
    SetBitrate(u32),
    /// Drop all paired headsets: each must pair again.
    ForgetHeadsets,
    Quit,
}

/// Video quality presets offered in the menu, Mbit/s per monitor.
const QUALITIES: [(u32, &str, &str); 3] = [
    (15, "Економна — 15 Мбіт/с", "Economy — 15 Mbit/s"),
    (30, "Звичайна — 30 Мбіт/с", "Normal — 30 Mbit/s"),
    (50, "Висока — 50 Мбіт/с", "High — 50 Mbit/s"),
];

#[derive(Clone, Default, PartialEq)]
pub struct Status {
    /// "Wi-Fi" or "USB" while a headset is connected.
    pub connected: Option<&'static str>,
    pub monitors: usize,
    pub mirror: bool,
    /// Connected, but the headset is not worn: streaming paused.
    pub paused: bool,
}

pub struct Tray {
    status: Status,
    /// Code to type in the headset, while pairing.
    pairing_code: Option<String>,
    bitrate: u32,
    commands: Sender<Command>,
}

pub type Handle = ksni::blocking::Handle<Tray>;

/// Shows the tray icon; `None` when the desktop has no tray.
pub fn spawn(commands: Sender<Command>, bitrate: u32) -> Option<Handle> {
    let tray = Tray {
        status: Status::default(),
        pairing_code: None,
        bitrate,
        commands,
    };
    tray.spawn()
        .inspect_err(|e| warn!("no system tray, running without an icon: {e}"))
        .ok()
}

/// Updates what the tray shows.
pub fn set_status(handle: Option<&Handle>, status: Status) {
    if let Some(h) = handle {
        h.update(|t| t.status = status);
    }
}

/// Shows (or hides) the pairing code in the tray.
pub fn set_pairing(handle: Option<&Handle>, code: Option<String>) {
    if let Some(h) = handle {
        h.update(|t| t.pairing_code = code);
    }
}

fn autostart_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("prostir.desktop"))
}

fn autostart_enabled() -> bool {
    autostart_file().is_some_and(|p| p.exists())
}

fn set_autostart(enabled: bool) {
    let Some(path) = autostart_file() else { return };
    let result = if enabled {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "prostir".into());
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Prostir\n\
             Comment=Virtual monitors for Meta Quest\nExec={exe}\nIcon=prostir\n\
             Terminal=false\nX-KDE-autostart-phase=2\n"
        );
        path.parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, entry))
    } else {
        std::fs::remove_file(&path)
    };
    if let Err(e) = result {
        warn!("cannot change autostart ({}): {e}", path.display());
    }
}

/// The app icon when installed (see `scripts/install.sh`), otherwise a stock one.
fn icon() -> String {
    let installed = std::env::var_os("HOME").is_some_and(|h| {
        PathBuf::from(h)
            .join(".local/share/icons/hicolor/scalable/apps/prostir.svg")
            .exists()
    }) || PathBuf::from("/usr/share/icons/hicolor/scalable/apps/prostir.svg")
        .exists();
    if installed {
        "prostir"
    } else {
        "video-display"
    }
    .into()
}

impl Tray {
    fn status_line(&self) -> String {
        match self.status.connected {
            Some(_) if self.status.paused => t(
                "Шолом знято — трансляція на паузі",
                "Headset taken off — streaming paused",
            )
            .into(),
            Some(via) => format!("{} ({via})", t("Шолом підключено", "Headset connected")),
            None => t("Чекаю на шолом…", "Waiting for the headset…").into(),
        }
    }

    fn monitors_line(&self) -> String {
        let mirror = if self.status.mirror {
            t(
                ", екран ноутбука в шоломі",
                ", laptop screen in the headset",
            )
        } else {
            ""
        };
        match self.status.monitors {
            0 => t("Віртуальних моніторів немає", "No virtual monitors").into(),
            n => format!(
                "{}: {n}{mirror}",
                t("Віртуальних моніторів", "Virtual monitors")
            ),
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "prostir".into()
    }

    fn title(&self) -> String {
        "Prostir".into()
    }

    fn icon_name(&self) -> String {
        icon()
    }

    fn status(&self) -> ksni::Status {
        // Waiting quietly sits in the tray's overflow; a session is shown.
        if self.pairing_code.is_some() {
            ksni::Status::NeedsAttention
        } else if self.status.connected.is_some() {
            ksni::Status::Active
        } else {
            ksni::Status::Passive
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Prostir".into(),
            description: match &self.pairing_code {
                Some(code) => format!("{}: {code}", t("Код сполучення", "Pairing code")),
                None => format!("{}\n{}", self.status_line(), self.monitors_line()),
            },
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let selected = QUALITIES
            .iter()
            .position(|(m, _, _)| *m == self.bitrate)
            .unwrap_or(1);
        let mut items: Vec<MenuItem<Self>> = Vec::new();
        if let Some(code) = &self.pairing_code {
            items.push(
                StandardItem {
                    label: format!("{}: {code}", t("Код сполучення", "Pairing code")),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            items.push(MenuItem::Separator);
        }
        items.extend([
            StandardItem {
                label: self.status_line(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: self.monitors_line(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: t("Якість відео", "Video quality").into(),
                submenu: vec![
                    RadioGroup {
                        selected,
                        select: Box::new(|tray: &mut Self, i| {
                            let mbps = QUALITIES[i].0;
                            tray.bitrate = mbps;
                            let _ = tray.commands.send(Command::SetBitrate(mbps));
                        }),
                        options: QUALITIES
                            .iter()
                            .map(|(_, uk, en)| RadioItem {
                                label: t(uk, en).into(),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: t("Запускати разом із системою", "Start with the system").into(),
                checked: autostart_enabled(),
                activate: Box::new(|_: &mut Self| set_autostart(!autostart_enabled())),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: t("Забути сполучені шоломи", "Forget paired headsets").into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(Command::ForgetHeadsets);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: t("Вийти", "Quit").into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(Command::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}
