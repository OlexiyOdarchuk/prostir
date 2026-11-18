//! The floating menu panel. It is drawn on the CPU into an RGBA image and only
//! re-uploaded when something changes (hover, a click, news from the host).

use std::collections::HashMap;

use fontdue::{Font, FontSettings, Metrics};
use prostir_proto::PcSettings;
use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::i18n::{Lang, Language};
use crate::profiles::Profile;
use crate::settings::{Background, Curve, HandMode, Preset, Settings, Sky};

pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 960;
/// Same limit as the host's.
pub const MAX_MONITORS: usize = 6;

static REGULAR: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");
static MEDIUM: &[u8] = include_bytes!("../assets/NotoSans-Medium.ttf");

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorEntry {
    pub id: u8,
    pub width: u32,
    pub height: u32,
}

/// Pairing with a new PC: either show `code` (the PC asks for it) or let the user
/// type the PC's code into `typed`.
#[derive(Debug, Clone, PartialEq)]
pub struct PairingView {
    pub host: String,
    pub code: Option<String>,
    pub typed: String,
    /// The last attempt's codes did not match.
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    pub pairing: Option<PairingView>,
    /// A PC that announces itself but does not accept the connection (a firewall).
    pub unreachable: Option<String>,
    /// An unknown PC: (its name, the last pairing stopped after wrong codes).
    pub pair_offer: Option<(String, bool)>,
    /// Name of the connected PC, `None` while searching.
    pub host: Option<String>,
    /// PCs seen on the network while not connected: (name, address).
    pub found: Vec<(String, String)>,
    /// IP being typed on the keypad.
    pub ip_input: Option<String>,
    /// Quest monitors, sorted by id.
    pub monitors: Vec<MonitorEntry>,
    pub primary: Option<u8>,
    pub mirror_wanted: bool,
    /// PC sound goes to the headset.
    pub audio_wanted: bool,
    pub mic_wanted: bool,
    /// Access to the microphone was refused in the system.
    pub mic_denied: bool,
    /// How the PC encodes and what it can encode with (while connected).
    pub pc: Option<(PcSettings, u8)>,
    /// The physical display is being shown (its lid is open).
    pub mirror_active: bool,
    pub settings: Settings,
    pub room_available: bool,
    pub panorama_available: bool,
    /// The user put a `background.jpg/png` on the headset.
    pub custom_available: bool,
    pub curve_available: bool,
    /// The headset can show a passthrough window onto the keyboard.
    pub keyboard_available: bool,
    pub error: Option<String>,
    /// Monitor whose "×" was pressed once and now asks for confirmation.
    pub confirm_remove: Option<u8>,
    /// A profile whose "Save current" was pressed once (press again to overwrite).
    pub confirm_save: Option<usize>,
    pub tab: Tab,
    pub lang: Lang,
    /// First run: show the tips page instead of the tabs.
    pub intro: bool,
    /// The PC speaks another protocol: (the PC is newer, the PC app's version).
    pub update: Option<(bool, String)>,
    pub profiles: Vec<Profile>,
    /// The profile last switched to (until something is changed by hand).
    pub active_profile: Option<usize>,
}

/// Pages of the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// Large tiles for what is used every day.
    #[default]
    Quick,
    Monitors,
    Settings(Section),
}

/// A part of the Settings tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Look,
    Sound,
    Language,
    Pc,
    Help,
}

impl Tab {
    fn section(self) -> Option<Section> {
        match self {
            Tab::Settings(s) => Some(s),
            _ => None,
        }
    }

    /// The same top-level tab (any section of Settings counts as Settings).
    fn same_page(self, other: Tab) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Add {
        width: u32,
        height: u32,
    },
    Remove(u8),
    MakePrimary(u8),
    /// Move a monitor left (-1) or right (+1) in the row.
    Move {
        id: u8,
        delta: i32,
    },
    SetMirror(bool),
    SetAudio(bool),
    SetMic(bool),
    Preset(Preset),
    Curve(Curve),
    Background(Background),
    Hands(HandMode),
    Focus(bool),
    KeyboardWindow(bool),
    Cinema(bool),
    /// Which monitor (position in the row) cinema mode shows.
    CinemaScreen(u8),
    ApplyProfile(usize),
    SaveProfile(usize),
    Tab(Tab),
    /// Lay everything out again in front of the user, where they look now.
    Recenter,
    /// The first-run tips were read.
    IntroDone,
    Language(Language),
    /// Connect to the n-th PC in `UiState::found`.
    ConnectTo(usize),
    ShowKeypad,
    Key(char),
    Backspace,
    ConnectTyped,
    CancelKeypad,
    /// Disconnect and pick another PC.
    ChangeHost,
    /// Pair the other way round (swap which side shows the code).
    PairOtherWay,
    /// Answer to "Pair with this PC?".
    PairAnswer(bool),
    /// Ask for microphone access again after it was refused.
    AskMic,
    /// Change how the PC encodes.
    SetPc(PcSettings),
    /// Show the first-run tips again.
    ShowTips,
    /// The monitors follow the head.
    Follow(bool),
    Close,
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Normal,
    Selected,
    /// A tab: the current one, or one to switch to.
    TabOn,
    TabOff,
    Danger,
    /// Asking to confirm a destructive action.
    Armed,
    Disabled,
}

enum Item {
    Panel(Rect),
    Box(Rect, [u8; 4]),
    /// A small filled circle: a status light.
    Dot {
        x: f32,
        y: f32,
        r: f32,
        color: [u8; 4],
    },
    Text {
        x: f32,
        baseline: f32,
        size: f32,
        medium: bool,
        color: [u8; 4],
        text: String,
    },
    Button {
        rect: Rect,
        label: String,
        style: Style,
        action: Option<Action>,
    },
}

const TEXT: [u8; 4] = [232, 234, 240, 255];
/// Secondary text: light enough to read comfortably in the headset.
const MUTED: [u8; 4] = [168, 175, 190, 255];
/// Section titles above groups of options.
const SECTION: [u8; 4] = [206, 211, 222, 255];
/// Status lights.
const GOOD: [u8; 4] = [74, 222, 128, 255];
const WAIT: [u8; 4] = [251, 191, 36, 255];
const ACCENT: [u8; 4] = [61, 126, 255, 255];
const DANGER: [u8; 4] = [255, 107, 107, 255];

pub struct Panel {
    regular: Font,
    medium: Font,
    glyphs: HashMap<(bool, char, u32), (Metrics, Vec<u8>)>,
    items: Vec<Item>,
    state: Option<UiState>,
    hovered: Option<usize>,
    pixmap: Pixmap,
    dirty: bool,
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel {
    pub fn new() -> Self {
        let load = |bytes| Font::from_bytes(bytes, FontSettings::default()).expect("bundled font");
        Self {
            regular: load(REGULAR),
            medium: load(MEDIUM),
            glyphs: HashMap::new(),
            items: Vec::new(),
            state: None,
            hovered: None,
            pixmap: Pixmap::new(WIDTH, HEIGHT).expect("panel size"),
            dirty: true,
        }
    }

    pub fn set_state(&mut self, state: &UiState) {
        if self.state.as_ref() == Some(state) {
            return;
        }
        self.items = self.build(state);
        self.state = Some(state.clone());
        self.hovered = None;
        self.dirty = true;
    }

    /// Updates the hovered button from a point in panel pixels (`None`: not pointing at the panel).
    pub fn hover(&mut self, point: Option<(f32, f32)>) {
        let hovered = point.and_then(|(x, y)| {
            self.items.iter().position(|item| match item {
                Item::Button {
                    rect,
                    action: Some(_),
                    style,
                    ..
                } => {
                    *style != Style::Disabled
                        && x >= rect.left()
                        && x <= rect.right()
                        && y >= rect.top()
                        && y <= rect.bottom()
                }
                _ => false,
            })
        });
        if hovered != self.hovered {
            self.hovered = hovered;
            self.dirty = true;
        }
    }

    pub fn hovered_action(&self) -> Option<Action> {
        match self.items.get(self.hovered?)? {
            Item::Button { action, .. } => *action,
            _ => None,
        }
    }

    /// Labels and texts that do not fit: a button label wider than its button, text
    /// running off the panel or into another text or button. Empty when the layout is
    /// clean; checked by the tests for every page in both languages.
    pub fn overflows(&mut self) -> Vec<String> {
        let items = std::mem::take(&mut self.items);
        let mut boxes: Vec<(Rect, String, bool)> = Vec::new(); // (bounds, text, is_button)
        let mut found = Vec::new();
        for item in &items {
            match item {
                Item::Button { rect: r, label, .. } => {
                    found.extend(self.missing_glyphs(label));
                    let size = button_font_size(label);
                    let tw = self.text_width(label, size, true);
                    if tw > r.width() - 16.0 {
                        found.push(format!(
                            "button too narrow: {label:?} ({tw:.0} > {:.0})",
                            r.width() - 16.0
                        ));
                    }
                    // An empty label: a tile, whose own texts are drawn on top of it.
                    if !label.is_empty() {
                        boxes.push((*r, label.clone(), true));
                    }
                }
                Item::Text {
                    x,
                    baseline,
                    size,
                    medium,
                    text,
                    ..
                } => {
                    found.extend(self.missing_glyphs(text));
                    let tw = self.text_width(text, *size, *medium);
                    if x + tw > WIDTH as f32 - 32.0 {
                        found.push(format!("off the panel: {text:?}"));
                    }
                    let r = rect(*x, baseline - size * 0.75, tw, size * 0.95);
                    boxes.push((r, text.clone(), false));
                }
                _ => {}
            }
        }
        for (i, (a, ta, a_button)) in boxes.iter().enumerate() {
            for (b, tb, b_button) in &boxes[i + 1..] {
                if (*a_button && *b_button) || !overlap(a, b) {
                    continue;
                }
                found.push(format!("{ta:?} overlaps {tb:?}"));
            }
        }
        self.items = items;
        found
    }

    fn missing_glyphs(&self, text: &str) -> Option<String> {
        let missing: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && self.regular.lookup_glyph_index(*c) == 0)
            .collect();
        (!missing.is_empty()).then(|| format!("no glyph for {missing:?} in {text:?}"))
    }

    /// Premultiplied RGBA, top row first, when it changed since the last call.
    pub fn render_if_dirty(&mut self) -> Option<&[u8]> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        self.render();
        Some(self.pixmap.data())
    }

    fn build(&mut self, s: &UiState) -> Vec<Item> {
        let l = s.lang;
        let mut items = vec![Item::Panel(rect(0.0, 0.0, WIDTH as f32, HEIGHT as f32))];
        let w = WIDTH as f32;

        // Header.
        items.push(text(44.0, 70.0, 34.0, true, TEXT, "Prostir"));
        let title_w = self.text_width("Prostir", 34.0, true);
        items.push(text(
            44.0 + title_w + 12.0,
            70.0,
            18.0,
            false,
            MUTED,
            env!("CARGO_PKG_VERSION"),
        ));
        let (status, light) = match &s.host {
            _ if s.update.is_some() => (
                l.t("Потрібне оновлення", "Update needed").to_owned(),
                DANGER,
            ),
            _ if s.pairing.is_some() => (l.t("Сполучення…", "Pairing…").to_owned(), WAIT),
            Some(h) => (
                format!("{} {h}", l.t("Підключено до", "Connected to")),
                GOOD,
            ),
            None => (l.t("Шукаю ПК…", "Looking for a PC…").to_owned(), WAIT),
        };
        let sw = self.text_width(&status, 21.0, false);
        let change_w = if s.host.is_some() { 190.0 } else { 0.0 };
        let sx = w - 120.0 - change_w - sw;
        items.push(Item::Dot {
            x: sx - 16.0,
            y: 59.0,
            r: 6.0,
            color: light,
        });
        items.push(text(sx, 66.0, 21.0, false, MUTED, &status));
        if s.host.is_some() {
            items.push(button(
                rect(w - 98.0 - 170.0, 32.0, 170.0, 48.0),
                l.t("Змінити ПК", "Change PC"),
                Style::Normal,
                Some(Action::ChangeHost),
            ));
        }
        items.push(button(
            rect(w - 88.0, 28.0, 56.0, 56.0),
            "×",
            Style::Normal,
            Some(Action::Close),
        ));
        items.push(Item::Box(
            rect(32.0, 108.0, w - 64.0, 2.0),
            [255, 255, 255, 18],
        ));

        let (lx, lw) = (44.0, 580.0);
        let (rx, rw) = (680.0, 556.0);
        if let Some((host, failed)) = &s.pair_offer {
            build_pair_offer(l, &mut items, lx, lw, host, *failed);
            return items;
        }
        if let Some(p) = &s.pairing {
            self.build_pairing(l, p, &mut items, lx, lw, 150.0);
            return items;
        }
        if let Some((pc_newer, pc_app)) = &s.update {
            build_update(l, &mut items, lx, w - 2.0 * lx, *pc_newer, pc_app);
            return items;
        }
        if s.intro && s.host.is_some() {
            build_intro(l, &mut items, lx, w - 2.0 * lx);
            return items;
        }
        // No PC yet: how to get one, and the PCs found so far.
        if s.host.is_none() {
            build_welcome(l, &mut items, lx, lw, 150.0);
            self.build_connection(s, &mut items, rx, rw, 150.0);
            return items;
        }

        // Tabs: everyday things, the monitors, everything set once.
        let tabs = [
            (Tab::Quick, l.t("Швидке", "Quick")),
            (Tab::Monitors, l.t("Монітори", "Monitors")),
            (
                Tab::Settings(s.tab.section().unwrap_or_default()),
                l.t("Налаштування", "Settings"),
            ),
        ];
        let tab_w = (w - 2.0 * lx - 2.0 * 10.0) / 3.0;
        // A track the tabs sit in, so they read as one control, apart from the
        // option buttons below.
        items.push(Item::Box(
            rect(lx - 6.0, 122.0, w - 2.0 * lx + 12.0, 62.0),
            [255, 255, 255, 10],
        ));
        for (i, (tab, label)) in tabs.into_iter().enumerate() {
            let r = rect(lx + i as f32 * (tab_w + 10.0), 128.0, tab_w, 50.0);
            if s.tab.same_page(tab) {
                items.push(button(r, label, Style::TabOn, None));
            } else {
                items.push(button(r, label, Style::TabOff, Some(Action::Tab(tab))));
            }
        }

        let top = 230.0;
        match s.tab {
            Tab::Quick => build_quick(s, &mut items, lx, w - 2.0 * lx, top - 12.0),
            Tab::Monitors => {
                let left = monitor_list(s, &mut items, lx, lw, top - 12.0);
                let right = build_monitor_options(s, &mut items, rx, rw, top);
                // Under the list if there is room above the error line, else on the
                // right (with six monitors listed).
                if left + 34.0 + 70.0 < HEIGHT as f32 - 60.0 {
                    save_as_profile(s, &mut items, lx, lw, left + 34.0);
                } else {
                    save_as_profile(s, &mut items, rx, rw, right);
                }
            }
            Tab::Settings(section) => build_settings(s, section, &mut items, top - 12.0),
        }
        if let Some(e) = &s.error {
            items.push(text(lx, HEIGHT as f32 - 34.0, 21.0, true, DANGER, e));
        }
        items
    }

    /// The code to type on the PC, or a keypad for the PC's code.
    fn build_pairing(
        &mut self,
        l: Lang,
        p: &PairingView,
        items: &mut Vec<Item>,
        lx: f32,
        lw: f32,
        mut y: f32,
    ) -> f32 {
        items.push(text(
            lx,
            y,
            21.0,
            false,
            MUTED,
            &format!("{} {}", l.t("Сполучення з", "Pairing with"), p.host),
        ));
        y += 40.0;
        if p.failed {
            items.push(text(
                lx,
                y,
                20.0,
                true,
                DANGER,
                l.t(
                    "Код не підійшов — спробуйте ще раз",
                    "The code did not match — try again",
                ),
            ));
            y += 34.0;
        }
        if let Some(code) = &p.code {
            items.push(text(
                lx,
                y,
                20.0,
                false,
                TEXT,
                l.t(
                    "Введіть цей код у віконці Prostir на ноутбуці:",
                    "Type this code into the Prostir window on the PC:",
                ),
            ));
            y += 30.0;
            items.push(Item::Box(rect(lx, y, lw, 130.0), [255, 255, 255, 16]));
            let cw = self.text_width(code, 84.0, true);
            items.push(text(lx + (lw - cw) / 2.0, y + 98.0, 84.0, true, TEXT, code));
            y += 150.0;
            items.push(text(
                lx,
                y,
                18.0,
                false,
                MUTED,
                l.t(
                    "Це потрібно лише один раз для кожного ПК.",
                    "This is needed only once per PC.",
                ),
            ));
            y += 30.0;
            items.push(button(
                rect(lx, y, lw, 50.0),
                l.t(
                    "Навпаки: код на ноутбуці, ввести тут",
                    "The other way: code on the PC, type it here",
                ),
                Style::Normal,
                Some(Action::PairOtherWay),
            ));
            return y + 62.0;
        }
        for line in [
            l.t(
                "Введіть код, який показує ноутбук",
                "Type the code the PC shows",
            ),
            l.t(
                "(сповіщення або іконка Prostir у треї):",
                "(a notification or the Prostir tray icon):",
            ),
        ] {
            items.push(text(lx, y, 20.0, false, TEXT, line));
            y += 30.0;
        }
        items.push(Item::Box(rect(lx, y, lw, 64.0), [255, 255, 255, 16]));
        let shown: String = (0..6)
            .map(|i| p.typed.chars().nth(i).unwrap_or('·'))
            .flat_map(|c| [c, ' '])
            .collect();
        let sw = self.text_width(shown.trim_end(), 40.0, true);
        items.push(text(
            lx + (lw - sw) / 2.0,
            y + 47.0,
            40.0,
            true,
            TEXT,
            shown.trim_end(),
        ));
        y += 76.0;
        let ready = p.typed.len() == 6;
        let erase = l.t("Стерти", "Delete");
        let done = l.t("Готово", "Done");
        let keys = [
            "1", "2", "3", "4", "5", "6", "7", "8", "9", erase, "0", done,
        ];
        let (kw, kh) = ((lw - 20.0) / 3.0, 50.0);
        for (i, k) in keys.iter().enumerate() {
            let r = rect(
                lx + (i % 3) as f32 * (kw + 10.0),
                y + (i / 3) as f32 * (kh + 8.0),
                kw,
                kh,
            );
            let (style, action) = match i {
                9 => (Style::Normal, Some(Action::Backspace)),
                11 if ready => (Style::Selected, Some(Action::ConnectTyped)),
                11 => (Style::Disabled, None),
                _ => (Style::Normal, k.chars().next().map(Action::Key)),
            };
            items.push(button(r, k, style, action));
        }
        y += 4.0 * (kh + 8.0) + 4.0;
        items.push(button(
            rect(lx, y, lw, 50.0),
            l.t(
                "Навпаки: код у шоломі, ввести на ноутбуці",
                "The other way: code in the headset, type it on the PC",
            ),
            Style::Normal,
            Some(Action::PairOtherWay),
        ));
        y + 62.0
    }

    /// PCs found on the network and a keypad for typing an address.
    fn build_connection(
        &mut self,
        s: &UiState,
        items: &mut Vec<Item>,
        lx: f32,
        lw: f32,
        mut y: f32,
    ) -> f32 {
        let l = s.lang;
        y += 18.0;
        if let Some(typed) = &s.ip_input {
            let shown = if typed.is_empty() {
                "192.168.1.10"
            } else {
                typed.as_str()
            };
            let color = if typed.is_empty() { MUTED } else { TEXT };
            items.push(Item::Box(rect(lx, y, lw, 56.0), [255, 255, 255, 16]));
            items.push(text(lx + 20.0, y + 38.0, 26.0, true, color, shown));
            y += 68.0;
            let keys = [
                "1",
                "2",
                "3",
                "4",
                "5",
                "6",
                "7",
                "8",
                "9",
                ".",
                "0",
                l.t("Стерти", "Delete"),
            ];
            let (kw, kh) = ((lw - 20.0) / 3.0, 50.0);
            for (i, k) in keys.iter().enumerate() {
                let r = rect(
                    lx + (i % 3) as f32 * (kw + 10.0),
                    y + (i / 3) as f32 * (kh + 8.0),
                    kw,
                    kh,
                );
                let action = match k.chars().next() {
                    Some(c) if k.len() == 1 => Action::Key(c),
                    _ => Action::Backspace,
                };
                items.push(button(r, k, Style::Normal, Some(action)));
            }
            y += 4.0 * (kh + 8.0) + 4.0;
            let half = (lw - 10.0) / 2.0;
            let ready = !typed.is_empty();
            items.push(button(
                rect(lx, y, half, 50.0),
                l.t("Підключитися", "Connect"),
                if ready {
                    Style::Selected
                } else {
                    Style::Disabled
                },
                ready.then_some(Action::ConnectTyped),
            ));
            items.push(button(
                rect(lx + half + 10.0, y, half, 50.0),
                l.t("Скасувати", "Cancel"),
                Style::Normal,
                Some(Action::CancelKeypad),
            ));
            return y + 62.0;
        }
        if s.found.is_empty() {
            items.push(text(
                lx,
                y + 20.0,
                21.0,
                true,
                MUTED,
                l.t("Шукаю ПК у мережі…", "Looking for PCs on the network…"),
            ));
            y += 48.0;
        } else {
            items.push(text(
                lx,
                y + 14.0,
                21.0,
                false,
                MUTED,
                l.t("Знайдено", "Found"),
            ));
            y += 30.0;
            for (i, (name, addr)) in s.found.iter().enumerate().take(5) {
                let label = format!("{name}   {addr}");
                items.push(button(
                    rect(lx, y, lw, 52.0),
                    &label,
                    Style::Normal,
                    Some(Action::ConnectTo(i)),
                ));
                y += 60.0;
            }
            if let Some(name) = &s.unreachable {
                // Seen on the network, but the connection itself is blocked.
                let name: String = name.chars().take(30).collect();
                for line in [
                    name,
                    l.t(
                        "видно в мережі, але з'єднання блокується.",
                        "is on the network, but the connection is blocked.",
                    )
                    .to_owned(),
                    l.t(
                        "Дозвольте в брандмауері ПК порт TCP 47830.",
                        "Allow TCP port 47830 in the PC's firewall.",
                    )
                    .to_owned(),
                ] {
                    items.push(text(lx, y + 16.0, 18.0, false, DANGER, &line));
                    y += 28.0;
                }
                y += 6.0;
            }
        }
        items.push(button(
            rect(lx, y + 6.0, 280.0, 48.0),
            l.t("Ввести IP вручну", "Type an IP address"),
            Style::Normal,
            Some(Action::ShowKeypad),
        ));
        y + 66.0
    }

    fn glyph(&mut self, medium: bool, c: char, size: f32) -> &(Metrics, Vec<u8>) {
        let key = (medium, c, size.to_bits());
        let font = if medium { &self.medium } else { &self.regular };
        self.glyphs
            .entry(key)
            .or_insert_with(|| font.rasterize(c, size))
    }

    fn text_width(&mut self, s: &str, size: f32, medium: bool) -> f32 {
        s.chars()
            .map(|c| self.glyph(medium, c, size).0.advance_width)
            .sum()
    }

    fn render(&mut self) {
        let mut pixmap = std::mem::replace(&mut self.pixmap, Pixmap::new(1, 1).unwrap());
        pixmap.fill(tiny_skia::Color::TRANSPARENT);
        let items = std::mem::take(&mut self.items);
        for (i, item) in items.iter().enumerate() {
            match item {
                Item::Panel(r) => fill(&mut pixmap, rounded(*r, 36.0), [16, 18, 24, 240]),
                Item::Box(r, color) => fill(&mut pixmap, rounded(*r, 14.0), *color),
                Item::Dot { x, y, r, color } => fill(&mut pixmap, circle(*x, *y, *r), *color),
                Item::Text {
                    x,
                    baseline,
                    size,
                    medium,
                    color,
                    text,
                } => self.draw_text(&mut pixmap, *x, *baseline, *size, *medium, *color, text),
                Item::Button {
                    rect: r,
                    label,
                    style,
                    ..
                } => {
                    let hovered = self.hovered == Some(i);
                    let (bg, fg) = match (style, hovered) {
                        (Style::Selected, _) => (ACCENT, [255, 255, 255, 255]),
                        (Style::TabOn, _) => ([255, 255, 255, 34], [255, 255, 255, 255]),
                        (Style::TabOff, true) => ([255, 255, 255, 22], [255, 255, 255, 255]),
                        (Style::TabOff, false) => ([0, 0, 0, 0], MUTED),
                        (Style::Armed, _) => ([220, 70, 70, 255], [255, 255, 255, 255]),
                        // Unavailable: an outline only, so it does not look pressable.
                        (Style::Disabled, _) => ([0, 0, 0, 0], [112, 118, 132, 255]),
                        (Style::Danger, true) => ([220, 70, 70, 255], [255, 255, 255, 255]),
                        (_, true) => ([255, 255, 255, 46], [255, 255, 255, 255]),
                        _ => ([255, 255, 255, 20], TEXT),
                    };
                    fill(&mut pixmap, rounded(*r, 12.0), bg);
                    match style {
                        Style::Disabled => {
                            stroke(&mut pixmap, rounded(*r, 12.0), [255, 255, 255, 26], 1.5);
                        }
                        // The current tab: an accent bar along its bottom edge.
                        Style::TabOn => fill(
                            &mut pixmap,
                            rounded(
                                rect(r.left() + 28.0, r.bottom() - 6.0, r.width() - 56.0, 4.0),
                                2.0,
                            ),
                            ACCENT,
                        ),
                        _ => {}
                    }
                    let size = button_font_size(label);
                    let tw = self.text_width(label, size, true);
                    let baseline = r.top() + r.height() / 2.0 + size * 0.36;
                    self.draw_text(
                        &mut pixmap,
                        r.left() + (r.width() - tw) / 2.0,
                        baseline,
                        size,
                        true,
                        fg,
                        label,
                    );
                }
            }
        }
        self.items = items;
        self.pixmap = pixmap;
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        pixmap: &mut Pixmap,
        x: f32,
        baseline: f32,
        size: f32,
        medium: bool,
        color: [u8; 4],
        s: &str,
    ) {
        let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
        let mut pen = x;
        for c in s.chars() {
            let (m, coverage) = self.glyph(medium, c, size).clone();
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
            let data = pixmap.data_mut();
            for row in 0..m.height as i32 {
                let py = gy + row;
                if py < 0 || py >= ph {
                    continue;
                }
                for col in 0..m.width as i32 {
                    let px = gx + col;
                    if px < 0 || px >= pw {
                        continue;
                    }
                    let cov = coverage[(row * m.width as i32 + col) as usize] as u32;
                    let a = cov * color[3] as u32 / 255;
                    if a == 0 {
                        continue;
                    }
                    let i = ((py * pw + px) * 4) as usize;
                    for k in 0..3 {
                        let src = color[k] as u32 * a / 255;
                        data[i + k] = (src + data[i + k] as u32 * (255 - a) / 255) as u8;
                    }
                    data[i + 3] = (a + data[i + 3] as u32 * (255 - a) / 255) as u8;
                }
            }
            pen += m.advance_width;
        }
    }
}

/// Pointer dot: white with a dark rim so it shows on light and dark content.
/// Premultiplied RGBA, `size`×`size`.
pub fn cursor_image(size: u32) -> Vec<u8> {
    let mut p = Pixmap::new(size, size).expect("cursor size");
    let c = size as f32 / 2.0;
    fill(&mut p, circle(c, c, c - 1.0), [20, 22, 28, 200]);
    fill(&mut p, circle(c, c, c * 0.62), [255, 255, 255, 255]);
    p.take()
}

/// Round "☰" button that opens the menu. Premultiplied RGBA, `size`×`size`.
pub fn menu_button_image(size: u32, hovered: bool) -> Vec<u8> {
    let mut p = Pixmap::new(size, size).expect("button size");
    let s = size as f32;
    let bg = if hovered { ACCENT } else { [16, 18, 24, 225] };
    fill(&mut p, circle(s / 2.0, s / 2.0, s / 2.0 - 1.0), bg);
    for i in -1..=1 {
        let y = s / 2.0 + i as f32 * s * 0.15;
        fill(
            &mut p,
            rounded(rect(s * 0.3, y - s * 0.035, s * 0.4, s * 0.07), s * 0.035),
            [255, 255, 255, 255],
        );
    }
    p.take()
}

/// Quest monitors with reorder / primary / remove, then the laptop screen.
fn monitor_list(s: &UiState, items: &mut Vec<Item>, lx: f32, lw: f32, mut y: f32) -> f32 {
    let l = s.lang;
    let removable = s.monitors.len() > 1;
    let count = s.monitors.len();
    for (i, m) in s.monitors.iter().enumerate() {
        items.push(Item::Box(rect(lx, y, lw, 62.0), [255, 255, 255, 12]));
        // Move left / right in the row (also reorders KDE's layout).
        for (k, (label, delta)) in [("‹", -1), ("›", 1)].into_iter().enumerate() {
            let enabled = if delta < 0 { i > 0 } else { i + 1 < count };
            items.push(button(
                rect(lx + 8.0 + k as f32 * 48.0, y + 7.0, 44.0, 48.0),
                label,
                if enabled {
                    Style::Normal
                } else {
                    Style::Disabled
                },
                enabled.then_some(Action::Move { id: m.id, delta }),
            ));
        }
        items.push(text(
            lx + 116.0,
            y + 29.0,
            22.0,
            true,
            TEXT,
            &format!("{} {}", l.t("Монітор", "Monitor"), m.id + 1),
        ));
        items.push(text(
            lx + 116.0,
            y + 52.0,
            16.0,
            false,
            MUTED,
            &format!("{} × {}", m.width, m.height),
        ));
        let remove = rect(lx + lw - 58.0, y + 7.0, 48.0, 48.0);
        let primary = rect(lx + lw - 58.0 - 10.0 - 210.0, y + 7.0, 210.0, 48.0);
        if s.confirm_remove == Some(m.id) {
            let both = rect(
                primary.left(),
                y + 7.0,
                remove.right() - primary.left(),
                48.0,
            );
            items.push(button(
                both,
                l.t("Прибрати? Ще раз", "Remove? Press again"),
                Style::Armed,
                Some(Action::Remove(m.id)),
            ));
            y += 70.0;
            continue;
        }
        if s.primary == Some(m.id) {
            items.push(button(
                primary,
                l.t("Головний", "Primary"),
                Style::Selected,
                None,
            ));
        } else {
            items.push(button(
                primary,
                l.t("Зробити головним", "Make primary"),
                Style::Normal,
                Some(Action::MakePrimary(m.id)),
            ));
        }
        let style = if removable {
            Style::Danger
        } else {
            Style::Disabled
        };
        items.push(button(
            remove,
            "×",
            style,
            removable.then_some(Action::Remove(m.id)),
        ));
        y += 70.0;
    }
    // Physical display mirror: shown or hidden, said by the highlighted option.
    let title = if s.mirror_wanted && !s.mirror_active {
        format!(
            "{} · {}",
            l.t("Екран ноутбука", "Laptop screen"),
            l.t("кришка закрита", "lid closed")
        )
    } else {
        l.t("Екран ноутбука", "Laptop screen").to_owned()
    };
    y += 34.0;
    segmented(
        items,
        lx,
        lw,
        &mut y,
        &title,
        vec![
            (
                l.t("Показано", "Shown"),
                Action::SetMirror(true),
                s.mirror_wanted,
                true,
            ),
            (
                l.t("Приховано", "Hidden"),
                Action::SetMirror(false),
                !s.mirror_wanted,
                true,
            ),
        ],
    );
    y -= 16.0;
    items.push(button(
        rect(lx, y, lw, 52.0),
        l.t(
            "Поставити все перед собою",
            "Bring everything in front of me",
        ),
        Style::Normal,
        Some(Action::Recenter),
    ));
    y + 52.0
}

/// A row with a title, a muted note and one button on the right.
fn hints(l: Lang, items: &mut Vec<Item>, lx: f32, mut y: f32) {
    for line in [
        l.t(
            "Пальцем: торкніться кнопки або тягніть меню за порожнє місце",
            "Finger: touch a button, or drag the menu by an empty spot",
        ),
        l.t(
            "Здалеку рукою: щипок натискає, щипок з утриманням переносить",
            "Hand from afar: pinch to press, pinch and hold to move",
        ),
        l.t(
            "Контролер: курок натискає, grip переносить",
            "Controller: trigger presses, grip moves",
        ),
        l.t(
            "Стік, поки тримаєте: відстань і розмір. A: вирівняти",
            "Stick while holding: distance and size. A: line up",
        ),
    ] {
        items.push(text(lx, y, 19.0, false, MUTED, line));
        y += 30.0;
    }
}

/// A titled group of buttons where one option is selected:
/// `(label, action, selected, enabled)`. An empty title continues the previous group.
fn segmented(
    items: &mut Vec<Item>,
    x: f32,
    w: f32,
    y: &mut f32,
    title: &str,
    options: Vec<(&str, Action, bool, bool)>,
) {
    if !title.is_empty() {
        items.push(text(x, *y, 21.0, true, SECTION, title));
        *y += 18.0;
    } else {
        *y -= 30.0;
    }
    let n = options.len() as f32;
    let bw = (w - 10.0 * (n - 1.0)) / n;
    for (i, (label, action, selected, enabled)) in options.into_iter().enumerate() {
        let r = rect(x + i as f32 * (bw + 10.0), *y, bw, 48.0);
        let style = match (enabled, selected) {
            (false, _) => Style::Disabled,
            (true, true) => Style::Selected,
            (true, false) => Style::Normal,
        };
        items.push(button(
            r,
            label,
            style,
            (enabled && !selected).then_some(action),
        ));
    }
    *y += 48.0 + 40.0;
}

/// Add a monitor, layout, curve.
fn build_monitor_options(s: &UiState, items: &mut Vec<Item>, rx: f32, rw: f32, mut y: f32) -> f32 {
    let l = s.lang;
    items.push(text(
        rx,
        y,
        21.0,
        false,
        MUTED,
        l.t("Додати монітор", "Add a monitor"),
    ));
    y += 18.0;
    let full = s.monitors.len() >= MAX_MONITORS || s.host.is_none();
    let wide = format!("3440 × 1440 {}", l.t("широкий", "wide"));
    let tall = format!("1440 × 2560 {}", l.t("портрет", "portrait"));
    let sizes = [
        (1920, 1080, "1920 × 1080"),
        (2560, 1440, "2560 × 1440"),
        (3840, 2160, "3840 × 2160"),
        (3440, 1440, wide.as_str()),
        (1440, 2560, tall.as_str()),
    ];
    let bw = (rw - 10.0) / 2.0;
    for (i, (width, height, label)) in sizes.iter().enumerate() {
        let r = rect(
            rx + (i % 2) as f32 * (bw + 10.0),
            y + (i / 2) as f32 * 58.0,
            bw,
            48.0,
        );
        let (style, action) = if full {
            (Style::Disabled, None)
        } else {
            (
                Style::Normal,
                Some(Action::Add {
                    width: *width,
                    height: *height,
                }),
            )
        };
        items.push(button(r, label, style, action));
    }
    y += 3.0 * 58.0 + 40.0;
    if s.monitors.len() >= MAX_MONITORS {
        items.push(text(
            rx,
            y - 22.0,
            18.0,
            false,
            MUTED,
            &format!(
                "{} {MAX_MONITORS} {}",
                l.t("Максимум", "At most"),
                l.t("моніторів", "monitors")
            ),
        ));
        y += 10.0;
    }

    let st = s.settings;
    segmented(
        items,
        rx,
        rw,
        &mut y,
        l.t("Розкладка", "Layout"),
        vec![
            (
                l.t("Навколо мене", "Around me"),
                Action::Preset(Preset::Arc),
                st.preset == Preset::Arc,
                true,
            ),
            (
                l.t("Стіна", "Wall"),
                Action::Preset(Preset::Wall),
                st.preset == Preset::Wall,
                true,
            ),
            (
                l.t("Лежачи", "Lying down"),
                Action::Preset(Preset::Overhead),
                st.preset == Preset::Overhead,
                true,
            ),
        ],
    );
    segmented(
        items,
        rx,
        rw,
        &mut y,
        l.t("Вигин", "Curve"),
        vec![
            (
                l.t("Пласкі", "Flat"),
                Action::Curve(Curve::Flat),
                st.curve == Curve::Flat,
                true,
            ),
            (
                l.t("Легкий", "Gentle"),
                Action::Curve(Curve::Gentle),
                st.curve == Curve::Gentle,
                s.curve_available,
            ),
            (
                l.t("Сильний", "Strong"),
                Action::Curve(Curve::Strong),
                st.curve == Curve::Strong,
                s.curve_available,
            ),
        ],
    );
    segmented(
        items,
        rx,
        rw,
        &mut y,
        l.t("Режим", "Mode"),
        vec![
            (
                l.t("Робочий стіл", "Desk"),
                Action::Cinema(false),
                !st.cinema,
                true,
            ),
            (
                l.t("Кінотеатр", "Cinema"),
                Action::Cinema(true),
                st.cinema,
                true,
            ),
        ],
    );
    segmented(
        items,
        rx,
        rw,
        &mut y,
        l.t("Слідом за головою", "Follow my head"),
        vec![
            (l.t("Ні", "Off"), Action::Follow(false), !st.follow, true),
            (l.t("Так", "On"), Action::Follow(true), st.follow, true),
        ],
    );
    // The cinema screen can be picked before switching to cinema.
    if s.monitors.len() > 1 {
        let sizes: Vec<(u8, u32, u32)> = s
            .monitors
            .iter()
            .map(|m| (m.id, m.width, m.height))
            .collect();
        let shown = crate::layout::cinema_choice(&sizes, st.cinema_screen);
        let short = s.monitors.len() > 3;
        let labels: Vec<String> = s
            .monitors
            .iter()
            .map(|m| {
                if short {
                    format!("{}", m.id + 1)
                } else {
                    format!("{} {}", l.t("Монітор", "Monitor"), m.id + 1)
                }
            })
            .collect();
        segmented(
            items,
            rx,
            rw,
            &mut y,
            l.t("Екран для кінотеатру", "Cinema screen"),
            s.monitors
                .iter()
                .zip(&labels)
                .enumerate()
                .map(|(i, (m, label))| {
                    (
                        label.as_str(),
                        Action::CinemaScreen(i as u8),
                        shown == Some(m.id),
                        true,
                    )
                })
                .collect(),
        );
    }
    y
}

/// Background and hands on the left, focus mode on the right.
fn build_look(s: &UiState, items: &mut Vec<Item>, lx: f32, lw: f32, rx: f32, rw: f32, top: f32) {
    let l = s.lang;
    let st = s.settings;
    let mut y = top;
    let bg = |b: Background| (Action::Background(b), st.background == b);
    let (a1, s1) = bg(Background::Black);
    let (a2, s2) = bg(Background::Room);
    let (a3, s3) = bg(Background::Custom);
    segmented(
        items,
        lx,
        lw,
        &mut y,
        l.t("Фон", "Background"),
        vec![
            (l.t("Чорний", "Black"), a1, s1, true),
            (l.t("Кімната", "Room"), a2, s2, s.room_available),
            (
                l.t("Своє фото", "Own photo"),
                a3,
                s3,
                s.panorama_available && s.custom_available,
            ),
        ],
    );
    // Where an own photo comes from, while there is none yet.
    if s.panorama_available && !s.custom_available {
        for (k, line) in [
            l.t(
                "Своє фото: панорама 360° у файлі",
                "Own photo: a 360° picture saved as",
            ),
            "Android/data/dev.prostir.client/files/background.jpg",
        ]
        .into_iter()
        .enumerate()
        {
            items.push(text(
                lx,
                y - 22.0 + 24.0 * k as f32,
                17.0,
                false,
                MUTED,
                line,
            ));
        }
        y += 38.0;
    }
    let skies = [
        (l.t("Ніч", "Night"), Sky::Night),
        (l.t("Захід", "Sunset"), Sky::Sunset),
        (l.t("Космос", "Space"), Sky::Space),
        (l.t("Сяйво", "Aurora"), Sky::Aurora),
    ];
    segmented(
        items,
        lx,
        lw,
        &mut y,
        "",
        skies
            .iter()
            .map(|(label, sky)| {
                let (a, sel) = bg(Background::Sky(*sky));
                (*label, a, sel, s.panorama_available)
            })
            .collect(),
    );
    segmented(
        items,
        lx,
        lw,
        &mut y,
        l.t("Руки", "Hands"),
        vec![
            (
                l.t("Сховати", "Hide"),
                Action::Hands(HandMode::Hidden),
                st.hands == HandMode::Hidden,
                true,
            ),
            (
                l.t("Показувати", "Show"),
                Action::Hands(HandMode::Visible),
                st.hands == HandMode::Visible,
                true,
            ),
            (
                l.t("І керувати", "Show and use"),
                Action::Hands(HandMode::Control),
                st.hands == HandMode::Control,
                true,
            ),
        ],
    );

    // Side by side, or (one column) under the background and hands.
    let mut y = if rx == lx { y - 10.0 } else { top };
    segmented(
        items,
        rx,
        rw,
        &mut y,
        l.t("Фокус", "Focus"),
        vec![
            (
                l.t("Вимкнено", "Off"),
                Action::Focus(false),
                !st.focus,
                true,
            ),
            (
                l.t("Затемнювати інші", "Dim the others"),
                Action::Focus(true),
                st.focus,
                true,
            ),
        ],
    );
    y -= 12.0;
    for line in [
        l.t(
            "Монітори, на яких немає миші,",
            "Monitors without the mouse",
        ),
        l.t("стають темнішими.", "get darker."),
    ] {
        items.push(text(rx, y, 19.0, false, MUTED, line));
        y += 28.0;
    }
    y += 40.0;
    segmented(
        items,
        rx,
        rw,
        &mut y,
        l.t("Вікно на клавіатуру", "Keyboard window"),
        vec![
            (
                l.t("Вимкнено", "Off"),
                Action::KeyboardWindow(false),
                !st.keyboard_window,
                s.keyboard_available,
            ),
            (
                l.t("Показувати", "Show"),
                Action::KeyboardWindow(true),
                st.keyboard_window,
                s.keyboard_available,
            ),
        ],
    );
    y -= 12.0;
    for line in [
        l.t(
            "Прямокутник, крізь який видно справжній стіл.",
            "A rectangle you see the real desk through.",
        ),
        l.t(
            "Перенесіть його на клавіатуру, як монітор.",
            "Move it onto the keyboard like a monitor.",
        ),
        l.t(
            "Поки він увімкнений, камери шолома працюють.",
            "The headset cameras run while it is on.",
        ),
    ] {
        items.push(text(rx, y, 19.0, false, MUTED, line));
        y += 28.0;
    }
}

/// A PC this headset has not paired with: pair only if the user says so.
fn build_pair_offer(l: Lang, items: &mut Vec<Item>, x: f32, w: f32, host: &str, failed: bool) {
    let mut y = 180.0;
    let title = if failed {
        l.t(
            "Код не підійшов кілька разів",
            "The code did not match several times",
        )
    } else {
        l.t("Новий ПК", "A new PC")
    };
    items.push(text(x, y, 34.0, true, TEXT, title));
    y += 60.0;
    let host: String = host.chars().take(40).collect();
    items.push(text(x, y, 24.0, false, TEXT, &host));
    y += 44.0;
    for line in [
        l.t(
            "Сполучайте шолом лише зі своїм ПК.",
            "Pair the headset only with your own PC.",
        ),
        l.t(
            "Потрібно ввести код з одного пристрою на іншому.",
            "You will type a code from one device into the other.",
        ),
    ] {
        items.push(text(x, y, 20.0, false, MUTED, line));
        y += 32.0;
    }
    y += 20.0;
    let half = (w - 12.0) / 2.0;
    let pair = if failed {
        l.t("Спробувати ще", "Try again")
    } else {
        l.t("Сполучити", "Pair")
    };
    items.push(button(
        rect(x, y, half, 56.0),
        pair,
        Style::Selected,
        Some(Action::PairAnswer(true)),
    ));
    items.push(button(
        rect(x + half + 12.0, y, half, 56.0),
        l.t("Не зараз", "Not now"),
        Style::Normal,
        Some(Action::PairAnswer(false)),
    ));
}

/// The headset and the PC cannot talk: which one to update, and where.
fn build_update(l: Lang, items: &mut Vec<Item>, x: f32, w: f32, pc_newer: bool, pc_app: &str) {
    let ours = env!("CARGO_PKG_VERSION");
    let mut y = 180.0;
    let title = if pc_newer {
        l.t(
            "Оновіть застосунок у шоломі",
            "Update the app in the headset",
        )
    } else {
        l.t("Оновіть Prostir на ПК", "Update Prostir on the PC")
    };
    items.push(text(x, y, 34.0, true, TEXT, title));
    y += 60.0;
    let versions = format!(
        "{} {pc_app}, {} {ours}.",
        l.t("Версія на ПК:", "The PC has"),
        l.t("у шоломі:", "the headset has")
    );
    items.push(text(x, y, 22.0, false, MUTED, &versions));
    y += 36.0;
    items.push(text(
        x,
        y,
        22.0,
        false,
        MUTED,
        l.t(
            "Різні версії не можуть з'єднатися.",
            "Different versions cannot connect.",
        ),
    ));
    y += 50.0;
    items.push(Item::Box(rect(x, y, w, 130.0), [255, 255, 255, 12]));
    let (how, where_) = if pc_newer && cfg!(feature = "store") {
        (
            l.t(
                "Оновіть Prostir у бібліотеці шолома:",
                "Update Prostir in the headset's library:",
            ),
            "Meta Horizon Store",
        )
    } else if pc_newer {
        (
            l.t("Встановіть новий APK з", "Install the new APK from"),
            "github.com/OlexiyOdarchuk/prostir/releases",
        )
    } else {
        (
            l.t(
                "Завантажте нову версію для ПК з",
                "Download the new PC version from",
            ),
            "prostir.ishawyha.dev",
        )
    };
    items.push(text(x + 28.0, y + 50.0, 22.0, false, TEXT, how));
    items.push(text(x + 28.0, y + 100.0, 32.0, true, ACCENT, where_));
    y += 170.0;
    items.push(text(
        x,
        y,
        19.0,
        false,
        MUTED,
        l.t(
            "Шолом сам підключиться, щойно версії зійдуться.",
            "The headset connects by itself once the versions match.",
        ),
    ));
}

/// The desktops the PC part supports, as the headset names them (Ukrainian, English).
/// Add new ones here.
pub const DESKTOPS: (&str, &str) = ("KDE Plasma 6 або Hyprland", "KDE Plasma 6 or Hyprland");

/// Before any PC is found: what Prostir needs and where to get the PC part.
fn build_welcome(l: Lang, items: &mut Vec<Item>, x: f32, w: f32, mut y: f32) {
    items.push(text(
        x,
        y + 6.0,
        26.0,
        true,
        TEXT,
        l.t(
            "Prostir показує монітори вашого ПК",
            "Prostir shows your PC's monitors",
        ),
    ));
    y += 36.0;
    items.push(text(
        x,
        y,
        19.0,
        false,
        MUTED,
        &format!(
            "{} {}.",
            l.t("Потрібен ПК з", "It needs a PC with"),
            l.t(DESKTOPS.0, DESKTOPS.1),
        ),
    ));
    y += 30.0;
    let steps: [(&str, &str, &str, &str, bool); 3] = [
        (
            "Встановіть Prostir на ПК",
            "Install Prostir on the PC",
            "prostir.ishawyha.dev",
            "prostir.ishawyha.dev",
            true,
        ),
        (
            "Запустіть його",
            "Start it",
            "У треї з'явиться іконка Prostir.",
            "The Prostir icon appears in the tray.",
            false,
        ),
        // USB needs developer mode and adb: not something a store install has.
        if cfg!(feature = "store") {
            (
                "Шолом знайде ПК сам",
                "The headset finds the PC",
                "У тій самій мережі Wi‑Fi.",
                "On the same Wi‑Fi network.",
                false,
            )
        } else {
            (
                "Шолом знайде ПК сам",
                "The headset finds the PC",
                "У тій самій мережі Wi‑Fi або через кабель USB.",
                "On the same Wi‑Fi network, or over a USB cable.",
                false,
            )
        },
    ];
    for (n, (uk_title, en_title, uk, en, link)) in steps.into_iter().enumerate() {
        y += 16.0;
        let h = if link { 116.0 } else { 96.0 };
        items.push(Item::Box(rect(x, y, w, h), [255, 255, 255, 12]));
        items.push(text(
            x + 22.0,
            y + 40.0,
            28.0,
            true,
            ACCENT,
            &format!("{}", n + 1),
        ));
        items.push(text(
            x + 64.0,
            y + 38.0,
            23.0,
            true,
            TEXT,
            l.t(uk_title, en_title),
        ));
        let (size, color, medium) = if link {
            (30.0, ACCENT, true)
        } else {
            (19.0, MUTED, false)
        };
        items.push(text(
            x + 64.0,
            y + if link { 88.0 } else { 72.0 },
            size,
            medium,
            color,
            l.t(uk, en),
        ));
        y += h;
    }
}

/// The first-run page: how to reach the menu, move and line up monitors, and what
/// happens when the headset comes off.
fn build_intro(l: Lang, items: &mut Vec<Item>, x: f32, w: f32) {
    let mut y = 160.0;
    items.push(text(
        x,
        y,
        28.0,
        true,
        TEXT,
        l.t(
            "Монітори вже тут. Кілька порад:",
            "Your monitors are here. A few tips:",
        ),
    ));
    y += 34.0;
    let tips: [(&str, &str, &str, &str); 5] = [
        (
            "Меню",
            "Menu",
            "Кругла кнопка під головним монітором або кнопка меню чи X на лівому контролері.",
            "The round button under the main monitor, or the menu or X button on the left controller.",
        ),
        (
            "Перенести монітор",
            "Move a monitor",
            "Тримайте grip і наводьте. Руками: щипок і тримати пів секунди.",
            "Hold grip and point. With hands: pinch and hold for half a second.",
        ),
        (
            "Вирівняти",
            "Line up",
            "Кнопка A ставить монітори рівно. «Поставити все перед собою» в меню розверне їх до вас.",
            "The A button lines monitors up. \"Bring everything in front of me\" in the menu turns them to you.",
        ),
        (
            "Пальцем",
            "With a finger",
            "Коли руки в полі зору, меню можна просто торкатися, як планшет.",
            "With hands in view, just touch the menu like a tablet.",
        ),
        (
            "Зняли шолом",
            "Headset off",
            "Через 10 с вікна, звук і мікрофон повертаються на ноутбук.",
            "After 10 s, windows, sound and microphone go back to the laptop.",
        ),
    ];
    for (uk_title, en_title, uk, en) in tips {
        y += 28.0;
        items.push(Item::Box(rect(x, y, w, 96.0), [255, 255, 255, 12]));
        items.push(text(
            x + 24.0,
            y + 38.0,
            23.0,
            true,
            TEXT,
            l.t(uk_title, en_title),
        ));
        items.push(text(x + 24.0, y + 74.0, 20.0, false, MUTED, l.t(uk, en)));
        y += 96.0;
    }
    y += 36.0;
    items.push(button(
        rect(x, y, 280.0, 56.0),
        l.t("Зрозуміло", "Got it"),
        Style::Selected,
        Some(Action::IntroDone),
    ));
}

/// A big square button with a title and its state underneath: what is on is lit.
#[allow(clippy::too_many_arguments)]
fn tile(
    items: &mut Vec<Item>,
    r: Rect,
    title: &str,
    state: &str,
    on: bool,
    action: Option<Action>,
) {
    let style = match (&action, on) {
        (None, _) => Style::Disabled,
        (_, true) => Style::Selected,
        _ => Style::Normal,
    };
    items.push(button(r, "", style, action));
    let (fg, sub) = if on {
        ([255, 255, 255, 255], [225, 235, 255, 255])
    } else {
        (TEXT, MUTED)
    };
    items.push(text(r.left() + 26.0, r.top() + 62.0, 27.0, true, fg, title));
    items.push(text(
        r.left() + 24.0,
        r.top() + 104.0,
        19.0,
        false,
        sub,
        state,
    ));
}

/// Everyday things in one place: profiles to switch to, and toggles as tiles.
fn build_quick(s: &UiState, items: &mut Vec<Item>, x: f32, w: f32, mut y: f32) {
    let l = s.lang;
    let st = s.settings;
    let connected = s.host.is_some();
    items.push(text(
        x,
        y + 14.0,
        21.0,
        true,
        SECTION,
        l.t("Профіль", "Profile"),
    ));
    y += 32.0;
    let names = [
        l.t("Робота", "Work"),
        l.t("Кіно", "Cinema"),
        l.t("Лежачи", "Lying down"),
    ];
    let cw = (w - 2.0 * 12.0) / 3.0;
    for (i, name) in names.iter().enumerate() {
        let r = rect(x + i as f32 * (cw + 12.0), y, cw, 72.0);
        let active = s.active_profile == Some(i);
        let style = match (connected, active) {
            (false, _) => Style::Disabled,
            (true, true) => Style::Selected,
            (true, false) => Style::Normal,
        };
        let action = (connected && !active).then_some(Action::ApplyProfile(i));
        items.push(button(r, name, style, action));
    }
    // The tiles sit low, where a hand reaches without lifting the arm.
    y += 72.0 + 60.0;
    let on_off = |on: bool| {
        if on {
            l.t("Увімкнено", "On")
        } else {
            l.t("Вимкнено", "Off")
        }
    };
    let tw = (w - 2.0 * 12.0) / 3.0;
    let th = 170.0;
    let tiles: [(&str, String, bool, Option<Action>); 6] = [
        (
            l.t("Перед собою", "In front of me"),
            l.t("Повернути все до себе", "Turn everything to me")
                .to_owned(),
            false,
            Some(Action::Recenter),
        ),
        (
            l.t("Фокус", "Focus"),
            on_off(st.focus).to_owned(),
            st.focus,
            Some(Action::Focus(!st.focus)),
        ),
        (
            l.t("Кінотеатр", "Cinema"),
            on_off(st.cinema).to_owned(),
            st.cinema,
            Some(Action::Cinema(!st.cinema)),
        ),
        (
            l.t("Вікно на клавіатуру", "Keyboard window"),
            on_off(st.keyboard_window).to_owned(),
            st.keyboard_window,
            s.keyboard_available
                .then_some(Action::KeyboardWindow(!st.keyboard_window)),
        ),
        (
            l.t("Звук ПК", "PC sound"),
            if s.audio_wanted {
                l.t("У шоломі", "In the headset")
            } else {
                l.t("На ПК", "On the PC")
            }
            .to_owned(),
            s.audio_wanted,
            connected.then_some(Action::SetAudio(!s.audio_wanted)),
        ),
        (
            l.t("Мікрофон", "Microphone"),
            if s.mic_wanted {
                l.t("Мікрофон шолома", "The headset's")
            } else {
                l.t("Мікрофон ПК", "The PC's")
            }
            .to_owned(),
            s.mic_wanted,
            connected.then_some(Action::SetMic(!s.mic_wanted)),
        ),
    ];
    for (i, (title, state, on, action)) in tiles.into_iter().enumerate() {
        let r = rect(
            x + (i % 3) as f32 * (tw + 12.0),
            y + (i / 3) as f32 * (th + 12.0),
            tw,
            th,
        );
        tile(items, r, title, &state, on, action);
    }
}

/// Saves the current setup into one of the profiles (second press confirms).
fn save_as_profile(s: &UiState, items: &mut Vec<Item>, x: f32, w: f32, mut y: f32) {
    let l = s.lang;
    items.push(text(
        x,
        y,
        21.0,
        true,
        SECTION,
        l.t("Зберегти як профіль", "Save as profile"),
    ));
    y += 18.0;
    let names = [
        l.t("Робота", "Work"),
        l.t("Кіно", "Cinema"),
        l.t("Лежачи", "Lying down"),
    ];
    let bw = (w - 2.0 * 10.0) / 3.0;
    for (i, name) in names.iter().enumerate() {
        let armed = s.confirm_save == Some(i);
        let label = if armed {
            l.t("Замінити?", "Replace?")
        } else {
            name
        };
        let style = match (s.host.is_some(), armed) {
            (false, _) => Style::Disabled,
            (true, true) => Style::Armed,
            (true, false) => Style::Normal,
        };
        items.push(button(
            rect(x + i as f32 * (bw + 10.0), y, bw, 48.0),
            label,
            style,
            s.host.is_some().then_some(Action::SaveProfile(i)),
        ));
    }
}

/// Settings: the parts listed on the left, the chosen one on the right.
fn build_settings(s: &UiState, section: Section, items: &mut Vec<Item>, top: f32) {
    let l = s.lang;
    let (nx, nw) = (44.0, 230.0);
    let sections = [
        (Section::Look, l.t("Вигляд", "Look")),
        (Section::Sound, l.t("Звук", "Sound")),
        (Section::Language, l.t("Мова", "Language")),
        (Section::Pc, l.t("ПК", "PC")),
        (Section::Help, l.t("Довідка", "Help")),
    ];
    for (i, (sec, label)) in sections.into_iter().enumerate() {
        let r = rect(nx, top + i as f32 * 60.0, nw, 52.0);
        if sec == section {
            items.push(button(r, label, Style::TabOn, None));
        } else {
            items.push(button(
                r,
                label,
                Style::TabOff,
                Some(Action::Tab(Tab::Settings(sec))),
            ));
        }
    }
    // A thin line between the list and the content.
    items.push(Item::Box(
        rect(nx + nw + 20.0, top, 2.0, 640.0),
        [255, 255, 255, 14],
    ));
    let (cx, cw) = (nx + nw + 50.0, WIDTH as f32 - (nx + nw + 50.0) - 44.0);
    let ctop = top + 12.0;
    match section {
        Section::Look => {
            let col = cw.min(640.0);
            build_look(s, items, cx, col, cx, col, ctop);
        }
        Section::Sound => build_sound(s, items, cx, cw.min(620.0), ctop - 12.0),
        Section::Language => {
            let mut y = ctop;
            segmented(
                items,
                cx,
                cw.min(620.0),
                &mut y,
                l.t("Мова меню · Language", "Menu language · Мова"),
                vec![
                    (
                        l.t("Як на ПК", "Same as PC"),
                        Action::Language(Language::LikePc),
                        s.settings.language == Language::LikePc,
                        true,
                    ),
                    (
                        "Українська",
                        Action::Language(Language::Fixed(Lang::Uk)),
                        s.settings.language == Language::Fixed(Lang::Uk),
                        true,
                    ),
                    (
                        "English",
                        Action::Language(Language::Fixed(Lang::En)),
                        s.settings.language == Language::Fixed(Lang::En),
                        true,
                    ),
                ],
            );
        }
        Section::Pc => build_pc(s, items, cx, cw.min(700.0), ctop),
        Section::Help => {
            hints(l, items, cx, ctop + 10.0);
            items.push(button(
                rect(cx, ctop + 170.0, 300.0, 50.0),
                l.t("Показати підказки знову", "Show the tips again"),
                Style::Normal,
                Some(Action::ShowTips),
            ));
            items.push(text(
                cx,
                ctop + 270.0,
                19.0,
                false,
                MUTED,
                &format!(
                    "Prostir {} · prostir.ishawyha.dev",
                    env!("CARGO_PKG_VERSION")
                ),
            ));
        }
    }
}

/// How the PC encodes: quality, codec, graphics card or CPU, text preset.
fn build_pc(s: &UiState, items: &mut Vec<Item>, x: f32, w: f32, mut y: f32) {
    let l = s.lang;
    let Some((pc, encoders)) = s.pc else {
        items.push(text(
            x,
            y + 20.0,
            21.0,
            false,
            MUTED,
            l.t(
                "Доступно, коли ПК підключено.",
                "Available while a PC is connected.",
            ),
        ));
        return;
    };
    use prostir_proto::encoders::{CPU_H264, GPU_H264, GPU_H265};
    let set = |change: &dyn Fn(&mut PcSettings)| {
        let mut p = pc;
        change(&mut p);
        Action::SetPc(p)
    };
    segmented(
        items,
        x,
        w,
        &mut y,
        l.t("Якість відео", "Video quality"),
        [
            (15, l.t("Економна", "Economy")),
            (30, l.t("Звичайна", "Normal")),
            (50, l.t("Висока", "High")),
        ]
        .into_iter()
        .map(|(q, label)| (label, set(&|p| p.quality = q), pc.quality == q, true))
        .collect(),
    );
    let gpu = !pc.software;
    segmented(
        items,
        x,
        w,
        &mut y,
        l.t("Кодує", "Encoding on"),
        vec![
            (
                l.t("Відеокарта", "Graphics card"),
                set(&|p| p.software = false),
                gpu,
                encoders & (GPU_H264 | GPU_H265) != 0,
            ),
            (
                l.t("Процесор", "Processor"),
                set(&|p| {
                    p.software = true;
                    p.codec = prostir_proto::Codec::H264;
                }),
                !gpu,
                encoders & CPU_H264 != 0,
            ),
        ],
    );
    segmented(
        items,
        x,
        w,
        &mut y,
        l.t("Кодек", "Codec"),
        vec![
            (
                "HEVC",
                set(&|p| p.codec = prostir_proto::Codec::H265),
                pc.codec == prostir_proto::Codec::H265,
                gpu && encoders & GPU_H265 != 0,
            ),
            (
                "H.264",
                set(&|p| p.codec = prostir_proto::Codec::H264),
                pc.codec == prostir_proto::Codec::H264,
                encoders & (GPU_H264 | CPU_H264) != 0,
            ),
        ],
    );
    items.push(text(
        x,
        y - 18.0,
        18.0,
        false,
        MUTED,
        l.t(
            "HEVC: найчіткіший текст. H.264: менше пам'яті на ПК.",
            "HEVC: the sharpest text. H.264: less memory on the PC.",
        ),
    ));
    y += 22.0;
    segmented(
        items,
        x,
        w,
        &mut y,
        l.t("Чіткіший текст (+2 мс)", "Sharper text (+2 ms)"),
        vec![
            (
                l.t("Вимкнено", "Off"),
                set(&|p| p.sharp = false),
                !pc.sharp,
                gpu,
            ),
            (
                l.t("Увімкнено", "On"),
                set(&|p| p.sharp = true),
                pc.sharp,
                gpu,
            ),
        ],
    );
}

/// PC sound and headset microphone.
fn build_sound(s: &UiState, items: &mut Vec<Item>, lx: f32, lw: f32, mut y: f32) {
    let l = s.lang;
    // The current state is the highlighted option, not a button label to decode.
    y += 18.0;
    segmented(
        items,
        lx,
        lw,
        &mut y,
        l.t("Звук ПК грає", "PC sound plays"),
        vec![
            (
                l.t("У шоломі", "In the headset"),
                Action::SetAudio(true),
                s.audio_wanted,
                true,
            ),
            (
                l.t("На ПК", "On the PC"),
                Action::SetAudio(false),
                !s.audio_wanted,
                true,
            ),
        ],
    );
    segmented(
        items,
        lx,
        lw,
        &mut y,
        l.t("Програми на ПК чують", "PC apps hear"),
        vec![
            (
                l.t("Мікрофон шолома", "The headset's mic"),
                Action::SetMic(true),
                s.mic_wanted,
                true,
            ),
            (
                l.t("Мікрофон ПК", "The PC's mic"),
                Action::SetMic(false),
                !s.mic_wanted,
                true,
            ),
        ],
    );
    y -= 24.0;
    if s.mic_denied {
        for line in [
            l.t(
                "Prostir не має доступу до мікрофона. Дозвольте його",
                "Prostir may not use the microphone. Allow it in the",
            ),
            l.t(
                "в налаштуваннях шолома (дозволи застосунків).",
                "headset settings (app permissions).",
            ),
        ] {
            items.push(text(lx, y, 19.0, false, DANGER, line));
            y += 30.0;
        }
        items.push(button(
            rect(lx, y - 8.0, 260.0, 48.0),
            l.t("Спитати ще раз", "Ask again"),
            Style::Normal,
            Some(Action::AskMic),
        ));
        y += 52.0;
    }
    y += 30.0;
    for line in [
        l.t(
            "Поки шолом підключено, звук із ПК грає в шоломі,",
            "While the headset is connected, PC sound plays in it",
        ),
        l.t(
            "а програми на ПК чують мікрофон шолома.",
            "and PC apps hear the headset microphone.",
        ),
        l.t(
            "Мікрофон вмикається, лише коли програма записує.",
            "The microphone turns on only while an app records.",
        ),
        l.t(
            "Коли шолом знято, звук і мікрофон повертаються на ПК.",
            "Take the headset off and both return to the PC.",
        ),
    ] {
        items.push(text(lx, y, 19.0, false, MUTED, line));
        y += 30.0;
    }
}

fn button_font_size(label: &str) -> f32 {
    match label {
        "×" => 32.0,
        "‹" | "›" => 34.0,
        l if l.len() == 1 => 26.0,
        _ => 19.0,
    }
}

fn overlap(a: &Rect, b: &Rect) -> bool {
    a.left() < b.right() && b.left() < a.right() && a.top() < b.bottom() && b.top() < a.bottom()
}

fn circle(cx: f32, cy: f32, r: f32) -> Path {
    PathBuilder::from_circle(cx, cy, r).expect("valid circle")
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::from_xywh(x, y, w, h).expect("valid rect")
}

fn text(x: f32, baseline: f32, size: f32, medium: bool, color: [u8; 4], s: &str) -> Item {
    Item::Text {
        x,
        baseline,
        size,
        medium,
        color,
        text: s.to_owned(),
    }
}

fn button(rect: Rect, label: &str, style: Style, action: Option<Action>) -> Item {
    Item::Button {
        rect,
        label: label.to_owned(),
        style,
        action,
    }
}

fn rounded(r: Rect, radius: f32) -> Path {
    let rad = radius.min(r.width() / 2.0).min(r.height() / 2.0);
    let k = rad * 0.552_284_8;
    let (l, t, rt, b) = (r.left(), r.top(), r.right(), r.bottom());
    let mut pb = PathBuilder::new();
    pb.move_to(l + rad, t);
    pb.line_to(rt - rad, t);
    pb.cubic_to(rt - rad + k, t, rt, t + rad - k, rt, t + rad);
    pb.line_to(rt, b - rad);
    pb.cubic_to(rt, b - rad + k, rt - rad + k, b, rt - rad, b);
    pb.line_to(l + rad, b);
    pb.cubic_to(l + rad - k, b, l, b - rad + k, l, b - rad);
    pb.line_to(l, t + rad);
    pb.cubic_to(l, t + rad - k, l + rad - k, t, l + rad, t);
    pb.close();
    pb.finish().expect("valid path")
}

fn stroke(pixmap: &mut Pixmap, path: Path, [r, g, b, a]: [u8; 4], width: f32) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    let stroke = Stroke {
        width,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn fill(pixmap: &mut Pixmap, path: Path, [r, g, b, a]: [u8; 4]) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample_state() -> UiState {
        UiState {
            pairing: None,
            unreachable: Some("workstation-with-a-long-name".into()),
            pair_offer: None,
            mic_denied: true,
            pc: Some((
                PcSettings {
                    quality: 30,
                    codec: prostir_proto::Codec::H265,
                    software: false,
                    sharp: true,
                },
                7,
            )),
            host: Some("workstation".into()),
            found: Vec::new(),
            ip_input: None,
            monitors: vec![
                MonitorEntry {
                    id: 0,
                    width: 2560,
                    height: 1440,
                },
                MonitorEntry {
                    id: 1,
                    width: 2560,
                    height: 1440,
                },
                MonitorEntry {
                    id: 2,
                    width: 1440,
                    height: 2560,
                },
            ],
            primary: Some(1),
            mirror_wanted: true,
            audio_wanted: true,
            mic_wanted: true,
            mirror_active: true,
            settings: Settings::default(),
            room_available: true,
            panorama_available: true,
            custom_available: false,
            curve_available: true,
            keyboard_available: true,
            error: None,
            confirm_remove: None,
            confirm_save: None,
            tab: Tab::Monitors,
            lang: Lang::Uk,
            intro: false,
            update: None,
            profiles: (0..crate::profiles::SLOTS)
                .map(Profile::default_for)
                .collect(),
            active_profile: None,
        }
    }

    #[test]
    fn hover_and_click() {
        let mut p = Panel::new();
        p.set_state(&sample_state());
        assert!(p.render_if_dirty().is_some());
        assert!(p.render_if_dirty().is_none());
        // Close button in the top-right corner.
        p.hover(Some((WIDTH as f32 - 60.0, 56.0)));
        assert_eq!(p.hovered_action(), Some(Action::Close));
        assert!(p.render_if_dirty().is_some());
        p.hover(None);
        assert_eq!(p.hovered_action(), None);
    }

    /// Every page and state, in both languages, without text that does not fit.
    #[test]
    fn nothing_overflows() {
        let mut states = Vec::new();
        for lang in [Lang::Uk, Lang::En] {
            for tab in [
                Tab::Quick,
                Tab::Monitors,
                Tab::Settings(Section::Look),
                Tab::Settings(Section::Sound),
                Tab::Settings(Section::Language),
                Tab::Settings(Section::Pc),
                Tab::Settings(Section::Help),
            ] {
                let mut st = sample_state();
                st.lang = lang;
                st.tab = tab;
                states.push(st.clone());
                st.confirm_remove = Some(2);
                st.settings.cinema = true;
                st.settings.focus = true;
                st.mic_wanted = false;
                st.audio_wanted = false;
                st.mirror_wanted = false;
                st.active_profile = Some(1);
                st.error = Some(format!(
                    "{} 6",
                    lang.t("Максимум моніторів:", "At most monitors:")
                ));
                states.push(st.clone());
                // Six monitors.
                st.monitors = (0..6)
                    .map(|id| MonitorEntry {
                        id,
                        width: 3440,
                        height: 1440,
                    })
                    .collect();
                states.push(st);
            }
            for failed in [false, true] {
                let mut offer = sample_state();
                offer.lang = lang;
                offer.host = None;
                offer.pair_offer = Some((
                    "a-very-long-pc-name.example.lan 192.168.100.200".into(),
                    failed,
                ));
                states.push(offer);
            }
            for pc_newer in [true, false] {
                let mut update = sample_state();
                update.lang = lang;
                update.host = None;
                update.update = Some((pc_newer, "0.12.3".into()));
                states.push(update);
            }
            let mut intro = sample_state();
            intro.lang = lang;
            intro.intro = true;
            states.push(intro);
            let mut offline = sample_state();
            offline.lang = lang;
            offline.host = None;
            offline.found = vec![("workstation".into(), "192.168.1.10".into())];
            states.push(offline.clone());
            offline.ip_input = Some("192.168.100.200".into());
            states.push(offline);
            for (code, typed, failed) in [
                (Some("482 913".to_owned()), String::new(), false),
                (None, "4829".to_owned(), true),
            ] {
                let mut pairing = sample_state();
                pairing.lang = lang;
                pairing.pairing = Some(PairingView {
                    host: "workstation".into(),
                    code,
                    typed,
                    failed,
                });
                states.push(pairing);
            }
        }
        let mut problems = Vec::new();
        for st in &states {
            let mut p = Panel::new();
            p.set_state(st);
            for o in p.overflows() {
                problems.push(format!("{:?}/{:?}: {o}", st.lang, st.tab));
            }
        }
        problems.dedup();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
