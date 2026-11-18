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

/// Video quality presets offered in the menu, Mbit/s per monitor.
const QUALITIES: [(u32, &str, &str); 3] = [
    (15, "Економна: 15 Мбіт/с", "Economy: 15 Mbit/s"),
    (30, "Звичайна: 30 Мбіт/с", "Normal: 30 Mbit/s"),
    (50, "Висока: 50 Мбіт/с", "High: 50 Mbit/s"),
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
    sharp: bool,
    /// The headset's battery: percent, charging.
    battery: Option<(u8, bool)>,
    commands: Sender<Command>,
}

pub type Handle = ksni::blocking::Handle<Tray>;

/// Shows the tray icon; `None` when the desktop has no tray.
pub fn spawn(commands: Sender<Command>, bitrate: u32, sharp: bool) -> Option<Handle> {
    // Rewritten on each start, so it names this install's program even after an
    // upgrade moved it (a Nix store path, say).
    if autostart_enabled() {
        set_autostart(true);
    }
    let tray = Tray {
        status: Status::default(),
        pairing_code: None,
        bitrate,
        sharp,
        battery: None,
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
/// Quality and the text preset changed elsewhere (the headset menu).
/// The headset's battery as it last reported it (`None`: no headset).
pub fn set_battery(handle: Option<&Handle>, battery: Option<(u8, bool)>) {
    if let Some(h) = handle {
        h.update(|t| t.battery = battery);
    }
}

pub fn set_encoding(handle: Option<&Handle>, bitrate: u32, sharp: bool) {
    if let Some(h) = handle {
        h.update(|t| {
            t.bitrate = bitrate;
            t.sharp = sharp;
        });
    }
}

pub fn set_pairing(handle: Option<&Handle>, code: Option<String>) {
    if let Some(h) = handle {
        h.update(|t| t.pairing_code = code);
    }
}

/// The label of an on/off item. Plasma draws a checkbox; GTK menus (Waybar's tray)
/// often draw nothing for an unchecked item, so there the state goes into the text.
fn with_state(label: &str, on: bool) -> String {
    let plasma = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.contains("KDE"));
    if plasma {
        label.to_owned()
    } else if on {
        format!("{label}: {}", t("увімкнено", "on"))
    } else {
        format!("{label}: {}", t("вимкнено", "off"))
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
        // A wrapper (Nix) names the program to start; else this very file.
        let exe = std::env::var("PROSTIR_EXE")
            .ok()
            .filter(|e| !e.is_empty())
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(|| "prostir".into());
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
                "Шолом знято, трансляцію призупинено",
                "Headset taken off, streaming paused",
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
        // Always visible (a passive icon hides in Plasma's overflow, and a fresh install
        // would show no sign of running at all); a code to read asks for attention.
        if self.pairing_code.is_some() {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: format!("Prostir {}", env!("CARGO_PKG_VERSION")),
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
        ]);
        if let Some((percent, charging)) = self.battery {
            items.push(
                StandardItem {
                    label: format!(
                        "{} {percent}%{}",
                        t("Заряд шолома:", "Headset battery:"),
                        if charging {
                            t(", заряджається", ", charging")
                        } else {
                            ""
                        }
                    ),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        if self.status.connected.is_some() {
            items.push(
                StandardItem {
                    label: t("Монітори перед собою", "Bring the monitors in front").into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.commands.send(Command::Recenter);
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.extend([
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
                label: with_state(
                    t(
                        "Чіткіший текст (кодування на ~2 мс довше)",
                        "Sharper text (about 2 ms more encoding)",
                    ),
                    self.sharp,
                ),
                checked: self.sharp,
                activate: Box::new(|tray: &mut Self| {
                    tray.sharp = !tray.sharp;
                    let _ = tray.commands.send(Command::SetSharp(tray.sharp));
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: with_state(
                    t(
                        "Рахувати цей ПК серед користувачів",
                        "Count this PC among users",
                    ),
                    crate::usage::enabled(),
                ),
                checked: crate::usage::enabled(),
                visible: crate::usage::available(),
                activate: Box::new(|tray: &mut Self| {
                    let on = !crate::usage::enabled();
                    let _ = tray.commands.send(Command::SetCounting(on));
                    // The tray re-reads the choice; write it now so the check mark follows.
                    crate::usage::set_enabled(on);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: t("Запускати разом із системою", "Start with the system").into(),
                checked: autostart_enabled(),
                // Hyprland (without uwsm) ignores XDG autostart: its own config starts it.
                visible: !crate::platform::hyprland::running(),
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
                label: format!("Prostir {}", env!("CARGO_PKG_VERSION")),
                enabled: false,
                ..Default::default()
            }
            .into(),
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
